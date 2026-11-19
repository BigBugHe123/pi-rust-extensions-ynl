//! Detached `async` calls in the extension: the launch, and the tracker that adopts the calls of
//! the current session from disk. The tracker keeps the run table and the bus events current,
//! turns a cancel into a stop command, and sends each call's `subagent-notify` once across pi
//! restarts.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pi_ext_sdk::prelude::*;

use crate::actions::{NOTIFY_TYPE, notify_message, wake_data};
use crate::app::App;
use crate::call::{self, Call, CallRun, Claim, Claimed, Report, RunReport};
use crate::command::{self, ChildInput};
use crate::jobs::{
    Job, Launch, START_CHANNEL, STOP_CHANNEL, TODO_SCOPE_CHANNEL, scope_payload, start_payload,
    stop_payload,
};
use crate::messaging::WAKE_CHANNEL;
use crate::runner::DETACH_FLAG;
use crate::runs::{Run, Status};
use crate::store;

const TICK: Duration = Duration::from_millis(500);
/// A runner whose heartbeat is older than this, without `result.json`, has stopped.
const STALE_MS: u64 = 30_000;
/// How long a heartbeat must stay stale while this process ticks without a pause, so that after
/// a sleep of the machine the runner has time to write a new one.
const STALE_CONFIRM: Duration = Duration::from_secs(5);
/// A call is adopted once its runner wrote `run.json`, or this long after the launch.
const ADOPT_AFTER_MS: u64 = 10_000;
/// A run directory without `call.json` this old never gets one.
const PLAIN_AFTER: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// pi saves a message it accepted before it is idle again, so a sent message that is not in the
/// session after pi was idle this long was lost.
const LOST_AFTER: Duration = Duration::from_secs(10);
const RUNNER_STOPPED: &str = "the background runner stopped before the run finished";

/// Starts `jobs` as one detached call: run directories, the first `status.json`, `call.json`,
/// then `pi-rust-ext subagents --detach`. `None` when the session has no id, the extension does
/// not run from the `pi-rust-ext` binary, or the runner does not start; the caller then runs the
/// jobs in this process.
pub async fn launch(
    app: &App,
    ctx: &ToolCtx,
    launch: &Launch,
    jobs: &[Job],
    mode: &str,
) -> Option<Vec<String>> {
    let exe = app.runner.clone()?;
    let snapshot = ctx.snapshot();
    let session_id = snapshot.session_id.clone().filter(|id| !id.is_empty())?;
    let host = ctx.host();
    let mut runs: Vec<CallRun> = Vec::new();
    for job in jobs {
        let id = match store::claim_run_dir(&launch.runs_root, || app.runs.next_id()) {
            Ok(id) => id,
            Err(err) => {
                pi_ext_sdk::warn!("cannot make a run directory: {err}");
                discard(&launch.runs_root, &runs);
                return None;
            }
        };
        let session_file = store::session_file(&launch.runs_root, &id);
        let spec = command::build(ChildInput {
            agent: &job.agent,
            task: &job.task,
            run_id: &id,
            session_file: &session_file,
            parent: &launch.parent,
            overrides: &job.overrides,
            program: &launch.program,
            cwd: &job.cwd,
            extra_args: &launch.extra_args,
            parent_address: launch.parent_address.as_deref(),
        });
        let (agent, task) = (job.agent.name.clone(), job.task.clone());
        runs.push(CallRun {
            id,
            agent,
            task,
            session_file,
            spec,
        });
    }
    let call = Call {
        version: call::VERSION,
        session_id,
        session_file: snapshot.session_file.clone(),
        mode: mode.to_owned(),
        created_ms: call::now_ms(),
        max_parallel: app.config(host).max_parallel,
        runs,
    };
    let dir = launch.runs_root.join(call.async_id());
    let table = call.runs(None);
    store::write_status(&dir, &table);
    if let Err(err) = start_runner(&exe, &dir, &call).await {
        pi_ext_sdk::warn!("cannot start the subagents runner, so the runs stay in pi: {err}");
        discard(&launch.runs_root, &call.runs);
        return None;
    }
    let mut events = Vec::new();
    for run in table {
        events.push((START_CHANNEL, start_payload(&run.id, &run.agent)));
        let scope = scope_payload(&run.id, &run.label(), "running", Value::Null);
        events.push((TODO_SCOPE_CHANNEL, scope));
        app.runs.insert(run);
    }
    let (app, host) = (app.clone(), host.clone());
    tokio::spawn(async move {
        crate::active_work::publish(&app, &host, false).await;
        for (channel, data) in events {
            emit(&host, channel, data).await;
        }
    });
    Some(call.runs.iter().map(|run| run.id.clone()).collect())
}

