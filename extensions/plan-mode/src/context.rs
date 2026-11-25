//! The `context` transforms: plan artifacts after the workflow, the active implementation plan,
//! and anchored interaction contracts (upstream `implementation-retention.ts`,
//! `message-transform.ts`, `interaction-coordinator.ts`).

use serde_json::Value;

use crate::anchor;
use crate::complete::strip_proposed_plan_blocks;
use crate::contracts::{
    HISTORY_IMPLEMENTATION_PROMPT, IMPLEMENTATION_CONTEXT_TYPE, IMPLEMENTATION_HANDOFF_PREFIX,
    Interaction, LEGACY_CONTEXT_TYPE, PROPOSED_PLAN_TYPE, active_implementation_context,
    content_text, context_message, custom_type, implementation_handoff,
    interaction_contract_content, interaction_contract_details, interaction_contract_of,
    latest_interaction_contract, role, unwrap,
};
use crate::state::{ActiveImplementation, COMPLETE_TOOL, InteractionAnchor, PlanState};

fn is_summary(message: &Value) -> bool {
    matches!(role(message), Some("compactionSummary" | "branchSummary"))
}

fn user_text(message: &Value) -> Option<String> {
    (role(message) == Some("user")).then(|| content_text(unwrap(message).get("content")))
}

fn is_handoff(message: &Value) -> bool {
    user_text(message).is_some_and(|t| t.trim_start().starts_with(IMPLEMENTATION_HANDOFF_PREFIX))
}

fn is_exact_handoff(message: &Value, plan: &str) -> bool {
    user_text(message).is_some_and(|t| t.trim() == implementation_handoff(plan).trim())
}

fn is_implementation_context(message: &Value) -> bool {
    custom_type(message) == Some(IMPLEMENTATION_CONTEXT_TYPE)
}

fn is_completion_result(message: &Value) -> bool {
    let m = unwrap(message);
    m.get("role").and_then(Value::as_str) == Some("toolResult")
        && m.get("toolName").and_then(Value::as_str) == Some(COMPLETE_TOOL)
}

fn is_inactive_artifact(message: &Value) -> bool {
    custom_type(message) == Some(PROPOSED_PLAN_TYPE) || is_completion_result(message)
}

fn has_proposed_block(text: &str) -> bool {
    strip_proposed_plan_blocks(text) != text
}

enum ArtifactKind {
    Completion { call_index: usize, call_id: String },
    Legacy,
    Presentation,
}

/// The plan artifact that the latest `Implement the plan.` kickoff refers to.
fn history_artifact(messages: &[Value]) -> Option<(usize, ArtifactKind)> {
    for kickoff in (0..messages.len()).rev() {
        if user_text(&messages[kickoff]).is_none_or(|t| t.trim() != HISTORY_IMPLEMENTATION_PROMPT) {
            continue;
        }
        if let Some(found) = artifact_before(messages, kickoff) {
            return Some(found);
        }
    }
    None
}

fn artifact_before(messages: &[Value], kickoff: usize) -> Option<(usize, ArtifactKind)> {
    for index in (0..kickoff).rev() {
        let message = &messages[index];
        if role(message) == Some("user") || is_summary(message) {
            return None;
        }
        if is_completion_result(message) {
            let call_id = unwrap(message).get("toolCallId").and_then(Value::as_str);
            if let Some(call_id) = call_id
                && let Some(call_index) = completion_call_index(messages, index, call_id)
            {
                let call_id = call_id.to_owned();
                return Some((
                    index,
                    ArtifactKind::Completion {
                        call_index,
                        call_id,
                    },
                ));
            }
            continue;
        }
        if custom_type(message) == Some(PROPOSED_PLAN_TYPE) {
            return Some((index, ArtifactKind::Presentation));
        }
        if role(message) == Some("assistant")
            && has_proposed_block(&content_text(unwrap(message).get("content")))
        {
            return Some((index, ArtifactKind::Legacy));
        }
    }
    None
}

