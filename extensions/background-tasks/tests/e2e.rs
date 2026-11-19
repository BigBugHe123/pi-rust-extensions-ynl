//! The extension under the real pi binary: tools, prompt section, task listing, and a task that
//! outlives a Paseo-style stop of pi in RPC mode.

use pi_ext_e2e::{Run, Turn, cargo_example, json, pi_bin_or_skip};

#[test]
fn bg_run_and_status_through_pi() {
    if pi_bin_or_skip().is_none() {
        return;
    }
    let binary = cargo_example("pi-ext-background-tasks", "background-tasks");
    let run = json!({"name": "E2E sleeper", "command": "sleep 5", "isAgent": false});
    let output = Run::new(binary)
        .args(["background-tasks"])
        .turns([
            Turn::tool_call("bg_run", run),
            Turn::tool_call("bg_status", json!({})),
            Turn::text("Started."),
        ])
        .prompt("Run a sleeper in the background")
        .run()
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    let started = output.tool_result_text("bg_run").unwrap_or_default();
    assert!(
        started.starts_with("Started background task E2E sleeper"),
        "{started}"
    );
    let status = output.tool_result_text("bg_status").unwrap_or_default();
    assert!(status.contains("E2E sleeper [running]"), "{status}");
    for tool in ["bg_run", "bg_status", "bg_logs", "bg_kill"] {
        assert!(output.tool_names(0).contains(&tool.to_string()), "{tool}");
    }
    assert!(
        output
            .system_prompt(0)
            .contains("Use bg_run for long commands")
    );
    assert_eq!(output.final_text(), "Started.");
}

#[cfg(unix)]
mod rpc {
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use pi_ext_e2e::{MockProvider, TempDir, Turn, Value, cargo_example, json, pi_bin_or_skip};

    /// One `pi --mode rpc` process, as Paseo runs it.
    struct RpcPi {
        child: Child,
        stdin: Option<ChildStdin>,
        events: Receiver<Value>,
    }

