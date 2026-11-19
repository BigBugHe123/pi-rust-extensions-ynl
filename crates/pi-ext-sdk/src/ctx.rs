use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::host::Host;
use crate::reply::ToolResult;
use crate::snapshot::Snapshot;

/// Shared parts of every handler context.
#[derive(Clone)]
pub(crate) struct Base {
    pub(crate) host: Host,
    pub(crate) snapshot: Arc<Snapshot>,
    pub(crate) cancel: CancellationToken,
}

macro_rules! base_accessors {
    () => {
        pub fn host(&self) -> &Host {
            &self.base.host
        }

        pub fn snapshot(&self) -> &Snapshot {
            &self.base.snapshot
        }

        /// Fires when pi sends `$/cancelRequest` for this request.
        pub fn cancel_token(&self) -> &CancellationToken {
            &self.base.cancel
        }

        pub fn is_cancelled(&self) -> bool {
            self.base.cancel.is_cancelled()
        }

        pub async fn cancelled(&self) {
            self.base.cancel.cancelled().await
        }
    };
}

/// Context of a `tool.execute` request.
#[derive(Clone)]
pub struct ToolCtx {
    pub(crate) base: Base,
    pub(crate) tool_call_id: String,
}

impl ToolCtx {
    base_accessors!();

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// Sends a `tool.update` with partial content.
    pub fn update(&self, partial: impl Into<ToolResult>) -> Result<()> {
        let partial = partial.into().partial_json();
        self.base.host.notify(
            "tool.update",
            json!({ "toolCallId": self.tool_call_id, "partial": partial }),
        )
    }
}

/// Context of an `event` request.
#[derive(Clone)]
pub struct EventCtx {
    pub(crate) base: Base,
    pub(crate) name: String,
    pub(crate) event: Arc<Value>,
    /// The `eventId` of a `turn_end` or `agent_before_settle` request, for [`Self::context`].
    pub(crate) event_id: Option<Value>,
}

impl EventCtx {
    base_accessors!();

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The event object; it includes the patches of earlier handlers of this extension.
    pub fn event(&self) -> &Value {
        &self.event
    }

    pub fn event_as<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(self.event.as_ref()).map_err(Error::invalid_params)
    }

    /// The `context` of a `turn_end` or `agent_before_settle` event, a preview of the whole model
    /// context: the one the event carries, else asked from pi, which builds it only then (feature
    /// `contextEvents`). `None` for other events.
    pub async fn context(&self) -> Result<Option<Value>> {
        if let Some(context) = self.event.get("context") {
            return Ok(Some(context.clone()));
        }
        let Some(event_id) = &self.event_id else {
            return Ok(None);
        };
        let params = serde_json::json!({ "eventId": event_id });
        self.host().call("event.context", params).await.map(Some)
    }

    /// Reads one field of the event, for example `ev.get("toolName")`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.event.get(key)
    }
}

/// Context of a `command.run` or `shortcut.run` request.
#[derive(Clone)]
pub struct CommandCtx {
    pub(crate) base: Base,
}

impl CommandCtx {
    base_accessors!();
}
