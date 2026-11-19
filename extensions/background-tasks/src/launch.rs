//! The launcher of a task. `pi-rust-ext background-tasks --launch <task dir> <cwd> <command>`
//! starts the task's wrapper shell and exits at once, so the wrapper's parent is not the
//! extension process: a kill of pi's process tree (Paseo stops pi that way) does not reach it.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::identity;
use crate::process;
use crate::task::{EXIT_FILE, OUTPUT_FILE};

pub const FLAG: &str = "--launch";
/// How long the extension waits for the launcher's answer.
const ANSWER_LIMIT: Duration = Duration::from_secs(30);
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The launcher's answer: the wrapper's pid, and its start time, read while the wrapper was
/// still the launcher's child, so the pid cannot belong to another process yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launched {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The launcher did not start the task, so the extension may start it itself.
    NotStarted(String),
    /// The launcher did not answer, so the task may run; a second start could run it twice.
    Unknown(String),
}

/// `--launch <task dir> <cwd> <command>`: starts the wrapper, prints one JSON line (the
/// [`Launched`] answer, or `{"error": ...}`), and exits without waiting for the task.
pub fn main(args: &[OsString]) -> ExitCode {
    let result = match args {
        [dir, cwd, command] => match command.to_str() {
            Some(command) => start_wrapper(Path::new(dir), Path::new(cwd), command)
                .map_err(|err| format!("cannot start `{command}`: {err}")),
            None => Err("the command is not valid UTF-8".to_owned()),
        },
        _ => Err(format!(
            "usage: pi-rust-ext {} {FLAG} <task dir> <cwd> <command>",
            crate::NAME
        )),
    };
    let (line, code) = match &result {
        Ok(launched) => (
            serde_json::to_string(launched).unwrap_or_default(),
            ExitCode::SUCCESS,
        ),
        Err(err) => (
            serde_json::json!({ "error": err }).to_string(),
            ExitCode::FAILURE,
        ),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}").and_then(|()| stdout.flush());
    code
}

/// Starts the wrapper of `command` in `cwd` with its files in `dir`, and leaves it running.
fn start_wrapper(dir: &Path, cwd: &Path, command: &str) -> std::io::Result<Launched> {
    let output = dir.join(OUTPUT_FILE);
    let mut wrapper = process::wrapper_command(command, cwd, &output, &dir.join(EXIT_FILE))?;
    let pid = spawn_detached(&mut wrapper)?.id();
    let process_start = identity::identity(pid).ok().flatten();
    Ok(Launched { pid, process_start })
}

/// The wrapper already leads a process group of its own (`process::wrapper_command`).
#[cfg(unix)]
fn spawn_detached(wrapper: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    wrapper.spawn()
}

/// In a process group of its own, out of pi's job when the job allows it, and with a hidden
/// console that the task's console programs share. (`DETACHED_PROCESS` would leave `cmd`
/// without a console, so each console program of the task would open a window.)
#[cfg(windows)]
fn spawn_detached(wrapper: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    wrapper
        .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .or_else(|_| wrapper.creation_flags(flags).spawn())
}

/// Starts the wrapper of `command` through `launcher`, the binary of this extension, and
/// returns its answer.
pub async fn start(
    launcher: &Path,
    dir: &Path,
    cwd: &Path,
    command: &str,
) -> Result<Launched, Failure> {
    let mut cmd = tokio::process::Command::new(launcher);
    cmd.arg(crate::NAME)
        .arg(FLAG)
        .arg(dir)
        .arg(cwd)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .map_err(|err| Failure::NotStarted(format!("cannot run {}: {err}", launcher.display())))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Failure::NotStarted("the launcher has no stdout".into()))?;
    // One line, not up to the end: on Windows the task inherits the launcher's handles, so the
    // pipe stays open while the task runs.
    let mut line = String::new();
    let mut stdout = BufReader::new(stdout);
    match tokio::time::timeout(ANSWER_LIMIT, stdout.read_line(&mut line)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            return Err(Failure::Unknown(format!(
                "cannot read the launcher's answer: {err}"
            )));
        }
        Err(_) => {
            return Err(Failure::Unknown(format!(
                "the launcher did not answer within {} seconds",
                ANSWER_LIMIT.as_secs()
            )));
        }
    }
    let status = tokio::time::timeout(ANSWER_LIMIT, child.wait()).await;
    let status = match status {
        Ok(Ok(status)) => status.to_string(),
        _ => "an unknown status".to_owned(),
    };
    parse_answer(&line, &status)
}

/// The launcher's answer line; `status` describes its exit for the error text.
pub fn parse_answer(line: &str, status: &str) -> Result<Launched, Failure> {
    let line = line.trim();
    if line.is_empty() {
        return Err(Failure::NotStarted(format!(
            "the launcher exited with {status} without an answer"
        )));
    }
    if let Ok(launched) = serde_json::from_str::<Launched>(line) {
        return Ok(launched);
    }
    let value: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    Err(Failure::NotStarted(match value["error"].as_str() {
        Some(error) => error.to_owned(),
        None => format!("unexpected answer from the launcher ({status}): {line}"),
    }))
}