    impl RpcPi {
        fn start(pi: &Path, root: &Path, args: &[&str], log: &str) -> RpcPi {
            let stderr = std::fs::File::create(root.join(log)).unwrap();
            let mut child = Command::new(pi)
                .args(["--mode", "rpc"])
                .args(args)
                .current_dir(root.join("work"))
                .env_clear()
                .envs(env(root))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(stderr)
                .spawn()
                .unwrap();
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().unwrap();
            let (tx, events) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(value) = serde_json::from_str::<Value>(&line)
                        && tx.send(value).is_err()
                    {
                        break;
                    }
                }
            });
            RpcPi {
                child,
                stdin,
                events,
            }
        }

        fn send(&mut self, command: Value) {
            let stdin = self.stdin.as_mut().expect("stdin open");
            writeln!(stdin, "{command}").unwrap();
        }

        /// Reads events until one matches; `None` after `timeout`.
        fn wait_for(
            &mut self,
            timeout: Duration,
            matches: impl Fn(&Value) -> bool,
        ) -> Option<Value> {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Ok(event) = self.events.recv_timeout(Duration::from_millis(50))
                    && matches(&event)
                {
                    return Some(event);
                }
            }
            None
        }

        /// Paseo's stop: stdin EOF, then at once SIGTERM to pi and each descendant (by parent
        /// pid, as the tree-kill package finds them), and SIGKILL after 2 seconds.
        fn paseo_stop(mut self) -> Vec<u32> {
            let tree = tree_of(self.child.id());
            drop(self.stdin.take());
            for pid in &tree {
                signal("-TERM", *pid);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && self.child.try_wait().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            for pid in &tree {
                if alive(*pid) {
                    signal("-KILL", *pid);
                }
            }
            let _ = self.child.wait();
            tree
        }

        /// Closes stdin and waits for pi to exit.
        fn quit(mut self) {
            drop(self.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && self.child.try_wait().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for RpcPi {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn env(root: &Path) -> Vec<(String, String)> {
        let mut env = vec![
            ("HOME".to_owned(), root.display().to_string()),
            (
                "PI_CODING_AGENT_DIR".to_owned(),
                root.join("agent").display().to_string(),
            ),
            ("PI_OFFLINE".to_owned(), "1".to_owned()),
        ];
        for key in ["PATH", "TMPDIR", "LANG"] {
            if let Ok(value) = std::env::var(key) {
                env.push((key.to_owned(), value));
            }
        }
        env
    }

    fn tree_of(pid: u32) -> Vec<u32> {
        let output = Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "ppid="])
            .output()
            .unwrap();
        let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace().map(|f| f.parse::<u32>());
                Some((fields.next()?.ok()?, fields.next()?.ok()?))
            })
            .collect();
        let mut tree = vec![pid];
        let mut next = 0;
        while next < tree.len() {
            let parent = tree[next];
            tree.extend(pairs.iter().filter(|(_, p)| *p == parent).map(|(c, _)| *c));
            next += 1;
        }
        tree
    }

    fn signal(signal: &str, pid: u32) {
        let _ = Command::new("kill")
            .args([signal, &pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }

    /// A process that exists and is not a zombie.
    fn alive(pid: u32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        !state.is_empty() && !state.starts_with('Z')
    }

    fn read_json(path: &Path) -> Value {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    }

    fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    /// The `background-task-notification` messages for `id` in the session file.
    fn saved_notices(session_file: &Path, id: &str) -> usize {
        let text = std::fs::read_to_string(session_file).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| {
                entry["type"] == "custom_message"
                    && entry["customType"] == "background-task-notification"
                    && entry["details"]["id"] == id
            })
            .count()
    }

    fn write_files(root: &Path, binary: &Path, url: &str) {
        for dir in ["agent", "work", "pkg"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let models = json!({"providers": {"mock": {"baseUrl": url, "api": "openai-completions",
            "apiKey": "e2e-key", "models": [{"id": "mock-model", "contextWindow": 100000,
            "maxTokens": 4000}]}}});
        std::fs::write(root.join("agent/models.json"), models.to_string()).unwrap();
        let native = json!({"command": binary, "args": ["background-tasks"],
            "env": {"PI_EXT_LOG": "info"}});
        let package = json!({"name": "e2e", "pi": {"native": [native]}});
        std::fs::write(root.join("pkg/package.json"), package.to_string()).unwrap();
        let settings = json!({"defaultProvider": "mock", "defaultModel": "mock-model",
            "packages": [root.join("pkg").display().to_string()]});
        std::fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
    }

    /// A command that runs until `release` exists, at most a minute, then prints `then`.
    fn gated(release: &Path, then: &str) -> String {
        format!(
            "n=0; while [ ! -f '{}' ] && [ $n -lt 1200 ]; do sleep 0.05; n=$((n+1)); done; echo {then}",
            release.display()
        )
    }

    /// The task records of `session_id`, by name.
    fn tasks(root: &Path, session_id: &str) -> std::collections::BTreeMap<String, Value> {
        let sessions = root
            .join("agent/ext/background-tasks/sessions")
            .join(session_id);
        std::fs::read_dir(sessions)
            .unwrap()
            .flatten()
            .map(|entry| read_json(&entry.path().join("task.json")))
            .map(|task| (task["name"].as_str().unwrap_or_default().to_owned(), task))
            .collect()
    }

    /// Starts a task `Keep` and a task `Stop` (`surviveReload: false`) in `pi --mode rpc <args>`,
    /// closes pi's stdin, and returns the two task records once pi exited, with each task's
    /// liveness. The tasks end before the call returns.
    fn graceful_quit(pi: &Path, args: &[&str]) -> Vec<(Value, bool)> {
        let binary = cargo_example("pi-ext-background-tasks", "background-tasks");
        let root = TempDir::new().unwrap();
        let root = root.path();
        let release = root.join("release");
        let keep = json!({"name": "Keep", "command": gated(&release, "kept"), "isAgent": false});
        let stop = json!({"name": "Stop", "command": gated(&release, "stopped"),
            "isAgent": false, "surviveReload": false});
        let provider = MockProvider::start(vec![
            Turn::tool_calls([("bg_run", keep), ("bg_run", stop)]),
            Turn::text("Started."),
        ])
        .unwrap();
        write_files(root, &binary, provider.url());
        let mut pi = RpcPi::start(pi, root, args, "pi.stderr");
        pi.send(json!({"id": "state", "type": "get_state"}));
        let state = pi
            .wait_for(Duration::from_secs(30), |e| e["id"] == "state")
            .expect("no get_state response");
        let session_id = state["data"]["sessionId"].as_str().unwrap().to_owned();
        pi.send(json!({"id": "go", "type": "prompt", "message": "Run both"}));
        let log = || std::fs::read_to_string(root.join("pi.stderr")).unwrap_or_default();
        let ended = pi.wait_for(Duration::from_secs(60), |e| e["type"] == "agent_end");
        assert!(ended.is_some(), "the turn did not end: {}", log());
        let pids: Vec<u32> = ["Keep", "Stop"]
            .map(|name| tasks(root, &session_id)[name]["pid"].as_u64().unwrap() as u32)
            .to_vec();
        assert!(pids.iter().all(|pid| alive(*pid)));

        // stdin EOF only: pi sends `session_shutdown` (quit) and exits.
        pi.quit();
        std::thread::sleep(Duration::from_millis(500));
        let records = tasks(root, &session_id);
        let result = ["Keep", "Stop"]
            .iter()
            .zip(&pids)
            .map(|(name, pid)| (records[*name].clone(), alive(*pid)))
            .collect();
        std::fs::write(&release, "").unwrap();
        assert!(wait_until(Duration::from_secs(5), || pids
            .iter()
            .all(|pid| !alive(*pid))));
        result
    }

    #[test]
    fn a_graceful_quit_of_pi_in_rpc_mode_keeps_survivors_and_stops_the_others() {
        let Some(pi) = pi_bin_or_skip() else {
            return;
        };
        let tasks = graceful_quit(&pi, &[]);
        let (keep, keep_alive) = &tasks[0];
        assert_eq!(keep["status"], "running", "{keep}");
        assert!(keep_alive, "the survivor ended with pi");
        let (stop, stop_alive) = &tasks[1];
        assert_eq!(stop["status"], "killed", "{stop}");
        assert_eq!(stop["error"], "stopped on session quit");
        assert!(!stop_alive);
    }

    #[test]
    fn a_quit_of_pi_without_a_session_file_stops_every_task() {
        let Some(pi) = pi_bin_or_skip() else {
            return;
        };
        // No later pi can resume a `--no-session` session, so nothing would adopt the tasks.
        for (task, alive) in graceful_quit(&pi, &["--no-session"]) {
            assert_eq!(task["status"], "killed", "{task}");
            assert_eq!(task["error"], "stopped on session quit");
            assert!(!alive, "{task}");
        }
    }

    #[test]
    fn a_task_outlives_a_paseo_stop_of_pi_and_the_resumed_session_gets_one_notification() {
        let Some(pi) = pi_bin_or_skip() else {
            return;
        };
        let binary = cargo_example("pi-ext-background-tasks", "background-tasks");
        let root = TempDir::new().unwrap();
        let root = root.path();
        let release = root.join("release");
        let command = gated(&release, "survived");
        let run = json!({"name": "Survivor", "command": command, "isAgent": false});
        let provider = MockProvider::start(vec![
            Turn::tool_call("bg_run", run),
            Turn::text("Started."),
            Turn::tool_call("bg_status", json!({})),
            Turn::text("Still running."),
            Turn::text("Noted."),
        ])
        .unwrap();
        write_files(root, &binary, provider.url());

        let mut first = RpcPi::start(&pi, root, &[], "pi-1.stderr");
        first.send(json!({"id": "state", "type": "get_state"}));
        let state = first
            .wait_for(Duration::from_secs(30), |e| e["id"] == "state")
            .expect("no get_state response");
        let session_file = PathBuf::from(state["data"]["sessionFile"].as_str().unwrap());
        let session_id = state["data"]["sessionId"].as_str().unwrap().to_owned();
        first.send(json!({"id": "go", "type": "prompt", "message": "Run the survivor"}));
        let ended = first.wait_for(Duration::from_secs(60), |e| e["type"] == "agent_end");
        let log = || std::fs::read_to_string(root.join("pi-1.stderr")).unwrap_or_default();
        assert!(ended.is_some(), "the turn did not end: {}", log());
        let task = tasks(root, &session_id)["Survivor"].clone();
        assert_eq!(task["status"], "running", "{task}");
        let task_file = PathBuf::from(task["dir"].as_str().unwrap()).join("task.json");
        let id = task["id"].as_str().unwrap().to_owned();
        let pid = task["pid"].as_u64().unwrap() as u32;

        let stopped = first.paseo_stop();
        assert!(
            stopped.len() > 1,
            "pi's extension is in its tree: {stopped:?}"
        );
        assert!(!stopped.contains(&pid), "{stopped:?}");
        assert!(alive(pid), "the task ended with pi");
        assert_eq!(read_json(&task_file)["status"], "running");

        // The resumed pi adopts the running task.
        let file = session_file.display().to_string();
        let mut second = RpcPi::start(&pi, root, &["--session", &file], "pi-2.stderr");
        second.send(json!({"id": "check", "type": "prompt", "message": "Check the survivor"}));
        let status = second.wait_for(Duration::from_secs(60), |e| {
            e["type"] == "tool_execution_end" && e["toolName"] == "bg_status"
        });
        let log = || std::fs::read_to_string(root.join("pi-2.stderr")).unwrap_or_default();
        let status = status.unwrap_or_else(|| panic!("no bg_status result: {}", log()));
        let text = status["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains(&format!("{id} Survivor [running]")),
            "{status}"
        );
        let ended = second.wait_for(Duration::from_secs(60), |e| e["type"] == "agent_end");
        assert!(ended.is_some(), "the check did not end: {}", log());
        let released = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(&release, "").unwrap();
        let confirmed = wait_until(Duration::from_secs(60), || {
            while second.events.try_recv().is_ok() {}
            read_json(&task_file)["notified"] == true
        });
        assert!(confirmed, "{}\n{}", read_json(&task_file), log());
        let task = read_json(&task_file);
        assert_eq!(task["status"], "completed", "{task}");
        assert_eq!(task["exitCode"], 0);
        assert!(task["endedAt"].as_u64().unwrap() >= released, "{task}");
        std::thread::sleep(Duration::from_secs(2));
        second.quit();
        assert_eq!(saved_notices(&session_file, &id), 1, "{}", log());
        let requests = provider.requests();
        assert_eq!(requests.len(), 5, "{requests:?}");
        let last = requests[4].to_string();
        assert!(last.contains("background-task-notification") && last.contains("survived"));
    }
}
