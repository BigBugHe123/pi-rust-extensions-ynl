//! Runs one child pi process and streams its events into the run registry.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use pi_ext_sdk::CancellationToken;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::call::{EVENTS_FILE, STDERR_FILE};
use crate::command::ChildSpec;
use crate::runs::{Runs, Status};
use crate::stream::Effect;

const STDERR_CAP: usize = 16 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(1);
/// How often a runner reads new lines of a child's `events.jsonl`.
const TAIL_EVERY: Duration = Duration::from_millis(100);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long shutdown waits for cancelled runs: above `KILL_GRACE`, below the SDK's 1.5 s limit.
pub const SHUTDOWN_WAIT: Duration = Duration::from_millis(1400);

pub async fn execute(
    spec: &ChildSpec,
    runs: &Runs,
    id: &str,
    cancel: &CancellationToken,
    on_effect: &mut (dyn FnMut(Effect) + Send),
) -> Status {
    runs.update(id, |run| run.model = spec.model.clone());
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("cannot start {}: {err}", spec.program.display());
            runs.update(id, |run| run.progress.error = Some(message));
            return Status::Failed;
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr_task = child.stderr.take().map(|stderr| {
        let runs = runs.clone();
        let id = id.to_owned();
        tokio::spawn(async move { collect_stderr(stderr, &runs, &id).await })
    });
    let mut lines = BufReader::new(stdout).lines();
    let cancelled = loop {
        tokio::select! {
            _ = cancel.cancelled() => break true,
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let effect = runs.update(id, |run| run.progress.apply_line(&line));
                    if let Some(effect) = effect.filter(|e| *e != Effect::None) {
                        on_effect(effect);
                    }
                }
                _ => break false,
            },
        }
    };
    if cancelled {
        stop(&mut child).await;
        if let Some(task) = stderr_task {
            task.abort();
        }
        return Status::Cancelled;
    }
    let exit = tokio::select! {
        _ = cancel.cancelled() => {
            stop(&mut child).await;
            return Status::Cancelled;
        }
        exit = child.wait() => exit,
    };
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    }
    exit_status(runs, id, exit)
}

/// Runs the child of a detached call: stdout goes to `<dir>/events.jsonl` and stderr to
/// `<dir>/stderr.log`, files that do not depend on the runner, and the events fold into the run
/// as the file grows. The child is not killed when the runner drops it.
pub async fn execute_to_files(
    spec: &ChildSpec,
    runs: &Runs,
    id: &str,
    cancel: &CancellationToken,
    dir: &Path,
    on_effect: &mut (dyn FnMut(Effect) + Send),
) -> Status {
    runs.update(id, |run| run.model = spec.model.clone());
    let (events, stderr) = (dir.join(EVENTS_FILE), dir.join(STDERR_FILE));
    let files = std::fs::create_dir_all(dir).and_then(|()| {
        Ok((
            std::fs::File::create(&events)?,
            std::fs::File::create(&stderr)?,
        ))
    });
    let (stdout_file, stderr_file) = match files {
        Ok(files) => files,
        Err(err) => {
            let message = format!("cannot write {}: {err}", dir.display());
            runs.update(id, |run| run.progress.error = Some(message));
            return Status::Failed;
        }
    };
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("cannot start {}: {err}", spec.program.display());
            runs.update(id, |run| run.progress.error = Some(message));
            return Status::Failed;
        }
    };
    let mut tail = Tail::default();
    let exit = loop {
        tail.read(&events, &mut |line| fold(runs, id, line, on_effect));
        tokio::select! {
            _ = cancel.cancelled() => break None,
            exit = child.wait() => break Some(exit),
            _ = tokio::time::sleep(TAIL_EVERY) => {}
        }
    };
    if exit.is_none() {
        stop(&mut child).await;
    }
    tail.read(&events, &mut |line| fold(runs, id, line, on_effect));
    tail.finish(&mut |line| fold(runs, id, line, on_effect));
    let stderr_tail = read_tail(&stderr, STDERR_CAP);
    runs.update(id, |run| run.stderr = stderr_tail);
    match exit {
        None => Status::Cancelled,
        Some(exit) => exit_status(runs, id, exit),
    }
}

fn fold(runs: &Runs, id: &str, line: &str, on_effect: &mut (dyn FnMut(Effect) + Send)) {
    let effect = runs.update(id, |run| run.progress.apply_line(line));
    if let Some(effect) = effect.filter(|e| *e != Effect::None) {
        on_effect(effect);
    }
}

/// The new complete lines of a growing file, from where the last read stopped.
#[derive(Default)]
struct Tail {
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    fn read(&mut self, path: &Path, on_line: &mut dyn FnMut(&str)) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut bytes).is_err()
        {
            return;
        }
        self.offset += bytes.len() as u64;
        self.partial.extend_from_slice(&bytes);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            on_line(&String::from_utf8_lossy(&line));
        }
    }

    /// The last line, when the child ended without a newline.
    fn finish(&mut self, on_line: &mut dyn FnMut(&str)) {
        if !self.partial.is_empty() {
            on_line(&String::from_utf8_lossy(&std::mem::take(&mut self.partial)));
        }
    }
}

/// The last `cap` bytes of a file, from a character boundary.
fn read_tail(path: &Path, cap: usize) -> String {
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(cap as u64)))?;
        file.read_to_end(&mut bytes)
    });
    if read.is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    text.trim_start_matches('\u{fffd}').to_owned()
}

/// `Completed` for exit code 0 without a model error, else `Failed`.
fn exit_status(runs: &Runs, id: &str, exit: std::io::Result<std::process::ExitStatus>) -> Status {
    let code = exit.ok().and_then(|status| status.code());
    let failed = runs
        .update(id, |run| {
            run.exit_code = code;
            run.progress.error.is_some()
        })
        .unwrap_or(true);
    if failed || code != Some(0) {
        Status::Failed
    } else {
        Status::Completed
    }
}

async fn collect_stderr(stderr: impl AsyncReadExt + Unpin, runs: &Runs, id: &str) {
    let mut reader = BufReader::new(stderr);
    let mut buf = [0u8; 4096];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
        runs.update(id, |run| {
            run.stderr.push_str(&chunk);
            if run.stderr.len() > STDERR_CAP {
                let mut cut = run.stderr.len() - STDERR_CAP;
                while !run.stderr.is_char_boundary(cut) {
                    cut += 1;
                }
                run.stderr.drain(..cut);
            }
        });
    }
}

/// SIGTERM to the child's process group, then SIGKILL after `KILL_GRACE` (inside the SDK cancel
/// window). Windows has no SIGTERM: `taskkill /T /F` ends the child's process tree at once.
async fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = group_signal_command("-TERM", pid).status();
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = group_signal_command("-KILL", pid).status();
        }
        let _ = child.kill().await;
    }
}

#[cfg(unix)]
fn group_signal_command(signal: &str, pid: u32) -> std::process::Command {
    let mut command = std::process::Command::new("kill");
    command
        .args([signal, "--", &format!("-{pid}")])
        .stderr(Stdio::null());
    command
}

#[cfg(all(test, unix))]
mod tests {
    use super::group_signal_command;

    #[test]
    fn process_group_signals_terminate_option_parsing() {
        for signal in ["-TERM", "-KILL"] {
            for pid in [42, 1_439_999, 2_147_483_000, i32::MAX as u32] {
                let command = group_signal_command(signal, pid);
                assert_eq!(command.get_program(), "kill");
                let args: Vec<_> = command.get_args().collect();
                assert_eq!(args, [signal, "--", &format!("-{pid}")]);
            }
        }
    }
}
