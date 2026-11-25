//! Run directories under `<agentDir>/ext/subagents/runs`: child session files, the async
//! `status.json`, and removal of old run directories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pi_ext_sdk::prelude::*;

use crate::runs::{Run, Status};

pub const RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);
pub const STATUS_FILE: &str = "status.json";

pub fn runs_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("ext/subagents/runs")
}

pub fn session_file(root: &Path, run_id: &str) -> PathBuf {
    root.join(run_id).join("session.jsonl")
}

/// Makes the directory of a new run and returns its id. The directory must not exist, so an id
/// that another process made too (same clock and counter) goes to one process only.
pub fn claim_run_dir(root: &Path, mut next_id: impl FnMut() -> String) -> std::io::Result<String> {
    std::fs::create_dir_all(root)?;
    let mut attempts = 0;
    loop {
        let id = next_id();
        match std::fs::create_dir(root.join(&id)) {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempts < 1000 => {
                attempts += 1;
            }
            result => return result.map(|()| id),
        }
    }
}

/// Deletes run directories in `root` last modified more than `max_age` ago, except the runs of
/// detached calls that run or wait for their notify.
pub fn prune(root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let dirs: Vec<std::fs::DirEntry> = entries.flatten().collect();
    let kept: HashSet<String> = dirs
        .iter()
        .flat_map(|entry| crate::call::undelivered_runs(&entry.path()))
        .collect();
    let now = SystemTime::now();
    for entry in dirs {
        if kept.contains(entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        let expired = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_dir())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if expired && let Err(err) = std::fs::remove_dir_all(entry.path()) {
            pi_ext_sdk::warn!("cannot remove {}: {err}", entry.path().display());
        }
    }
}

/// `running` until every run ends, then `complete`, `stopped` (any cancelled), or `failed`.
pub fn call_state(runs: &[Run]) -> &'static str {
    if runs.iter().any(|r| r.status == Status::Running) {
        "running"
    } else if runs.iter().all(|r| r.status == Status::Completed) {
        "complete"
    } else if runs.iter().any(|r| r.status == Status::Cancelled) {
        "stopped"
    } else {
        "failed"
    }
}

pub fn step_status(status: Status) -> &'static str {
    match status {
        Status::Running => "running",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Cancelled => "stopped",
    }
}

pub fn status_json(runs: &[Run]) -> Value {
    let steps: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({ "agent": r.agent, "status": step_status(r.status), "sessionFile": r.session_file })
        })
        .collect();
    json!({ "state": call_state(runs), "steps": steps })
}

/// Writes `<dir>/status.json` through a rename, so readers see a whole file.
pub fn write_status(dir: &Path, runs: &[Run]) {
    let path = dir.join(STATUS_FILE);
    let written = crate::registry::write_atomic(&path, &status_json(runs).to_string());
    if let Err(err) = written {
        pi_ext_sdk::warn!("cannot write {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::Runs;

    fn runs_with(statuses: &[Status]) -> Vec<Run> {
        let runs = Runs::default();
        for status in statuses {
            let id = runs.next_id();
            runs.insert(Run::new(&id, "explore", "t", true));
            if *status != Status::Running {
                runs.finish(&id, *status, 10);
            }
        }
        runs.all()
    }

    #[test]
    fn call_state_follows_the_runs() {
        use Status::*;
        let cases: [(&[Status], &str); 5] = [
            (&[Running, Completed], "running"),
            (&[Completed, Completed], "complete"),
            (&[Completed, Failed], "failed"),
            (&[Failed, Cancelled], "stopped"),
            (&[Cancelled], "stopped"),
        ];
        for (statuses, state) in cases {
            assert_eq!(call_state(&runs_with(statuses)), state, "{statuses:?}");
        }
        let steps = status_json(&runs_with(&[Cancelled, Failed]))["steps"].clone();
        assert_eq!(steps[0]["status"], "stopped");
        assert_eq!(steps[1]["status"], "failed");
    }

    #[cfg(unix)]
    #[test]
    fn prune_removes_only_old_run_directories() {
        use crate::call::{CALL_FILE, mark_delivered};
        let root = std::env::temp_dir().join(format!("subagents-prune-{}", std::process::id()));
        let names = ["old", "new", "call", "call-run", "done"];
        let [old, new, call, call_run, done] = names.map(|name| root.join(name));
        let detached = json!({ "runs": [{ "id": "call" }, { "id": "call-run" }] });
        for dir in [&call, &done] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(CALL_FILE), detached.to_string()).unwrap();
        }
        mark_delivered(&done).unwrap();
        let past = SystemTime::now() - RETENTION - Duration::from_secs(60);
        for dir in [&old, &new, &call, &call_run, &done] {
            std::fs::create_dir_all(dir).unwrap();
            if dir != &new {
                std::fs::File::open(dir)
                    .unwrap()
                    .set_modified(past)
                    .unwrap();
            }
        }
        prune(&root, RETENTION);
        assert!(!old.exists() && !done.exists());
        assert!(new.exists() && call.exists() && call_run.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn run_ids_do_not_collide_across_processes() {
        let root = std::env::temp_dir().join(format!("subagents-ids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claim = |root: std::path::PathBuf| {
            // A fresh `Runs` per thread stands for a process: its own counter, the same clock.
            std::thread::spawn(move || {
                let runs = Runs::default();
                let ids: Vec<String> = (0..50)
                    .map(|_| claim_run_dir(&root, || runs.next_id()).unwrap())
                    .collect();
                ids
            })
        };
        let threads: Vec<_> = (0..4).map(|_| claim(root.clone())).collect();
        let ids: Vec<String> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 200);
        assert!(ids.iter().all(|id| root.join(id).is_dir()));
        // Two processes that make the same first id: the second one moves on to its next id.
        let mut first = ["sa1", "sa2"].into_iter().map(String::from);
        let mut second = ["sa1", "sa3"].into_iter().map(String::from);
        let claimed = claim_run_dir(&root, || first.next().unwrap()).unwrap();
        assert_eq!(claimed, "sa1");
        let claimed = claim_run_dir(&root, || second.next().unwrap()).unwrap();
        assert_eq!(claimed, "sa3");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
