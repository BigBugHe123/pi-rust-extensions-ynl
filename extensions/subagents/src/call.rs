//! The files of a detached `async` call in its `asyncDir` (the first run's directory): the call
//! that the extension writes, the state that the runner writes, stop commands, and the markers
//! that let one process send the call's `subagent-notify`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::ChildSpec;
use crate::registry::write_atomic;
use crate::runs::{Run, Status};
use crate::stream::Progress;

pub const VERSION: u32 = 1;
pub const CALL_FILE: &str = "call.json";
/// The runner's state while it runs, rewritten at least every [`HEARTBEAT`].
pub const STATE_FILE: &str = "run.json";
/// The runner's final state; the runner writes it last.
pub const RESULT_FILE: &str = "result.json";
pub const COMMANDS_DIR: &str = "commands";
pub const CLAIM_FILE: &str = "notify.claim";
pub const DONE_FILE: &str = "notify.done";
/// The child's stdout (its `--mode json` events) and stderr, in each run directory.
pub const EVENTS_FILE: &str = "events.jsonl";
pub const STDERR_FILE: &str = "stderr.log";
/// The runner's own stderr.
pub const LOG_FILE: &str = "runner.log";
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// A claim that its holder did not refresh for this long is free.
pub const CLAIM_STALE: Duration = Duration::from_secs(10);

/// `call.json`: the runs of the call and the session that gets the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    pub version: u32,
    pub session_id: String,
    pub session_file: Option<String>,
    /// `single` or `parallel`.
    pub mode: String,
    pub created_ms: u64,
    pub max_parallel: usize,
    pub runs: Vec<CallRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRun {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub session_file: PathBuf,
    pub spec: ChildSpec,
}

/// `run.json` and `result.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Report {
    /// The runner's process id.
    pub pid: u32,
    pub heartbeat_ms: u64,
    pub runs: Vec<RunReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub id: String,
    pub status: Status,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub model: Option<String>,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub progress: Progress,
}

impl Call {
    pub fn async_id(&self) -> &str {
        &self.runs[0].id
    }

    /// The runs with their state in `report`; a run that `report` lacks is running.
    pub fn runs(&self, report: Option<&Report>) -> Vec<Run> {
        let state = |id: &str| report.and_then(|r| r.runs.iter().find(|run| run.id == id));
        self.runs
            .iter()
            .map(|run| run.run(self.created_ms, state(&run.id)))
            .collect()
    }
}

impl CallRun {
    pub fn run(&self, created_ms: u64, report: Option<&RunReport>) -> Run {
        let mut run = Run {
            started: instant_at(created_ms),
            session_file: self.session_file.clone(),
            detached: true,
            ..Run::new(&self.id, &self.agent, &self.task, true)
        };
        if let Some(report) = report {
            run.status = report.status;
            run.finished = report.finished_ms.map(instant_at);
            run.exit_code = report.exit_code;
            run.model = report.model.clone();
            run.stderr = report.stderr.clone();
            run.progress = report.progress.clone();
        }
        run
    }
}