async fn start_runner(exe: &Path, dir: &Path, call: &Call) -> std::result::Result<(), String> {
    call::write_json(&dir.join(call::CALL_FILE), call).map_err(|err| err.to_string())?;
    let output = tokio::process::Command::new(exe)
        .args([crate::NAME, DETACH_FLAG])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn discard(root: &Path, runs: &[CallRun]) {
    for run in runs {
        let _ = std::fs::remove_dir_all(root.join(&run.id));
    }
}

async fn emit(host: &Host, channel: &str, data: Value) {
    if let Err(err) = host.emit(channel, data).await {
        pi_ext_sdk::debug!("{channel} emit failed: {}", err.message());
    }
}

/// Adopts and follows the detached calls of the current session until shutdown.
pub async fn serve(app: App, host: Host) -> Result<()> {
    let mut tracker = Tracker::default();
    loop {
        tracker.tick(&app, &host).await;
        tokio::time::sleep(TICK).await;
    }
}

#[derive(Default)]
struct Tracker {
    /// Followed calls by async id.
    calls: BTreeMap<String, Tracked>,
    /// Run directories that hold no undelivered call of `session`.
    skip: HashSet<String>,
    session: Option<String>,
    last_tick: Option<SystemTime>,
}

struct Tracked {
    dir: PathBuf,
    call: Call,
    report: Option<Report>,
    /// `result.json` arrived, or the runner stopped without one.
    finished: bool,
    stale_since: Option<SystemTime>,
    /// Runs whose stop command is written.
    stopping: HashSet<String>,
    claim: Option<Held>,
    retry_at: Instant,
    backoff: Duration,
    /// Since when pi is idle while the sent message is not in the session.
    idle_since: Option<Instant>,
}

/// The notify claim of this process.
struct Held {
    claim: Claim,
    refreshed: Instant,
}

impl Tracker {
    async fn tick(&mut self, app: &App, host: &Host) {
        let now = SystemTime::now();
        // A pause of this process (a sleep of the machine) restarts the stale heartbeat checks.
        let paused = self
            .last_tick
            .and_then(|last| now.duration_since(last).ok())
            .is_some_and(|gap| gap > STALE_CONFIRM);
        self.last_tick = Some(now);
        let session = host.snapshot().and_then(|s| s.session_id.clone());
        let session = session.filter(|id| !id.is_empty());
        if session != self.session {
            self.skip.clear();
            self.session.clone_from(&session);
        }
        let root = store::runs_root(&host.agent_dir());
        let adopted = match &session {
            Some(session) => self.adopt(app, host, &root, session).await,
            None => false,
        };
        if self.calls.is_empty() {
            return;
        }
        let keep = app.config(host).max_recent_runs;
        let ids: Vec<String> = self.calls.keys().cloned().collect();
        for id in ids {
            let Some(tracked) = self.calls.get_mut(&id) else {
                continue;
            };
            if !tracked.dir.join(call::CALL_FILE).exists() {
                self.calls.remove(&id);
                continue;
            }
            tracked.refresh(app, host, keep, paused).await;
            let current = session.as_deref() == Some(tracked.call.session_id.as_str());
            if tracked.finished && current && tracked.deliver(host).await {
                self.calls.remove(&id);
                self.skip.insert(id);
            }
        }
        if adopted {
            crate::active_work::publish(app, host, false).await;
        }
    }

    /// Follows the undelivered calls of `session` in `root`; `true` when it adopted one. Their
    /// runs enter the table with the runner's last state, still running, so a run that ended
    /// while no extension followed it ends at the next refresh with its bus events. The child's
    /// todo state goes out again, since it may have changed while pi was down.
    async fn adopt(&mut self, app: &App, host: &Host, root: &Path, session: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        let mut adopted = false;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.calls.contains_key(&name) || self.skip.contains(&name) {
                continue;
            }
            let dir = entry.path();
            let Some(call) = call::read_call(&dir) else {
                let old = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age > PLAIN_AFTER);
                if old || dir.join(call::CALL_FILE).exists() {
                    self.skip.insert(name);
                }
                continue;
            };
            if call.session_id != session || call.async_id() != name || call::delivered(&dir) {
                self.skip.insert(name);
                continue;
            }
            let started = dir.join(call::STATE_FILE).exists()
                || dir.join(call::RESULT_FILE).exists()
                || call::now_ms().saturating_sub(call.created_ms) > ADOPT_AFTER_MS;
            if !started {
                continue;
            }
            let report = call::read_report(&dir).map(|(report, _)| report);
            let runs = call.runs(report.as_ref());
            for run in &runs {
                app.runs.insert(Run {
                    status: Status::Running,
                    finished: None,
                    ..run.clone()
                });
            }
            pi_ext_sdk::debug!("adopted the detached subagent call {name}");
            self.calls.insert(name, Tracked::new(dir, call));
            adopted = true;
            for run in runs.iter().filter(|run| run.status == Status::Running) {
                if let Some(todo) = &run.progress.todo {
                    let scope = scope_payload(&run.id, &run.label(), "running", todo.clone());
                    emit(host, TODO_SCOPE_CHANNEL, scope).await;
                }
            }
        }
        if adopted {
            app.start_widget(host);
        }
        adopted
    }
}

