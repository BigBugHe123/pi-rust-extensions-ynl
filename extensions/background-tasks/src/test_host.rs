use std::time::Duration;

use pi_ext_sdk::prelude::*;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

use crate::extension;

/// Answers a request of the extension from its method and params.
pub type Answer = Box<dyn FnMut(&str, &Value) -> Value + Send>;

pub struct Host {
    writer: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    pub requests: Vec<Value>,
    /// Answers the extension's requests; `{}` unless a test sets it.
    pub answer: Answer,
}

impl Host {
    pub fn start() -> Self {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(server);
        tokio::spawn(extension().serve(read, write));
        let (read, writer) = tokio::io::split(client);
        Self {
            writer,
            lines: BufReader::new(read).lines(),
            requests: Vec::new(),
            answer: Box::new(|_, _| json!({})),
        }
    }

    /// Records and answers a request of the extension; ignores anything else.
    async fn handle(&mut self, message: &Value) {
        if let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) {
            let result = (self.answer)(method, &message["params"]);
            let id = id.clone();
            self.requests.push(message.clone());
            self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
                .await;
        }
    }

    /// Answers requests until `done` holds; `false` after `limit`.
    pub async fn pump_until(&mut self, limit: Duration, done: impl Fn(&Host) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while !done(self) {
            let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
            if wait.is_zero() {
                return false;
            }
            let wait = wait.min(Duration::from_millis(100));
            if let Ok(Ok(Some(line))) = tokio::time::timeout(wait, self.lines.next_line()).await {
                let message: Value = serde_json::from_str(&line).unwrap();
                self.handle(&message).await;
            }
        }
        true
    }

    /// The requests of the extension for `method` so far.
    pub fn count(&self, method: &str) -> usize {
        self.requests
            .iter()
            .filter(|r| r["method"] == method)
            .count()
    }

    pub async fn send(&mut self, message: Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    pub async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("timed out waiting for the extension")
            .unwrap()
            .expect("extension closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    /// Answers extension requests (recording them) until `method` arrives.
    pub async fn wait_request(&mut self, method: &str) -> Value {
        loop {
            let message = self.recv().await;
            self.handle(&message).await;
            if message["method"] == method && message.get("id").is_some() {
                return message["params"].clone();
            }
        }
    }

    /// Sends a request and answers extension requests until its response arrives.
    pub async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        loop {
            let message = self.recv().await;
            if message.get("method").is_none() && message["id"] == id {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
            self.handle(&message).await;
        }
    }

    pub async fn tool(&mut self, id: u64, ctx: &Value, name: &str, params: Value) -> String {
        let params = json!({"name":name,"params":params,"toolCallId":format!("c{id}"),"ctx":ctx});
        let result = self.call(id, "tool.execute", params).await;
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}
