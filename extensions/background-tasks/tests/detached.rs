//! Tasks that the `background-tasks` binary starts through `--launch`: a pi stop the way Paseo
//! does it (stdin EOF, then SIGTERM and SIGKILL to every descendant found by parent pid), a new
//! extension process for the same session, and exactly one `background-task-notification` per
//! task. A scripted pi answers the extension.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pi_ext_e2e::{TempDir, cargo_example};
use serde_json::{Value, json};

const SESSION: &str = "session-1";
const NOTIFICATION: &str = "background-task-notification";

fn binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| cargo_example("pi-ext-background-tasks", "background-tasks"))
}

/// Temp dirs and the session entries that every extension process of a test shares.
struct Setup {
    root: TempDir,
    agent: PathBuf,
    cwd: PathBuf,
    entries: Arc<Mutex<Vec<Value>>>,
}

impl Setup {
    fn new() -> Setup {
        let root = TempDir::new().unwrap();
        let [agent, cwd] = ["agent", "work"].map(|name| root.path().join(name));
        for dir in [&agent, &cwd] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Setup {
            root,
            agent,
            cwd,
            entries: Arc::default(),
        }
    }

    /// A command that runs until [`Setup::release`], at most a minute, then runs `then`.
    fn gated(&self, then: &str) -> String {
        let release = self.root.path().join("release");
        format!(
            "n=0; while [ ! -f '{}' ] && [ $n -lt 1200 ]; do sleep 0.05; n=$((n+1)); done; {then}",
            release.display()
        )
    }

    fn release(&self) {
        std::fs::write(self.root.path().join("release"), "").unwrap();
    }

    fn task_file(&self, id: &str) -> PathBuf {
        self.agent
            .join("ext/background-tasks/sessions")
            .join(SESSION)
            .join(id)
            .join("task.json")
    }

    fn ext(&self, mode: &str, pi: Pi) -> Ext {
        Ext::start(self, mode, pi)
    }
}

