//! Running and recent child runs, shared by the tool, the widget, and `/agents runs`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pi_ext_sdk::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::stream::Progress;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Status::Running => "\u{25d0}",
            Status::Completed => "\u{2713}",
            Status::Failed => "\u{2717}",
            Status::Cancelled => "\u{2298}",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub background: bool,
    pub status: Status,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub progress: Progress,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub model: Option<String>,
    pub cancel: CancellationToken,
    /// The child's pi session file, `<run dir>/session.jsonl`.
    pub session_file: PathBuf,
    /// A runner process runs the child, so the run outlives this process.
    pub detached: bool,
}

impl Run {
    pub fn new(id: &str, agent: &str, task: &str, background: bool) -> Run {
        Run {
            id: id.to_owned(),
            agent: agent.to_owned(),
            task: task.to_owned(),
            background,
            status: Status::Running,
            started: Instant::now(),
            finished: None,
            progress: Progress::default(),
            stderr: String::new(),
            exit_code: None,
            model: None,
            cancel: CancellationToken::new(),
            session_file: PathBuf::new(),
            detached: false,
        }
    }

    pub fn dir(&self) -> &Path {
        self.session_file.parent().unwrap_or(Path::new(""))
    }

    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    pub fn label(&self) -> String {
        format!("{}: {}", self.agent, first_line(&self.task, 60))
    }

    /// The text the parent gets: the child's answer, or the failure reason.
    pub fn result_text(&self) -> String {
        let output = self.progress.output();
        match self.status {
            Status::Completed if output.is_empty() => "(no output)".into(),
            Status::Completed => output,
            Status::Cancelled => join_nonempty("Cancelled.", &output),
            Status::Failed => {
                let reason = self.progress.error.clone().unwrap_or_else(|| {
                    let tail = tail_lines(&self.stderr, 8);
                    match self.exit_code {
                        Some(code) => format!("pi exited with code {code}. {tail}"),
                        None => format!("pi stopped. {tail}"),
                    }
                });
                join_nonempty(&format!("Failed: {}", reason.trim()), &output)
            }
            Status::Running => output,
        }
    }
}

#[derive(Clone, Default)]
pub struct Runs {
    inner: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    runs: Vec<Run>,
    next: u64,
}

impl Runs {
    /// A new run id: `sa`, the clock in hex, and a counter of this process. Another process can
    /// make the same id; `store::claim_run_dir` makes it unique.
    pub fn next_id(&self) -> String {
        let mut state = self.inner.lock().expect("runs lock");
        state.next += 1;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        format!("sa{:x}{}", millis % 0xff_ffff, state.next)
    }

    /// Adds `run`; `false` when a run with its id is already there.
    pub fn insert(&self, run: Run) -> bool {
        let mut state = self.inner.lock().expect("runs lock");
        let known = state.runs.iter().any(|known| known.id == run.id);
        if !known {
            state.runs.push(run);
        }
        !known
    }

    pub fn update<R>(&self, id: &str, change: impl FnOnce(&mut Run) -> R) -> Option<R> {
        let mut state = self.inner.lock().expect("runs lock");
        state.runs.iter_mut().find(|run| run.id == id).map(change)
    }

    pub fn get(&self, id: &str) -> Option<Run> {
        self.update(id, |run| run.clone())
    }

    pub fn all(&self) -> Vec<Run> {
        self.inner.lock().expect("runs lock").runs.clone()
    }

    pub fn finish(&self, id: &str, status: Status, keep: usize) {
        let mut state = self.inner.lock().expect("runs lock");
        if let Some(run) = state.runs.iter_mut().find(|run| run.id == id) {
            run.status = status;
            run.finished = Some(Instant::now());
        }
        let finished = state
            .runs
            .iter()
            .filter(|r| r.status != Status::Running)
            .count();
        let mut excess = finished.saturating_sub(keep);
        state.runs.retain(|run| {
            let drop = excess > 0 && run.status != Status::Running;
            excess -= usize::from(drop);
            !drop
        });
    }

    /// Cancels a running run; `false` when it is not running.
    pub fn cancel(&self, id: &str) -> bool {
        self.update(id, |run| {
            let running = run.status == Status::Running;
            run.cancel.cancel();
            running
        })
        .unwrap_or(false)
    }

    /// Whether a run of this process runs; detached runs do not count.
    pub fn any_in_process_running(&self) -> bool {
        let state = self.inner.lock().expect("runs lock");
        let mut in_process = state.runs.iter().filter(|run| !run.detached);
        in_process.any(|run| run.status == Status::Running)
    }

    /// Cancels the runs of this process. Detached runs go on in their runner.
    pub fn cancel_in_process(&self) {
        let state = self.inner.lock().expect("runs lock");
        for run in state.runs.iter().filter(|run| !run.detached) {
            run.cancel.cancel();
        }
    }
}

pub fn first_line(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or("");
    if line.chars().count() > max {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    } else {
        line.to_owned()
    }
}

pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn join_nonempty(head: &str, body: &str) -> String {
    if body.trim().is_empty() {
        head.to_owned()
    } else {
        format!("{head}\n\n{body}")
    }
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
