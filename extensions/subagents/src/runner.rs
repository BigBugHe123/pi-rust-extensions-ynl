//! The runner of a detached `async` call. `pi-rust-ext subagents --detach <asyncDir>` starts
//! `--runner <asyncDir>` outside pi's process tree and exits at once. The runner runs the call's
//! children, folds their events into `run.json`, keeps `status.json` current, stops a run when
//! `commands/stop-<run id>` appears, and writes `result.json` last. pi sessions read these files.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::call::{self, Report, RunReport};
use crate::exec;
use crate::runs::{Runs, Status};
use crate::store;

pub const DETACH_FLAG: &str = "--detach";
pub const RUNNER_FLAG: &str = "--runner";
const TICK: Duration = Duration::from_millis(250);
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// `--detach <dir>`: starts the runner and exits, so the runner's parent is not pi and a tree
/// kill of pi does not reach it.
pub fn detach(dir: Option<&str>) -> ExitCode {
    let Some(dir) = dir.map(PathBuf::from) else {
        pi_ext_sdk::error!("usage: pi-rust-ext {} {DETACH_FLAG} <dir>", crate::NAME);
        return ExitCode::from(2);
    };
    match spawn_runner(&dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            pi_ext_sdk::error!("cannot start the subagents runner: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Starts `--runner <dir>` with its stderr in `runner.log`: on Unix in a process group of its
/// own, on Windows without a console and out of pi's job when the job allows it.
fn spawn_runner(dir: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(call::LOG_FILE))?;
    let command = |log: std::fs::File| {
        let mut command = std::process::Command::new(&exe);
        command
            .args([crate::NAME, RUNNER_FLAG])
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        command
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command(log).process_group(0).spawn().map(drop)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        command(log.try_clone()?)
            .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .or_else(|_| command(log).creation_flags(flags).spawn())
            .map(drop)
    }
}

/// `--runner <dir>`: runs the call in `dir` to its end.
pub fn main(dir: Option<&str>) -> ExitCode {
    let Some(dir) = dir.map(PathBuf::from) else {
        pi_ext_sdk::error!("usage: pi-rust-ext {} {RUNNER_FLAG} <dir>", crate::NAME);
        return ExitCode::from(2);
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(err) => {
            pi_ext_sdk::error!("cannot start the tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(serve(dir));
    runtime.shutdown_background();
    code
}

async fn serve(dir: PathBuf) -> ExitCode {
    let Some(call) = call::read_call(&dir) else {
        pi_ext_sdk::error!("cannot read {}", dir.join(call::CALL_FILE).display());
        return ExitCode::FAILURE;
    };
    ignore_hangup();
    let runs = Runs::default();
    for run in call.runs(None) {
        runs.insert(run);
    }
    let changed = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let mut children = JoinSet::new();
    let mut next = 0;
    write_report(&dir, &runs, call::STATE_FILE);
    let mut written = call::now_ms();
    loop {
        for (index, run) in call.runs.iter().enumerate() {
            let running = runs
                .get(&run.id)
                .is_some_and(|r| r.status == Status::Running);
            if !running || !call::stop_requested(&dir, &run.id) {
                continue;
            }
            runs.cancel(&run.id);
            if index >= next {
                runs.finish(&run.id, Status::Cancelled, usize::MAX);
                ended.store(true, Ordering::SeqCst);
            }
        }
        // The runs start in call order, `maxParallel` at a time.
        while next < call.runs.len() && children.len() < call.max_parallel.max(1) {
            let run = &call.runs[next];
            next += 1;
            let Some(cancel) = runs
                .get(&run.id)
                .filter(|r| r.status == Status::Running)
                .map(|r| r.cancel)
            else {
                continue;
            };
            let (runs, changed, ended) = (runs.clone(), changed.clone(), ended.clone());
            let (id, spec) = (run.id.clone(), run.spec.clone());
            let run_dir = run.session_file.parent().unwrap_or(&dir).to_owned();
            children.spawn(async move {
                let mut on_effect = |_| changed.store(true, Ordering::SeqCst);
                let status =
                    exec::execute_to_files(&spec, &runs, &id, &cancel, &run_dir, &mut on_effect)
                        .await;
                runs.finish(&id, status, usize::MAX);
                ended.store(true, Ordering::SeqCst);
            });
        }
        if ended.swap(false, Ordering::SeqCst) {
            store::write_status(&dir, &runs.all());
            changed.store(true, Ordering::SeqCst);
        }
        let beat = call::now_ms().saturating_sub(written) >= call::HEARTBEAT.as_millis() as u64;
        if changed.swap(false, Ordering::SeqCst) || beat {
            write_report(&dir, &runs, call::STATE_FILE);
            written = call::now_ms();
        }
        if next == call.runs.len() && children.is_empty() {
            break;
        }
        tokio::select! {
            Some(_) = children.join_next() => {}
            () = tokio::time::sleep(TICK) => {}
        }
    }
    store::write_status(&dir, &runs.all());
    write_report(&dir, &runs, call::STATE_FILE);
    write_report(&dir, &runs, call::RESULT_FILE);
    ExitCode::SUCCESS
}

fn write_report(dir: &Path, runs: &Runs, file: &str) {
    let report = Report {
        pid: std::process::id(),
        heartbeat_ms: call::now_ms(),
        runs: runs.all().iter().map(RunReport::of).collect(),
    };
    if let Err(err) = call::write_json(&dir.join(file), &report) {
        pi_ext_sdk::warn!("cannot write {}: {err}", dir.join(file).display());
    }
}

/// A terminal hangup must not end the runner.
fn ignore_hangup() {
    #[cfg(unix)]
    if let Ok(mut hangup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        tokio::spawn(async move { while hangup.recv().await.is_some() {} });
    }
}
