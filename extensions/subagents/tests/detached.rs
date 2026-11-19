//! Detached `async` calls through the `pi-rust-ext subagents` binary: a pi stop the way Paseo does
//! it (stdin EOF, then SIGTERM and SIGKILL to every descendant), a new extension process for the
//! same session, and exactly one `subagent-notify` per call. A fake pi child prints scripted
//! events and runs until the test releases it.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pi_ext_e2e::{TempDir, cargo_bin};
use serde_json::{Value, json};

const SESSION: &str = "session-1";

fn binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| cargo_bin("pi-rust-ext", "pi-rust-ext"))
}

/// Temp dirs, a fake pi, and the session entries that every extension process of a test shares.
struct Setup {
    agent: PathBuf,
    cwd: PathBuf,
    fake: PathBuf,
    entries: Arc<Mutex<Vec<Value>>>,
    root: TempDir,
}

impl Setup {
    fn new(answer: &str) -> Setup {
        let root = TempDir::new().unwrap();
        let [agent, cwd, fake] = ["agent", "work", "fake"].map(|name| root.path().join(name));
        for dir in [&agent, &cwd, &fake] {
            std::fs::create_dir_all(dir.join("ext")).unwrap();
        }
        let events: Vec<String> = common::scripted_events(answer)
            .iter()
            .map(Value::to_string)
            .collect();
        std::fs::write(fake.join("script.jsonl"), events.join("\n") + "\n").unwrap();
        let f = fake.display();
        // The loop ends by itself after a minute, so a failed test leaves no process behind.
        let script = format!(
            "#!/bin/sh\necho $$ > '{f}/pid'\ncat '{f}/script.jsonl'\nn=0\n\
while [ ! -f '{f}/release' ] && [ $n -lt 1200 ]; do sleep 0.05; n=$((n+1)); done\n"
        );
        let bin = fake.join("fake-pi");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = json!({ "piBin": bin });
        std::fs::write(agent.join("ext/subagents.json"), config.to_string()).unwrap();
        Setup {
            agent,
            cwd,
            fake,
            entries: Arc::default(),
            root,
        }
    }

    fn ext(&self, session: &str) -> Ext {
        Ext::start(self, session)
    }

    fn child_pid(&self) -> u32 {
        let file = self.fake.join("pid");
        wait_until(Duration::from_secs(10), || file.exists());
        wait_until(Duration::from_secs(2), || {
            read(&file).trim().parse::<u32>().is_ok()
        });
        read(&file).trim().parse().unwrap()
    }

    fn release(&self) {
        std::fs::write(self.fake.join("release"), "").unwrap();
    }
}