fn completion_call_index(messages: &[Value], end: usize, call_id: &str) -> Option<usize> {
    for index in (0..end).rev() {
        let message = &messages[index];
        if role(message) == Some("user") || is_summary(message) {
            return None;
        }
        let calls = unwrap(message).get("content").and_then(Value::as_array);
        let found = calls.into_iter().flatten().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("toolCall")
                && block.get("id").and_then(Value::as_str) == Some(call_id)
                && block.get("name").and_then(Value::as_str) == Some(COMPLETE_TOOL)
        });
        if found {
            return Some(index);
        }
    }
    None
}

/// Rewrites the content of an assistant message, keeping the session-entry wrapper.
fn map_assistant_content(mut message: Value, map: impl FnOnce(&Value) -> Option<Value>) -> Value {
    if role(&message) != Some("assistant") {
        return message;
    }
    let Some(content) = unwrap(&message).get("content").and_then(map) else {
        return message;
    };
    let target = if message.get("message").is_some() {
        &mut message["message"]
    } else {
        &mut message
    };
    target["content"] = content;
    message
}

fn strip_blocks(content: &Value) -> Option<Value> {
    match content {
        Value::String(text) => {
            let stripped = strip_proposed_plan_blocks(text);
            (stripped != *text).then_some(Value::String(stripped))
        }
        Value::Array(blocks) => {
            let mut changed = false;
            let next: Vec<Value> = blocks
                .iter()
                .map(|block| {
                    let text = block.get("text").and_then(Value::as_str);
                    match text {
                        Some(text) if block.get("type").and_then(Value::as_str) == Some("text") => {
                            let stripped = strip_proposed_plan_blocks(text);
                            if stripped == text {
                                return block.clone();
                            }
                            changed = true;
                            let mut block = block.clone();
                            block["text"] = Value::String(stripped);
                            block
                        }
                        _ => block.clone(),
                    }
                })
                .collect();
            changed.then_some(Value::Array(next))
        }
        _ => None,
    }
}

fn strip_completion_calls(content: &Value, keep: Option<&str>) -> Option<Value> {
    let blocks = content.as_array()?;
    let next: Vec<Value> = blocks
        .iter()
        .filter(|block| {
            !(block.get("type").and_then(Value::as_str) == Some("toolCall")
                && block.get("name").and_then(Value::as_str) == Some(COMPLETE_TOOL)
                && keep.is_none_or(|id| block.get("id").and_then(Value::as_str) != Some(id)))
        })
        .cloned()
        .collect();
    (next.len() != blocks.len()).then_some(Value::Array(next))
}

