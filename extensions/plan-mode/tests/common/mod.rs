#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A temp directory removed on drop.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(label: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let name = format!("pi-plan-mode-{label}-{}-{id}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tool(name: &str) -> Value {
    json!({ "name": name, "description": name, "exposure": "direct",
        "sourceInfo": { "source": "builtin", "path": format!("builtin:{name}") } })
}

/// An in-memory pi host. Appended entries and sent custom messages go to `branch`.
pub struct Host {
    writer: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next_id: u64,
    pub calls: Vec<(String, Value)>,
    pub branch: Vec<Value>,
    pub ctx: Value,
    pub tools: Vec<Value>,
    pub active: Vec<String>,
    pub thinking: String,
    pub idle: bool,
    /// Answers of `ui.select` in order: an option prefix, or `None` to dismiss.
    pub selects: VecDeque<Option<String>>,
    pub inputs: VecDeque<Option<String>>,
    responses: HashMap<u64, Value>,
}

impl Host {
    pub async fn start(work: &Path, agent: &Path, branch: Vec<Value>) -> Self {
        Self::start_mode(work, agent, branch, "tui").await
    }

    pub async fn start_mode(work: &Path, agent: &Path, branch: Vec<Value>, mode: &str) -> Self {
        Self::start_full(work, agent, branch, mode, "startup").await
    }