impl Drop for Setup {
    /// Ends the fake child and lets its runner finish before the temp dir goes.
    fn drop(&mut self) {
        let _ = std::fs::write(self.fake.join("release"), "");
        if let Ok(pid) = read(&self.fake.join("pid")).trim().parse::<u32>() {
            wait_until(Duration::from_secs(3), || !alive(pid));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// One `pi-rust-ext subagents` process with a scripted pi on the other side: it answers
/// `session.getEntries` from the shared entries, saves each `session.sendMessage` there as pi
/// does when it is idle, and says pi is idle.
struct Ext {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    next_id: u64,
    ctx: Value,
    entries: Arc<Mutex<Vec<Value>>>,
    /// The `subagent-notify` messages this process sent.
    notifies: Vec<Value>,
}

impl Ext {
    fn start(setup: &Setup, session: &str) -> Ext {
        let mut child = Command::new(binary())
            .arg("subagents")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
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
        let session_file = setup.root.path().join(format!("{session}.jsonl"));
        let ctx = json!({ "cwd": setup.cwd, "hasUI": false, "flags": {}, "isProjectTrusted": true,
            "model": { "provider": "mock", "id": "parent-model" }, "thinkingLevel": "high",
            "sessionId": session, "sessionFile": session_file });
        let mut ext = Ext {
            child,
            stdin,
            lines,
            next_id: 1,
            ctx,
            entries: setup.entries.clone(),
            notifies: Vec::new(),
        };
        let init = json!({ "protocolVersion": 1, "piVersion": "0.12.16", "cwd": setup.cwd,
            "agentDir": setup.agent, "extensionPath": "native:test/subagents", "platform": "linux",
            "mode": "print", "hasUI": false, "flags": {} });
        ext.call("initialize", init);
        ext.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
        let start = json!({ "name": "session_start", "event": { "type": "session_start",
            "reason": "resume" }, "ctx": ext.ctx });
        ext.call("event", start);
        ext
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").unwrap();
    }

    /// Sends a request and answers the extension's requests until the response arrives.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let Ok(message) = self.lines.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };
            if message.get("method").is_none() && message["id"] == id {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
            self.answer(&message);
        }
        panic!("no answer to {method}");
    }

    fn tool(&mut self, params: Value) -> Value {
        let call = json!({ "toolCallId": format!("t{}", self.next_id), "name": "subagent",
            "params": params, "ctx": self.ctx });
        self.call("tool.execute", call)
    }

    fn answer(&mut self, message: &Value) {
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let params = &message["params"];
        let result = match message["method"].as_str().unwrap_or("") {
            "session.getEntries" => json!(*self.entries.lock().unwrap()),
            "session.sendMessage" => {
                if params["customType"] == "subagent-notify" {
                    self.notifies.push(params.clone());
                }
                let mut entries = self.entries.lock().unwrap();
                let entry = json!({ "type": "custom_message", "customType": params["customType"],
                    "content": params["content"], "display": true, "details": params["details"],
                    "id": format!("e{}", entries.len()) });
                entries.push(entry);
                Value::Null
            }
            "ctx.isIdle" => json!(true),
            _ => Value::Null,
        };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    /// Answers requests until `done` holds; `false` after `timeout`.
    fn pump_until(&mut self, timeout: Duration, done: impl Fn(&Ext) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done(self) {
                return true;
            }
            if let Ok(message) = self.lines.recv_timeout(Duration::from_millis(50)) {
                self.answer(&message);
            }
        }
        done(self)
    }

    fn pump(&mut self, time: Duration) {
        self.pump_until(time, |_| false);
    }

    /// Closes stdin and waits for the process to exit.
    fn quit(mut self) {
        drop(self.stdin.take());
        wait_until(Duration::from_secs(5), || {
            self.child.try_wait().is_ok_and(|exit| exit.is_some())
        });
    }

    /// The run as `action: status` shows it, once this process has adopted it.
    fn adopted(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = self.tool(json!({ "action": "status", "id": id }));
            let run = status["details"]["runs"][0].clone();
            if !run.is_null() || Instant::now() > deadline {
                return run;
            }
            self.pump(Duration::from_millis(200));
        }
    }

    /// Stops the process the way Paseo stops pi: stdin EOF, SIGTERM to it and each descendant,
    /// then SIGKILL after 2 seconds. Returns the processes it signalled.
    fn paseo_stop(mut self) -> Vec<u32> {
        let tree = tree_of(self.pid());
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
}

impl Drop for Ext {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `pid` and its descendants, found by parent pid as the tree-kill package does.
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

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap_or(Value::Null)
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// Starts one background run and returns its id and async directory once its child runs.
fn start_call(ext: &mut Ext, setup: &Setup) -> (String, PathBuf, u32) {
    let result = ext.tool(json!({ "agent": "explore", "task": "Find main", "async": true }));
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("Started 1 background subagent run"),
        "{result}"
    );
    let details = &result["details"];
    let id = details["asyncId"].as_str().unwrap().to_owned();
    let dir = PathBuf::from(details["asyncDir"].as_str().unwrap());
    assert_eq!(dir, setup.agent.join("ext/subagents/runs").join(&id));
    assert_eq!(details["results"], json!([]));
    assert_eq!(details["runs"][0]["status"], "running");
    let child = setup.child_pid();
    let state = dir.join("run.json");
    let started = ext.pump_until(Duration::from_secs(10), |_| {
        json_of(&state)["runs"][0]["progress"]["settled"] == true
    });
    assert!(started, "{}", read(&state));
    (id, dir, child)
}

fn first_line(notify: &Value) -> &str {
    let content = notify["content"].as_str().unwrap_or_default();
    content.lines().next().unwrap_or_default()
}

#[test]
fn a_paseo_stop_leaves_the_run_going_and_the_resumed_session_gets_one_notify() {
    let setup = Setup::new("Found main");
    let mut first = setup.ext(SESSION);
    let (id, dir, child) = start_call(&mut first, &setup);
    let runner = json_of(&dir.join("run.json"))["pid"].as_u64().unwrap() as u32;
    let stopped = first.paseo_stop();
    assert!(
        !stopped.contains(&runner) && !stopped.contains(&child),
        "{stopped:?}"
    );
    assert!(alive(child), "the child ended with pi");
    assert!(alive(runner), "the runner ended with pi");
    assert_eq!(json_of(&dir.join("status.json"))["state"], "running");

    let mut second = setup.ext(SESSION);
    let run = second.adopted(&id);
    assert_eq!(run["status"], "running", "{run}");
    assert_eq!(run["output"], "Found main", "{run}");
    setup.release();
    let notified = second.pump_until(Duration::from_secs(15), |ext| !ext.notifies.is_empty());
    assert!(notified, "no notify after the resume");
    let notify = second.notifies[0].clone();
    assert_eq!(
        first_line(&notify),
        format!("Background task completed: **explore** ({id})")
    );
    assert!(notify["content"].as_str().unwrap().contains("Found main"));
    assert_eq!(notify["details"]["runs"][0]["id"], id.as_str());
    assert_eq!(notify["deliverAs"], "followUp");
    let done = dir.join("notify.done");
    assert!(second.pump_until(Duration::from_secs(5), |_| done.exists()));
    second.pump(Duration::from_secs(2));
    assert_eq!(second.notifies.len(), 1, "{:?}", second.notifies);
    let status = json_of(&dir.join("status.json"));
    assert_eq!(status["state"], "complete", "{status}");
    assert_eq!(status["steps"][0]["status"], "completed");
    assert!(dir.join("result.json").exists() && dir.join("events.jsonl").exists());
    second.quit();

    let mut third = setup.ext(SESSION);
    third.pump(Duration::from_secs(3));
    assert!(third.notifies.is_empty(), "{:?}", third.notifies);
    assert_eq!(setup.entries.lock().unwrap().len(), 1);
}

#[test]
fn another_session_adopts_nothing_and_a_saved_notify_is_not_sent_again() {
    let setup = Setup::new("Found main");
    let mut first = setup.ext(SESSION);
    let (id, dir, _) = start_call(&mut first, &setup);
    first.paseo_stop();
    setup.release();
    assert!(wait_until(Duration::from_secs(10), || dir
        .join("result.json")
        .exists()));

    let mut other = setup.ext("session-2");
    other.pump(Duration::from_secs(3));
    assert!(other.notifies.is_empty());
    let status = other.tool(json!({ "action": "status" }));
    assert_eq!(
        status["content"][0]["text"], "No subagent runs.",
        "{status}"
    );
    other.quit();
    assert!(!dir.join("notify.done").exists());

    // The notify reached the session, but its process ended before it wrote `notify.done`.
    let saved = json!({ "type": "custom_message", "customType": "subagent-notify",
        "content": "Background task completed", "display": true,
        "details": { "runs": [{ "id": id }] }, "id": "e0" });
    setup.entries.lock().unwrap().push(saved);
    let mut second = setup.ext(SESSION);
    let done = dir.join("notify.done");
    assert!(second.pump_until(Duration::from_secs(10), |_| done.exists()));
    second.pump(Duration::from_secs(1));
    assert!(second.notifies.is_empty(), "{:?}", second.notifies);
}

#[test]
fn stopping_an_adopted_run_ends_its_child_and_reports_stopped() {
    let setup = Setup::new("never");
    let mut first = setup.ext(SESSION);
    let (id, dir, child) = start_call(&mut first, &setup);
    first.paseo_stop();
    assert!(alive(child));

    let mut second = setup.ext(SESSION);
    assert_eq!(second.adopted(&id)["status"], "running");
    let stop = second.tool(json!({ "action": "stop", "id": id }));
    assert_eq!(
        stop["content"][0]["text"],
        format!("Stopping run {id}."),
        "{stop}"
    );
    let notified = second.pump_until(Duration::from_secs(15), |ext| !ext.notifies.is_empty());
    assert!(notified, "no notify after the stop");
    assert_eq!(
        first_line(&second.notifies[0]),
        format!("Background task stopped: **explore** ({id})")
    );
    assert!(wait_until(Duration::from_secs(5), || !alive(child)));
    assert!(dir.join(format!("commands/stop-{id}")).exists());
    assert_eq!(json_of(&dir.join("status.json"))["state"], "stopped");
    second.pump(Duration::from_secs(1));
    assert_eq!(second.notifies.len(), 1);
}

#[test]
fn a_killed_runner_gives_one_failed_notify() {
    let setup = Setup::new("never");
    let mut ext = setup.ext(SESSION);
    let (id, dir, child) = start_call(&mut ext, &setup);
    let state = dir.join("run.json");
    let runner = json_of(&state)["pid"].as_u64().unwrap() as u32;
    signal("-KILL", runner);
    std::thread::sleep(Duration::from_millis(500));
    // The runner is gone, so nothing rewrites its last heartbeat; date it a minute back.
    let mut report = json_of(&state);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    report["heartbeatMs"] = json!(now - 60_000);
    std::fs::write(&state, report.to_string()).unwrap();

    let notified = ext.pump_until(Duration::from_secs(20), |ext| !ext.notifies.is_empty());
    assert!(notified, "no notify for the stopped runner");
    let notify = &ext.notifies[0];
    assert_eq!(
        first_line(notify),
        format!("Background task failed: **explore** ({id})")
    );
    let content = notify["content"].as_str().unwrap();
    assert!(
        content.contains("the background runner stopped"),
        "{content}"
    );
    ext.pump(Duration::from_secs(2));
    assert_eq!(ext.notifies.len(), 1);
    assert_eq!(json_of(&dir.join("status.json"))["state"], "failed");
    assert!(alive(child), "the runner's child runs on until it ends");
}

#[test]
fn a_parallel_call_runs_its_tasks_in_one_runner_and_notifies_once() {
    let setup = Setup::new("Found main");
    let config = json!({ "piBin": setup.fake.join("fake-pi"), "maxParallel": 2 });
    std::fs::write(setup.agent.join("ext/subagents.json"), config.to_string()).unwrap();
    let mut ext = setup.ext(SESSION);
    let tasks =
        json!([{ "task": "one" }, { "agent": "build", "task": "two" }, { "task": "three" }]);
    let result = ext.tool(json!({ "tasks": tasks, "async": true }));
    let details = &result["details"];
    assert_eq!(details["mode"], "parallel", "{result}");
    let ids: Vec<String> = details["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(details["asyncId"], ids[0].as_str());
    let dir = PathBuf::from(details["asyncDir"].as_str().unwrap());
    let state = dir.join("run.json");
    let two_started = ext.pump_until(Duration::from_secs(10), |_| {
        let runs = json_of(&state)["runs"].clone();
        let settled = |i: usize| runs[i]["progress"]["settled"] == true;
        settled(0) && settled(1)
    });
    assert!(two_started, "{}", read(&state));
    assert_eq!(
        json_of(&state)["runs"][2]["progress"]["turns"],
        0,
        "maxParallel is 2"
    );
    setup.release();
    let notified = ext.pump_until(Duration::from_secs(15), |ext| !ext.notifies.is_empty());
    assert!(notified, "no notify for the parallel call");
    assert_eq!(
        first_line(&ext.notifies[0]),
        "Background tasks completed (3):"
    );
    ext.pump(Duration::from_secs(1));
    assert_eq!(ext.notifies.len(), 1);
    let status = json_of(&dir.join("status.json"));
    assert_eq!(status["state"], "complete", "{status}");
    let steps: Vec<&str> = status["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["agent"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["explore", "build", "explore"]);
    for id in &ids[1..] {
        let run_dir = setup.agent.join("ext/subagents/runs").join(id);
        assert!(run_dir.join("events.jsonl").exists(), "{id}");
    }
}