fn is_empty_assistant(message: &Value) -> bool {
    role(message) == Some("assistant")
        && unwrap(message)
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

/// Keeps the current implementation context and handoff, drops stale copies, and inserts one
/// context message after the leading summaries when neither is present.
fn inject_active(messages: Vec<Value>, active: &ActiveImplementation) -> Vec<Value> {
    let expected = active_implementation_context(&active.plan);
    let (mut found_context, mut found_handoff) = (false, false);
    let mut kept: Vec<Value> = messages
        .into_iter()
        .filter(|message| {
            if is_implementation_context(message) {
                let current = !found_context
                    && unwrap(message).get("content").and_then(Value::as_str)
                        == Some(expected.as_str());
                found_context |= current;
                return current;
            }
            if !is_handoff(message) {
                return true;
            }
            let current = !found_handoff && is_exact_handoff(message, &active.plan);
            found_handoff |= current;
            current
        })
        .collect();
    if found_context || found_handoff {
        return kept;
    }
    let index = kept.iter().take_while(|m| is_summary(m)).count();
    let mut message = context_message(IMPLEMENTATION_CONTEXT_TYPE, &expected, None);
    message["timestamp"] = Value::from(active.started_at);
    kept.insert(index, message);
    kept
}

/// Tracks when a guaranteed-plan implementation has reached the model and when it settles.
#[derive(Debug, Default)]
pub struct Retention {
    delivered: Option<String>,
    restored_awaiting: Option<String>,
}

impl Retention {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn restore(&mut self, active: Option<&ActiveImplementation>) {
        self.restored_awaiting = active
            .filter(|a| a.retention != "keep")
            .map(|a| a.id.clone());
    }

    /// The model context and the id of an implementation to clear now.
    pub fn transform(
        &mut self,
        messages: Vec<Value>,
        state: &PlanState,
    ) -> (Vec<Value>, Option<String>) {
        let messages: Vec<Value> = messages
            .into_iter()
            .filter(|m| custom_type(m) != Some(LEGACY_CONTEXT_TYPE))
            .collect();
        let strip_implementation = |messages: Vec<Value>| -> Vec<Value> {
            messages
                .into_iter()
                .filter(|m| !is_implementation_context(m) && !is_handoff(m))
                .collect()
        };
        if state.enabled {
            return (strip_implementation(messages), None);
        }
        let active = state.active_implementation.as_ref();
        let inactive = if active.is_some() {
            messages
        } else {
            strip_implementation(messages)
        };
        let artifact = if active.is_some() {
            None
        } else {
            history_artifact(&inactive)
        };
        let (keep_index, keep_call) = match &artifact {
            Some((
                index,
                ArtifactKind::Completion {
                    call_index,
                    call_id,
                },
            )) => (Some(*index), Some((*call_index, call_id.clone()))),
            Some((index, _)) => (Some(*index), None),
            None => (None, None),
        };
        let legacy_kept = matches!(artifact, Some((_, ArtifactKind::Legacy)));
        let filtered: Vec<Value> = inactive
            .into_iter()
            .enumerate()
            .filter(|(i, m)| Some(*i) == keep_index || !is_inactive_artifact(m))
            .map(|(i, m)| {
                let m = if legacy_kept && Some(i) == keep_index {
                    m
                } else {
                    map_assistant_content(m, strip_blocks)
                };
                let keep = keep_call
                    .as_ref()
                    .filter(|(call_index, _)| *call_index == i)
                    .map(|(_, id)| id.clone());
                map_assistant_content(m, |content| {
                    strip_completion_calls(content, keep.as_deref())
                })
            })
            .filter(|m| !is_empty_assistant(m))
            .collect();
        let Some(active) = active else {
            return (filtered, None);
        };
        let delivered = self.restored_awaiting.as_deref() == Some(active.id.as_str())
            || filtered.iter().any(|m| is_exact_handoff(m, &active.plan));
        let contextual = inject_active(filtered, active);
        if !delivered {
            return (contextual, None);
        }
        self.restored_awaiting = None;
        if active.retention == "clear-after-first-run" {
            self.delivered = Some(active.id.clone());
        }
        let clear = (active.retention == "clear-on-start").then(|| active.id.clone());
        (contextual, clear)
    }

    /// The implementation to clear when its first run settles.
    pub fn settled(&mut self, active: Option<&ActiveImplementation>) -> Option<String> {
        let active = active.filter(|a| a.retention == "clear-after-first-run")?;
        if self.delivered.as_deref() != Some(active.id.as_str()) {
            return None;
        }
        self.delivered = None;
        Some(active.id.clone())
    }
}

fn anchored_message(mode: Interaction, boundary: &crate::state::Boundary) -> Value {
    let mut details = interaction_contract_details(mode);
    details["contextBoundary"] = serde_json::to_value(boundary).unwrap_or_default();
    context_message(
        crate::contracts::INTERACTION_CONTRACT_TYPE,
        interaction_contract_content(mode),
        Some(details),
    )
}

fn anchor_of(message: &Value) -> Option<InteractionAnchor> {
    let mode = interaction_contract_of(message)?;
    let boundary = unwrap(message).get("details")?.get("contextBoundary")?;
    let boundary: crate::state::Boundary = serde_json::from_value(boundary.clone()).ok()?;
    anchor::is_hash(&boundary.hash).then_some(InteractionAnchor {
        version: 1,
        mode,
        boundary,
    })
}

/// Puts recorded interaction contracts back at their boundaries, then appends the expected one
/// when the workflow is active and the latest contract differs. Returns the new anchor to record.
pub fn interaction(
    messages: Vec<Value>,
    anchors: &[InteractionAnchor],
    expected: Option<Interaction>,
) -> (Vec<Value>, Option<InteractionAnchor>) {
    let mut all: Vec<InteractionAnchor> = anchors.to_vec();
    for found in messages.iter().filter_map(anchor_of) {
        if !all.contains(&found) {
            all.push(found);
        }
    }
    let mut result = messages;
    for restored in &all {
        let remaining: Vec<Value> = result
            .iter()
            .filter(|m| anchor_of(m).as_ref() != Some(restored))
            .cloned()
            .collect();
        if !anchor::matches(&remaining, &restored.boundary) {
            continue;
        }
        result = remaining;
        result.insert(
            restored.boundary.length,
            anchored_message(restored.mode, &restored.boundary),
        );
    }
    let Some(expected) = expected else {
        return (result, None);
    };
    if latest_interaction_contract(&result).map(|(_, mode)| mode) == Some(expected) {
        return (result, None);
    }
    let boundary = anchor::boundary(&result);
    result.push(anchored_message(expected, &boundary));
    let anchor = InteractionAnchor {
        version: 1,
        mode: expected,
        boundary,
    };
    (result, Some(anchor))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::contracts::{Mode, mode_contract_message};

    fn completion(id: &str) -> Vec<Value> {
        vec![
            json!({ "role": "assistant", "content": [
                { "type": "text", "text": "done" },
                { "type": "toolCall", "id": id, "name": COMPLETE_TOOL, "arguments": {} }
            ]}),
            json!({ "role": "toolResult", "toolName": COMPLETE_TOOL, "toolCallId": id, "content": [] }),
        ]
    }

    #[test]
    fn strips_plan_artifacts_after_exit_but_keeps_the_implemented_one() {
        let mut messages = vec![json!({ "role": "user", "content": "plan" })];
        messages.extend(completion("a"));
        messages
            .push(json!({ "role": "custom", "customType": PROPOSED_PLAN_TYPE, "content": "x" }));
        let (stripped, _) = Retention::default().transform(messages.clone(), &PlanState::default());
        assert_eq!(stripped.len(), 2, "{stripped:?}");
        assert_eq!(stripped[1]["content"].as_array().unwrap().len(), 1);

        let mut implemented = vec![json!({ "role": "user", "content": "plan" })];
        implemented.extend(completion("b"));
        implemented.push(json!({ "role": "user", "content": HISTORY_IMPLEMENTATION_PROMPT }));
        let (kept, _) = Retention::default().transform(implemented.clone(), &PlanState::default());
        assert_eq!(kept, implemented);
    }

    #[test]
    fn injects_the_active_plan_until_the_handoff_arrives() {
        let state = PlanState {
            active_implementation: Some(ActiveImplementation {
                id: "i1".into(),
                plan: "# P".into(),
                source: COMPLETE_TOOL.into(),
                started_at: 5,
                retention: "clear-after-first-run".into(),
            }),
            ..PlanState::default()
        };
        let mut retention = Retention::default();
        let (with_context, clear) =
            retention.transform(vec![json!({ "role": "user", "content": "go" })], &state);
        assert_eq!(with_context[0]["customType"], IMPLEMENTATION_CONTEXT_TYPE);
        assert!(clear.is_none());
        let handoff = json!({ "role": "user", "content": implementation_handoff("# P") });
        let (delivered, _) = retention.transform(vec![handoff], &state);
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            retention
                .settled(state.active_implementation.as_ref())
                .as_deref(),
            Some("i1")
        );
    }

    #[test]
    fn interaction_fallback_is_anchored() {
        let base = vec![
            mode_contract_message(Mode::Plan),
            json!({ "role": "user", "content": "go" }),
        ];
        let (first, anchor) = interaction(base.clone(), &[], Some(Interaction::Discussion));
        let anchor = anchor.unwrap();
        assert_eq!(first.len(), 3);
        let mut later = base;
        later.push(json!({ "role": "assistant", "content": [] }));
        let (second, again) = interaction(later, &[anchor], Some(Interaction::Discussion));
        assert!(again.is_none());
        assert_eq!(
            interaction_contract_of(&second[2]),
            Some(Interaction::Discussion)
        );
    }
}