impl Tracked {
    fn new(dir: PathBuf, call: Call) -> Tracked {
        Tracked {
            dir,
            call,
            report: None,
            finished: false,
            stale_since: None,
            stopping: HashSet::new(),
            claim: None,
            retry_at: Instant::now(),
            backoff: TICK,
            idle_since: None,
        }
    }

    /// Writes stop commands for cancelled runs, reads the runner's state, and applies it to the
    /// run table.
    async fn refresh(&mut self, app: &App, host: &Host, keep: usize, paused: bool) {
        if !self.finished {
            for run in &self.call.runs {
                let cancelled = app
                    .runs
                    .get(&run.id)
                    .is_some_and(|r| r.status == Status::Running && r.cancel.is_cancelled());
                if cancelled && !self.stopping.contains(&run.id) {
                    match call::request_stop(&self.dir, &run.id) {
                        Ok(()) => {
                            self.stopping.insert(run.id.clone());
                        }
                        Err(err) => pi_ext_sdk::warn!("cannot stop run {}: {err}", run.id),
                    }
                }
            }
            if let Some((report, last)) = call::read_report(&self.dir) {
                self.report = Some(report);
                self.finished = last;
            }
            if !self.finished && self.runner_stopped(paused) {
                self.report = Some(self.failed_report());
                self.finished = true;
                store::write_status(&self.dir, &self.call.runs(self.report.as_ref()));
            }
        }
        for run in self.call.runs(self.report.as_ref()) {
            apply(app, host, &run, keep).await;
        }
    }

    /// Whether the heartbeat stayed stale for [`STALE_CONFIRM`] without a pause.
    fn runner_stopped(&mut self, paused: bool) -> bool {
        let beat = self
            .report
            .as_ref()
            .map_or(self.call.created_ms, |report| report.heartbeat_ms);
        if paused || call::now_ms().saturating_sub(beat) <= STALE_MS {
            self.stale_since = None;
            return false;
        }
        let now = SystemTime::now();
        let since = *self.stale_since.get_or_insert(now);
        now.duration_since(since)
            .is_ok_and(|stale| stale >= STALE_CONFIRM)
    }

    /// The last state with each running run failed.
    fn failed_report(&self) -> Report {
        let mut report = self.report.clone().unwrap_or_default();
        report.runs = self
            .call
            .runs(self.report.as_ref())
            .into_iter()
            .map(|mut run| {
                if run.status == Status::Running {
                    run.status = Status::Failed;
                    run.finished = Some(Instant::now());
                    run.progress.error = Some(RUNNER_STOPPED.into());
                }
                RunReport::of(&run)
            })
            .collect();
        report
    }