impl Drop for Setup {
    /// Ends the gated tasks before the temp dir goes.
    fn drop(&mut self) {
        self.release();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// How the scripted pi answers: idle or in a turn, and whether it saves the messages it takes
/// (in a turn it only queues them, and a stop loses the queue).
#[derive(Clone, Copy)]
struct Pi {
    idle: bool,
    saves: bool,
}

const IDLE: Pi = Pi {
    idle: true,
    saves: true,
};
const BUSY: Pi = Pi {
    idle: false,
    saves: false,
};

/// One extension process with a scripted pi on the other side.
struct Ext {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    next_id: u64,
    ctx: Value,
    pi: Pi,
    entries: Arc<Mutex<Vec<Value>>>,
    /// The notifications this process sent.
    notices: Vec<Value>,
}

impl Ext {
    fn start(setup: &Setup, mode: &str, pi: Pi) -> Ext {
        let mut child = Command::new(binary())
            .arg("background-tasks")
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
        let session_file = setup.root.path().join(format!("{SESSION}.jsonl"));
        let ctx = json!({ "cwd": setup.cwd, "hasUI": false, "flags": {}, "sessionId": SESSION,
            "sessionFile": session_file, "mode": mode });
        let mut ext = Ext {
            child,
            stdin,
            lines,
            next_id: 1,
            ctx,
            pi,
            entries: setup.entries.clone(),
            notices: Vec::new(),
        };
        let init = json!({ "protocolVersion": 1, "piVersion": "0.12.18", "cwd": setup.cwd,
            "agentDir": setup.agent, "extensionPath": "native:test/background-tasks",
            "platform": std::env::consts::OS, "mode": mode, "hasUI": false, "flags": {} });
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

    fn tool(&mut self, name: &str, params: Value) -> Value {
        let call = json!({ "toolCallId": format!("t{}", self.next_id), "name": name,
            "params": params, "ctx": self.ctx });
        self.call("tool.execute", call)
    }

    /// `session_shutdown` as pi sends it before it stops the extension.
    fn shutdown(&mut self, reason: &str) {
        let event = json!({ "name": "session_shutdown", "event": { "type": "session_shutdown",
            "reason": reason }, "ctx": self.ctx });
        self.call("event", event);
    }

    fn answer(&mut self, message: &Value) {
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let params = &message["params"];
        let result = match message["method"].as_str().unwrap_or("") {
            "session.getEntries" => json!(*self.entries.lock().unwrap()),
            "session.sendMessage" => {
                if params["customType"] == NOTIFICATION {
                    self.notices.push(params.clone());
                }
                if self.pi.saves {
                    let mut entries = self.entries.lock().unwrap();
                    let entry = json!({ "type": "custom_message", "customType": params["customType"],
                        "content": params["content"], "display": true, "details": params["details"],
                        "id": format!("e{}", entries.len()) });
                    entries.push(entry);
                }
                Value::Null
            }
            "ctx.isIdle" => json!(self.pi.idle),
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

/// One `ps` column of `pid`; empty when there is no such process.
fn ps(column: &str, pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", &format!("{column}="), "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A process that exists and is not a zombie.
fn alive(pid: u32) -> bool {
    let state = ps("stat", pid);
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
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

/// Starts a task and returns its id and pid.
fn run(ext: &mut Ext, name: &str, command: &str) -> (String, u32) {
    let result = ext.tool(
        "bg_run",
        json!({ "name": name, "command": command, "isAgent": false }),
    );
    assert!(
        text(&result).starts_with(&format!("Started background task {name}")),
        "{result}"
    );
    let task = &result["details"];
    let pid = task["pid"].as_u64().unwrap() as u32;
    (task["id"].as_str().unwrap().to_owned(), pid)
}

#[test]
fn a_paseo_stop_leaves_the_task_running_and_the_next_pi_notifies_once() {
    let setup = Setup::new();
    let mut first = setup.ext("rpc", IDLE);
    let (id, pid) = run(&mut first, "Gated", &setup.gated("echo released"));
    let extension = first.pid();
    assert!(alive(pid));
    let parent: u32 = ps("ppid", pid).parse().unwrap();
    assert_ne!(parent, extension, "the task is a child of the extension");
    assert_eq!(ps("pgid", pid), pid.to_string(), "own process group");
    let record = read_json(&setup.task_file(&id));
    assert_eq!(record["processStart"], ps("lstart", pid), "{record}");

    // pi sends `session_shutdown` (quit) on stdin EOF; Paseo then kills pi's process tree.
    first.shutdown("quit");
    let stopped = first.paseo_stop();
    assert!(stopped.contains(&extension), "{stopped:?}");
    assert!(!stopped.contains(&pid), "{stopped:?}");
    assert!(alive(pid), "the task ended with pi");
    assert_eq!(read_json(&setup.task_file(&id))["status"], "running");

    let mut second = setup.ext("rpc", IDLE);
    let status = second.tool("bg_status", json!({ "taskId": id }));
    assert!(text(&status).contains("Gated [running]"), "{status}");
    setup.release();
    let notified = second.pump_until(Duration::from_secs(15), |ext| !ext.notices.is_empty());
    assert!(notified, "no notification after the restart");
    let notice = second.notices[0].clone();
    assert_eq!(notice["details"]["id"], id.as_str());
    assert_eq!(notice["details"]["status"], "completed");
    assert_eq!(notice["deliverAs"], "followUp");
    assert_eq!(notice["triggerTurn"], true);
    assert!(notice["content"].as_str().unwrap().contains("released"));
    let file = setup.task_file(&id);
    let confirmed = second.pump_until(Duration::from_secs(10), |_| {
        read_json(&file)["notified"] == true
    });
    assert!(confirmed, "{}", read_json(&file));
    second.pump(Duration::from_secs(1));
    assert_eq!(second.notices.len(), 1, "{:?}", second.notices);
    second.quit();

    let mut third = setup.ext("rpc", IDLE);
    third.pump(Duration::from_secs(4));
    assert!(third.notices.is_empty(), "{:?}", third.notices);
    assert_eq!(setup.entries.lock().unwrap().len(), 1);
}

#[test]
fn a_task_that_does_not_survive_reloads_ends_with_a_paseo_stop() {
    let setup = Setup::new();
    let mut first = setup.ext("rpc", IDLE);
    let result = first.tool(
        "bg_run",
        json!({ "name": "Local", "command": setup.gated("echo late"), "isAgent": false, "surviveReload": false }),
    );
    let pid = result["details"]["pid"].as_u64().unwrap() as u32;
    let extension = first.pid();
    let parent: u32 = ps("ppid", pid).parse().unwrap();
    assert_eq!(
        parent, extension,
        "started in the extension, not through the launcher"
    );
    // The tree kill comes before `session_shutdown` reaches the extension: the task still ends.
    let stopped = first.paseo_stop();
    assert!(stopped.contains(&pid), "{stopped:?}");
    assert!(
        wait_until(Duration::from_secs(5), || !alive(pid)),
        "the task outlived pi"
    );
}

#[test]
fn a_notification_lost_with_a_stopped_pi_is_sent_again_once() {
    let setup = Setup::new();
    // pi runs a turn, so it only queues the notification, and the stop loses it.
    let mut first = setup.ext("rpc", BUSY);
    let (id, _) = run(&mut first, "Quick", "echo quick");
    assert!(first.pump_until(Duration::from_secs(10), |ext| ext.notices.len() == 1));
    first.paseo_stop();
    let file = setup.task_file(&id);
    let record = read_json(&file);
    assert!(record["noticeSentAt"].is_u64(), "{record}");
    assert_eq!(record["notified"], false);
    assert!(setup.entries.lock().unwrap().is_empty());

    let mut second = setup.ext("rpc", IDLE);
    // The first check finds it missing; it goes out again once pi was idle for 10 seconds.
    second.pump(Duration::from_secs(5));
    assert!(second.notices.is_empty(), "sent again too early");
    let resent = second.pump_until(Duration::from_secs(15), |ext| !ext.notices.is_empty());
    assert!(resent, "the lost notification was not sent again");
    assert_eq!(second.notices[0]["details"]["id"], id.as_str());
    let confirmed = second.pump_until(Duration::from_secs(10), |_| {
        read_json(&file)["notified"] == true
    });
    assert!(confirmed, "{}", read_json(&file));
    second.pump(Duration::from_secs(1));
    assert_eq!(second.notices.len(), 1);
    assert_eq!(setup.entries.lock().unwrap().len(), 1);
}

#[test]
fn a_notification_saved_before_its_process_ended_is_not_sent_again() {
    let setup = Setup::new();
    let mut first = setup.ext("rpc", IDLE);
    let (id, _) = run(&mut first, "Quick", "echo quick");
    // pi saved the notification, but the process ends before it confirms it in the session.
    assert!(first.pump_until(Duration::from_secs(10), |ext| ext.notices.len() == 1));
    first.paseo_stop();
    let file = setup.task_file(&id);
    let record = read_json(&file);
    assert!(record["noticeSentAt"].is_u64(), "{record}");
    assert_eq!(record["notified"], false);
    assert_eq!(setup.entries.lock().unwrap().len(), 1);

    let mut second = setup.ext("rpc", IDLE);
    let confirmed = second.pump_until(Duration::from_secs(10), |_| {
        read_json(&file)["notified"] == true
    });
    assert!(confirmed, "{}", read_json(&file));
    second.pump(Duration::from_secs(1));
    assert!(second.notices.is_empty(), "{:?}", second.notices);
    assert_eq!(setup.entries.lock().unwrap().len(), 1);
}

#[test]
fn quit_outside_rpc_mode_and_bg_kill_stop_launched_tasks() {
    let setup = Setup::new();
    let mut ext = setup.ext("tui", IDLE);
    let (killed, killed_pid) = run(&mut ext, "Killed", &setup.gated("true"));
    let (quit, quit_pid) = run(&mut ext, "Quit", &setup.gated("true"));
    for pid in [killed_pid, quit_pid] {
        let parent: u32 = ps("ppid", pid).parse().unwrap();
        assert_ne!(parent, ext.pid());
    }
    let result = ext.tool("bg_kill", json!({ "taskId": killed }));
    assert!(
        text(&result).starts_with("Killed background task Killed"),
        "{result}"
    );
    assert!(wait_until(Duration::from_secs(5), || !alive(killed_pid)));

    ext.shutdown("quit");
    assert!(wait_until(Duration::from_secs(5), || !alive(quit_pid)));
    let record = read_json(&setup.task_file(&quit));
    assert_eq!(record["status"], "killed", "{record}");
    assert_eq!(record["error"], "stopped on session quit");
    ext.pump(Duration::from_secs(1));
    assert!(ext.notices.is_empty(), "{:?}", ext.notices);
}

#[test]
fn the_launcher_answers_with_the_pid_and_exits_before_the_task() {
    let setup = Setup::new();
    let dir = setup.root.path().join("task");
    std::fs::create_dir_all(&dir).unwrap();
    let started = Instant::now();
    let output = Command::new(binary())
        .args(["background-tasks", "--launch"])
        .arg(&dir)
        .arg(&setup.cwd)
        .arg(setup.gated("echo out; echo err >&2; exit 3"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    let pid = answer["pid"].as_u64().unwrap() as u32;
    assert!(alive(pid), "the task ended with the launcher");
    assert_eq!(answer["processStart"], ps("lstart", pid));
    setup.release();
    let exit = dir.join("exit");
    assert!(wait_until(Duration::from_secs(10), || exit.exists()));
    assert_eq!(std::fs::read_to_string(exit).unwrap(), "3");
    let log = std::fs::read_to_string(dir.join("output.log")).unwrap();
    assert!(log.contains("out\n") && log.contains("err\n"), "{log}");

    let output = Command::new(binary())
        .args(["background-tasks", "--launch"])
        .arg(&dir)
        .arg(setup.root.path().join("missing"))
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        answer["error"].as_str().unwrap().contains("cannot start"),
        "{answer}"
    );
}