    pub async fn start_full(
        work: &Path,
        agent: &Path,
        branch: Vec<Value>,
        mode: &str,
        reason: &str,
    ) -> Self {
        let (host_side, ext_side) = tokio::io::duplex(1024 * 1024);
        let (read, write) = tokio::io::split(ext_side);
        tokio::spawn(pi_ext_plan_mode::extension(agent).serve(read, write));
        let (read, writer) = tokio::io::split(host_side);
        let ctx = json!({
            "cwd": work, "mode": mode, "hasUI": mode != "json", "isIdle": true,
            "isProjectTrusted": true, "sessionId": "s1", "sessionFile": "/sessions/s1.jsonl",
            "flags": {}, "model": { "provider": "mock", "id": "m1" }, "thinkingLevel": "medium"
        });
        let names = [
            "read",
            "bash",
            "edit",
            "write",
            "plan_mode_question",
            "plan_mode_complete",
        ];
        let mut host = Self {
            writer,
            lines: BufReader::new(read).lines(),
            next_id: 0,
            calls: Vec::new(),
            branch,
            ctx,
            tools: names.iter().map(|n| tool(n)).collect(),
            active: names.iter().map(|n| (*n).to_owned()).collect(),
            thinking: "medium".into(),
            idle: true,
            selects: VecDeque::new(),
            inputs: VecDeque::new(),
            responses: HashMap::new(),
        };
        let init = json!({
            "protocolVersion": 1, "piVersion": "0.12.0", "cwd": work, "agentDir": agent,
            "extensionPath": "native:test/plan-mode", "platform": "darwin", "mode": mode,
            "hasUI": mode != "json", "flags": {}
        });
        let result = host.request("initialize", init).await;
        host.calls.push(("$initialize".into(), result));
        host.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }))
            .await;
        host.event(
            "session_start",
            json!({ "type": "session_start", "reason": reason }),
        )
        .await;
        host
    }

    pub fn init_result(&self) -> &Value {
        &self.calls[0].1
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .expect("extension timed out")
            .unwrap()
            .expect("extension closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    fn select(&mut self, params: &Value) -> Value {
        let Some(answer) = self.selects.pop_front().flatten() else {
            return Value::Null;
        };
        let options = params["options"].as_array().cloned().unwrap_or_default();
        let found = options
            .iter()
            .filter_map(Value::as_str)
            .find(|o| o.starts_with(&answer))
            .unwrap_or_else(|| panic!("no option starts with {answer:?}: {options:?}"));
        json!(found)
    }

    fn pi_call(&mut self, params: &Value) -> Value {
        let target = params["target"].as_str().unwrap_or("");
        let method = params["method"].as_str().unwrap_or("");
        match (target, method) {
            ("ctx.modelRegistry", "find") => {
                json!({ "provider": params["args"][0], "id": params["args"][1] })
            }
            ("ctx.modelRegistry", "getApiKeyAndHeaders") => json!({ "ok": true }),
            ("ctx.modelRegistry", "getAvailable") => json!([{ "provider": "mock", "id": "m1" }]),
            ("ctx.sessionManager", "getEntry") => {
                let id = &params["args"][0];
                self.branch
                    .iter()
                    .find(|e| e["id"] == *id)
                    .cloned()
                    .unwrap_or(Value::Null)
            }
            ("ctx.sessionManager", "getHeader") => {
                json!({ "type": "session", "parentSession": "/sessions/s1.jsonl" })
            }
            _ => Value::Null,
        }
    }

    fn answer_for(&mut self, method: &str, params: &Value) -> Value {
        match method {
            "session.appendEntry" => {
                self.branch.push(json!({
                    "type": "custom", "customType": params["customType"], "data": params["data"]
                }));
                Value::Null
            }
            "session.sendMessage" => {
                self.branch.push(json!({
                    "type": "custom_message", "id": format!("m{}", self.branch.len()),
                    "customType": params["customType"], "content": params["content"],
                    "display": params["display"], "details": params["details"]
                }));
                Value::Null
            }
            "session.getBranch" => Value::Array(self.branch.clone()),
            "tools.getAll" => Value::Array(self.tools.clone()),
            "tools.getActive" => json!(self.active),
            "ctx.isIdle" => json!(self.idle),
            "thinking.get" => json!(self.thinking),
            "thinking.set" => {
                self.thinking = params["level"].as_str().unwrap_or("").to_owned();
                Value::Null
            }
            "model.get" => json!({ "provider": "mock", "id": "m1" }),
            "ui.select" => self.select(params),
            "ui.input" => self
                .inputs
                .pop_front()
                .flatten()
                .map_or(Value::Null, Value::String),
            "pi.call" => self.pi_call(params),
            _ => Value::Null,
        }
    }

    async fn handle(&mut self, message: Value) -> Option<Value> {
        let Some(method) = message["method"].as_str().map(String::from) else {
            return Some(message);
        };
        self.calls.push((method.clone(), message["params"].clone()));
        if message.get("id").is_some() {
            let result = self.answer_for(&method, &message["params"]);
            let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
            self.send(reply).await;
        }
        None
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(request).await;
        let response = loop {
            if let Some(response) = self.responses.remove(&id) {
                break response;
            }
            let message = self.next().await;
            if let Some(response) = self.handle(message).await {
                let id = response["id"].as_u64().unwrap_or_default();
                self.responses.insert(id, response);
            }
        };
        if let Some(error) = response.get("error") {
            return json!({ "$error": error });
        }
        response["result"].clone()
    }

    pub async fn event(&mut self, name: &str, event: Value) -> Value {
        let params = json!({ "name": name, "event": event, "ctx": self.ctx });
        self.request("event", params).await
    }

    /// Runs a tool and returns the result.
    pub async fn tool(&mut self, name: &str, id: &str, params: Value) -> Value {
        let call = json!({ "toolCallId": id, "name": name, "params": params, "ctx": self.ctx });
        self.request("tool.execute", call).await
    }

    pub async fn command(&mut self, args: &str) -> Value {
        let params = json!({ "name": "plan", "args": args, "ctx": self.ctx });
        self.request("command.run", params).await
    }

    /// The `tool_call` reply for `name` with `input`.
    pub async fn tool_call(&mut self, name: &str, input: Value) -> Value {
        let event =
            json!({ "type": "tool_call", "toolName": name, "toolCallId": "c1", "input": input });
        self.event("tool_call", event).await
    }

    /// Reads and answers messages until the extension calls `method`; returns its params.
    pub async fn wait_for(&mut self, method: &str) -> Value {
        loop {
            let message = self.next().await;
            let found = message["method"] == method;
            let params = message["params"].clone();
            if let Some(response) = self.handle(message).await {
                let id = response["id"].as_u64().unwrap_or_default();
                self.responses.insert(id, response);
            }
            if found {
                return params;
            }
        }
    }

    /// The params of call number `index` of `method`, waiting for it when needed.
    pub async fn nth_call(&mut self, method: &str, index: usize) -> Value {
        while self.called(method).len() <= index {
            self.wait_for(method).await;
        }
        self.called(method)[index].clone()
    }

    pub fn called(&self, method: &str) -> Vec<&Value> {
        let calls = self.calls.iter().filter(|(name, _)| name == method);
        calls.map(|(_, params)| params).collect()
    }

    pub fn last_status(&self) -> Value {
        self.called("ui.setStatus")
            .last()
            .map_or(Value::Null, |p| p["text"].clone())
    }

    pub fn state(&self) -> Value {
        self.branch
            .iter()
            .rev()
            .find(|e| e["customType"] == "plan-mode-state")
            .map_or(Value::Null, |e| e["data"].clone())
    }

    pub fn sent_custom(&self, custom_type: &str) -> Vec<&Value> {
        self.called("session.sendMessage")
            .into_iter()
            .filter(|p| p["customType"] == custom_type)
            .collect()
    }

    pub fn user_messages(&self) -> Vec<String> {
        self.called("session.sendUserMessage")
            .iter()
            .map(|p| p["content"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    pub fn notices(&self) -> Vec<String> {
        self.called("ui.notify")
            .iter()
            .map(|p| p["message"].as_str().unwrap_or("").to_owned())
            .collect()
    }
}