    /// Sends the notify of a finished call once: `notify.done` ends it for every process, a
    /// message already in the session counts as sent, and only the holder of `notify.claim`
    /// sends. A message that pi accepted during a turn only waits in pi's queue, so `notify.done`
    /// follows only once the message is in the session entries. `true` when delivered.
    async fn deliver(&mut self, host: &Host) -> bool {
        if call::delivered(&self.dir) {
            return true;
        }
        if self.claim.is_none() {
            let me = Claim::mine();
            let mut held = Held {
                claim: me.clone(),
                refreshed: Instant::now(),
            };
            match call::claim(&self.dir, &me) {
                Claimed::Busy => return false,
                Claimed::Fresh => {}
                // The last holder ran in this pi (a `/reload`), so its message waits in the queue.
                Claimed::TakenOver(Some(stale))
                    if stale.sent_ms.is_some() && stale.pi_pid == me.pi_pid =>
                {
                    held.claim.sent_ms = stale.sent_ms;
                    call::refresh_claim(&self.dir, &held.claim);
                }
                Claimed::TakenOver(_) => {}
            }
            self.claim = Some(held);
        }
        let Some(held) = self.claim.as_mut() else {
            return false;
        };
        if held.refreshed.elapsed() >= call::HEARTBEAT {
            if !call::refresh_claim(&self.dir, &held.claim) {
                self.claim = None;
                return false;
            }
            held.refreshed = Instant::now();
        }
        let sent = held.claim.sent_ms.is_some();
        if sent && !host.is_idle().await.unwrap_or(false) {
            self.idle_since = None;
            return false;
        }
        if !sent && Instant::now() < self.retry_at {
            return false;
        }
        match self.saved(host).await {
            Some(true) => return self.mark_delivered(),
            Some(false) if !sent => {}
            Some(false) => {
                let idle = *self.idle_since.get_or_insert_with(Instant::now);
                if idle.elapsed() < LOST_AFTER {
                    return false;
                }
                pi_ext_sdk::warn!(
                    "the subagent result {} was lost; sending it again",
                    self.call.async_id()
                );
                self.idle_since = None;
            }
            None => return false,
        }
        let runs = self.call.runs(self.report.as_ref());
        let message = notify_message(&runs, &self.call.mode);
        match host.send_message(message).await {
            Ok(()) => {
                if let Some(held) = self.claim.as_mut() {
                    held.claim.sent_ms = Some(call::now_ms());
                    call::refresh_claim(&self.dir, &held.claim);
                    held.refreshed = Instant::now();
                }
                emit(host, WAKE_CHANNEL, wake_data("subagents")).await;
            }
            Err(err) => {
                pi_ext_sdk::debug!("subagent result not delivered yet: {}", err.message());
                self.retry_at = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(RETRY_MAX);
            }
        }
        false
    }

    /// Whether the session holds a `subagent-notify` message for a run of this call; `None`
    /// when pi does not answer.
    async fn saved(&self, host: &Host) -> Option<bool> {
        let entries = host.entries().await.ok()?;
        let ids: HashSet<&str> = self.call.runs.iter().map(|run| run.id.as_str()).collect();
        let notifies = entries.iter().filter(|entry| {
            entry["type"] == "custom_message" && entry["customType"] == NOTIFY_TYPE
        });
        let mut runs =
            notifies.flat_map(|entry| entry["details"]["runs"].as_array().into_iter().flatten());
        Some(runs.any(|run| run["id"].as_str().is_some_and(|id| ids.contains(id))))
    }

    fn mark_delivered(&mut self) -> bool {
        match call::mark_delivered(&self.dir) {
            Ok(()) => true,
            Err(err) => {
                pi_ext_sdk::warn!(
                    "cannot write {}: {err}",
                    self.dir.join(call::DONE_FILE).display()
                );
                false
            }
        }
    }
}

/// Applies the runner's state of `run` to the run table and emits the bus events of the
/// change: the child's todo state, and when the run ended, the active work, its final scope,
/// and `subagents:stop`.
async fn apply(app: &App, host: &Host, run: &Run, keep: usize) {
    let Some(old) = app.runs.get(&run.id) else {
        return;
    };
    app.runs.update(&run.id, |known| {
        known.progress = run.progress.clone();
        known.stderr.clone_from(&run.stderr);
        known.exit_code = run.exit_code;
        known.model.clone_from(&run.model);
    });
    let ended = old.status == Status::Running && run.status != Status::Running;
    if ended {
        app.runs.finish(&run.id, run.status, keep);
        app.runs
            .update(&run.id, |known| known.finished = run.finished);
    }
    if run.progress.todo != old.progress.todo
        && let Some(todo) = &run.progress.todo
    {
        let scope = scope_payload(&run.id, &run.label(), "running", todo.clone());
        emit(host, TODO_SCOPE_CHANNEL, scope).await;
    }
    if !ended {
        return;
    }
    crate::active_work::publish(app, host, false).await;
    let scope = scope_payload(&run.id, &run.label(), run.status.as_str(), Value::Null);
    emit(host, TODO_SCOPE_CHANNEL, scope).await;
    let output = run.progress.output();
    let stop = stop_payload(&run.id, &run.agent, run.status, &output);
    emit(host, STOP_CHANNEL, stop).await;
}