impl RunReport {
    pub fn of(run: &Run) -> RunReport {
        RunReport {
            id: run.id.clone(),
            status: run.status,
            finished_ms: run.finished.map(ms_at),
            exit_code: run.exit_code,
            model: run.model.clone(),
            stderr: run.stderr.clone(),
            progress: run.progress.clone(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The `Instant` of the wall-clock time `ms`, for runs that another process started.
pub fn instant_at(ms: u64) -> Instant {
    let ago = Duration::from_millis(now_ms().saturating_sub(ms));
    Instant::now().checked_sub(ago).unwrap_or_else(Instant::now)
}

pub fn ms_at(instant: Instant) -> u64 {
    now_ms().saturating_sub(instant.elapsed().as_millis() as u64)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

pub fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let text = serde_json::to_string(value).map_err(std::io::Error::other)?;
    write_atomic(path, &text)
}

/// The call in `dir`; `None` without a readable `call.json` of this version.
pub fn read_call(dir: &Path) -> Option<Call> {
    let call: Call = read_json(&dir.join(CALL_FILE))?;
    (call.version == VERSION && !call.runs.is_empty()).then_some(call)
}

/// `result.json` with `true`, else `run.json` with `false`.
pub fn read_report(dir: &Path) -> Option<(Report, bool)> {
    if let Some(report) = read_json(&dir.join(RESULT_FILE)) {
        return Some((report, true));
    }
    read_json(&dir.join(STATE_FILE)).map(|report| (report, false))
}

fn stop_file(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(COMMANDS_DIR).join(format!("stop-{run_id}"))
}

/// Asks the runner to stop the run; only an explicit cancel of the user writes this.
pub fn request_stop(dir: &Path, run_id: &str) -> std::io::Result<()> {
    write_atomic(&stop_file(dir, run_id), "")
}

pub fn stop_requested(dir: &Path, run_id: &str) -> bool {
    stop_file(dir, run_id).exists()
}

pub fn delivered(dir: &Path) -> bool {
    dir.join(DONE_FILE).exists()
}

/// Writes `notify.done`, which ends the delivery for every process, and drops the claim.
pub fn mark_delivered(dir: &Path) -> std::io::Result<()> {
    write_atomic(&dir.join(DONE_FILE), &now_ms().to_string())?;
    let _ = std::fs::remove_file(dir.join(CLAIM_FILE));
    Ok(())
}

/// The run ids of an undelivered detached call in `dir`; empty for any other directory. It
/// reads `call.json` loosely, so a call of another version stays too.
pub fn undelivered_runs(dir: &Path) -> Vec<String> {
    if delivered(dir) {
        return Vec::new();
    }
    let Some(call) = read_json::<Value>(&dir.join(CALL_FILE)) else {
        return Vec::new();
    };
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut ids: Vec<String> = name.into_iter().collect();
    let runs = call["runs"].as_array().into_iter().flatten();
    for id in runs.filter_map(|run| run["id"].as_str()) {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// `notify.claim`: the process that sends the notify, and when it sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub pid: u32,
    /// The pi that this extension process runs in.
    pub pi_pid: u32,
    pub sent_ms: Option<u64>,
}

impl Claim {
    pub fn mine() -> Claim {
        Claim {
            pid: std::process::id(),
            pi_pid: pi_pid(),
            sent_ms: None,
        }
    }
}

/// The pi process of this extension; it stays the same across `/reload`.
fn pi_pid() -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::process::parent_id()
    }
    #[cfg(not(unix))]
    {
        std::process::id()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claimed {
    /// This process made the claim.
    Fresh,
    /// Another process holds it.
    Busy,
    /// This process took over a stale claim; holds the stale one when it could read it.
    TakenOver(Option<Claim>),
}

/// Takes `notify.claim` for `me`: a new file, or one whose holder did not refresh it for
/// [`CLAIM_STALE`].
pub fn claim(dir: &Path, me: &Claim) -> Claimed {
    let path = dir.join(CLAIM_FILE);
    if create_claim(&path, me).is_ok() {
        return Claimed::Fresh;
    }
    let age = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    if age.is_none_or(|age| age < CLAIM_STALE) {
        return Claimed::Busy;
    }
    let stale: Option<Claim> = read_json(&path);
    let aside = dir.join(format!("{CLAIM_FILE}.{}.stale", std::process::id()));
    if std::fs::rename(&path, &aside).is_err() {
        return Claimed::Busy;
    }
    let _ = std::fs::remove_file(&aside);
    match create_claim(&path, me) {
        Ok(()) => Claimed::TakenOver(stale),
        Err(_) => Claimed::Busy,
    }
}

fn create_claim(path: &Path, me: &Claim) -> std::io::Result<()> {
    let text = serde_json::to_string(me).map_err(std::io::Error::other)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(text.as_bytes())
}

/// Rewrites the claim, which keeps it fresh; `false` when another process took it.
pub fn refresh_claim(dir: &Path, me: &Claim) -> bool {
    let path = dir.join(CLAIM_FILE);
    let holder: Option<Claim> = read_json(&path);
    if holder.is_none_or(|holder| holder.pid != me.pid) {
        return false;
    }
    write_json(&path, me).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("subagents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_claim_at_a_time_until_it_is_stale() {
        let dir = temp_dir("claim");
        let first = Claim {
            pid: 1,
            pi_pid: 10,
            sent_ms: Some(5),
        };
        let second = Claim {
            pid: 2,
            pi_pid: 20,
            sent_ms: None,
        };
        assert_eq!(claim(&dir, &first), Claimed::Fresh);
        assert_eq!(claim(&dir, &second), Claimed::Busy);
        assert!(refresh_claim(&dir, &first));
        assert!(!refresh_claim(&dir, &second));
        let old = SystemTime::now() - CLAIM_STALE - Duration::from_secs(1);
        let file = OpenOptions::new()
            .write(true)
            .open(dir.join(CLAIM_FILE))
            .unwrap();
        file.set_modified(old).unwrap();
        assert_eq!(
            claim(&dir, &second),
            Claimed::TakenOver(Some(first.clone()))
        );
        assert!(!refresh_claim(&dir, &first));
        assert!(refresh_claim(&dir, &second));
        mark_delivered(&dir).unwrap();
        assert!(delivered(&dir) && !dir.join(CLAIM_FILE).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn undelivered_calls_name_their_runs() {
        let dir = temp_dir("undelivered").join("sa1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(undelivered_runs(&dir).is_empty());
        let call = serde_json::json!({ "version": 99, "runs": [{ "id": "sa1" }, { "id": "sa2" }] });
        std::fs::write(dir.join(CALL_FILE), call.to_string()).unwrap();
        assert_eq!(undelivered_runs(&dir), ["sa1", "sa2"]);
        mark_delivered(&dir).unwrap();
        assert!(undelivered_runs(&dir).is_empty());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
