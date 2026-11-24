//! The workflow state persisted as `plan-mode-state` session entries (upstream `state.ts`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contracts::Interaction;

pub const STATE_ENTRY: &str = "plan-mode-state";
pub const COMPLETE_TOOL: &str = "plan_mode_complete";
pub const LEGACY_SOURCE: &str = "legacy_proposed_plan";
/// Upstream counts JavaScript characters (UTF-16 code units).
pub const MAX_PLAN_CHARS: usize = 50_000;
pub const MAX_MODEL_IDENTIFIER: usize = 512;
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const RETENTIONS: [&str; 3] = ["clear-on-start", "clear-after-first-run", "keep"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPlan {
    pub plan: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveImplementation {
    pub id: String,
    pub plan: String,
    pub source: String,
    pub started_at: u64,
    pub retention: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn valid(&self) -> bool {
        valid_identifier(&self.provider) && valid_identifier(&self.model_id)
    }
}

pub fn valid_identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.encode_utf16().count() <= MAX_MODEL_IDENTIFIER
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPolicy {
    /// `automatic` or `explicit`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_names: Option<Vec<String>>,
    pub allowed_names: Vec<String>,
    pub resolved: bool,
}

impl ToolPolicy {
    pub fn automatic(&self) -> bool {
        self.kind == "automatic"
    }
}

/// A contract anchored at a context boundary: inserted again at `length` while the first `length`
/// messages still hash to `hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub length: usize,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionAnchor {
    pub version: u8,
    pub mode: Interaction,
    pub boundary: Boundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftIdentity {
    pub version: u8,
    pub workflow_id: String,
    pub session_id: String,
    /// `pending` until the first context admits it, then `active`.
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_mode: Option<Interaction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interaction_restorations: Vec<InteractionAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_plan_source: Option<String>,
    pub awaiting_action: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_plan: Option<SavedPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_implementation: Option<ActiveImplementation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_tool_policy: Option<ToolPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_thinking_level: Option<String>,
}

impl PlanState {
    pub fn interaction(&self) -> Interaction {
        self.interaction_mode.unwrap_or_default()
    }

    /// The plan `/plan show`, export, and implementation use: ready, saved, or active.
    pub fn stored_plan(&self) -> Option<&str> {
        self.ready_plan()
            .or(self.saved_plan.as_ref().map(|s| s.plan.as_str()))
            .or(self.active_implementation.as_ref().map(|a| a.plan.as_str()))
    }

    pub fn ready_plan(&self) -> Option<&str> {
        self.enabled
            .then_some(self.latest_plan.as_deref())
            .flatten()
    }

    /// Clears the workflow fields when the workflow ends, keeping tool selection and anchors.
    pub fn end_workflow(&mut self) {
        self.enabled = false;
        self.interaction_mode = None;
        self.latest_plan = None;
        self.latest_plan_source = None;
        self.awaiting_action = false;
        self.saved_plan = None;
        self.active_implementation = None;
        self.workflow_tool_policy = None;
        self.draft = None;
        self.manual_thinking_level = None;
    }
}

/// `Ok(plan)` when the text is a non-empty plan within the size limit.
pub fn validate_plan(plan: &str) -> Result<String, String> {
    if plan.trim().is_empty() {
        return Err("plan must not be empty".into());
    }
    if plan.encode_utf16().count() > MAX_PLAN_CHARS {
        return Err(format!("plan must not exceed {MAX_PLAN_CHARS} characters"));
    }
    Ok(plan.to_owned())
}

/// The plan in `plan_mode_complete` result details.
pub fn plan_from_completion_details(details: &Value) -> Option<String> {
    if details.get("version").and_then(Value::as_u64) != Some(1)
        || details.get("source").and_then(Value::as_str) != Some(COMPLETE_TOOL)
    {
        return None;
    }
    validate_plan(details.get("plan")?.as_str()?).ok()
}

fn is_source(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| *s == COMPLETE_TOOL || *s == LEGACY_SOURCE)
        .map(str::to_owned)
}

fn plan_text(value: Option<&Value>) -> Option<String> {
    validate_plan(value?.as_str()?).ok()
}

fn fixed_level(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|level| THINKING_LEVELS.contains(level))
        .map(str::to_owned)
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    let names: Option<Vec<String>> = items
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect();
    names.map(|names| crate::policy::unique(&names))
}

fn saved_plan(value: Option<&Value>) -> Option<SavedPlan> {
    let value = value?;
    Some(SavedPlan {
        plan: plan_text(value.get("plan"))?,
        source: is_source(value.get("source"))?,
    })
}

fn active_implementation(value: Option<&Value>) -> Option<ActiveImplementation> {
    let value = value?;
    let id = value.get("id")?.as_str()?;
    let id_ok = (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
    if !id_ok {
        return None;
    }
    let retention = value
        .get("retention")
        .and_then(Value::as_str)
        .filter(|r| RETENTIONS.contains(r))
        .unwrap_or("keep");
    Some(ActiveImplementation {
        id: id.to_owned(),
        plan: plan_text(value.get("plan"))?,
        source: is_source(value.get("source"))?,
        started_at: value.get("startedAt")?.as_u64()?,
        retention: retention.to_owned(),
    })
}

fn tool_policy(value: Option<&Value>) -> Option<ToolPolicy> {
    let value = value?;
    let denied = ToolPolicy {
        kind: "automatic".into(),
        desired_names: None,
        allowed_names: Vec::new(),
        resolved: true,
    };
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .filter(|k| *k == "automatic" || *k == "explicit");
    let allowed = strings(value.get("allowedNames"));
    let resolved = value.get("resolved").and_then(Value::as_bool);
    let (Some(kind), Some(allowed_names), Some(resolved)) = (kind, allowed, resolved) else {
        return Some(denied);
    };
    let desired_names = if kind == "explicit" {
        match strings(value.get("desiredNames")) {
            Some(names) => Some(names),
            None => return Some(denied),
        }
    } else {
        None
    };
    Some(ToolPolicy {
        kind: kind.to_owned(),
        desired_names,
        allowed_names,
        resolved,
    })
}

fn draft(value: Option<&Value>) -> Option<DraftIdentity> {
    let value = value?;
    let workflow_id = value.get("workflowId")?.as_str()?;
    let session_id = value.get("sessionId")?.as_str()?;
    let status = value.get("status")?.as_str()?;
    let ok = value.get("version").and_then(Value::as_u64) == Some(1)
        && crate::ids::is_uuid(workflow_id)
        && !session_id.is_empty()
        && (status == "pending" || status == "active");
    ok.then(|| DraftIdentity {
        version: 1,
        workflow_id: workflow_id.to_owned(),
        session_id: session_id.to_owned(),
        status: status.to_owned(),
    })
}

fn anchors(value: Option<&Value>) -> Vec<InteractionAnchor> {
    let items = value.and_then(Value::as_array).cloned().unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<InteractionAnchor>(item).ok())
        .filter(|anchor| anchor.version == 1 && crate::anchor::is_hash(&anchor.boundary.hash))
        .collect()
}

/// The latest `plan_mode_complete` plan in `entries`.
fn latest_completion(entries: &[Value]) -> Option<String> {
    entries.iter().rev().find_map(|entry| {
        let message = entry.get("message")?;
        (message.get("role").and_then(Value::as_str) == Some("toolResult")
            && message.get("toolName").and_then(Value::as_str) == Some(COMPLETE_TOOL))
        .then(|| plan_from_completion_details(message.get("details")?))
        .flatten()
    })
}

/// The state of the latest state entry of `branch`, normalized like upstream `restorePlanModeState`.
pub fn restore(branch: &[Value]) -> PlanState {
    let found = branch.iter().enumerate().rev().find(|(_, entry)| {
        entry.get("type").and_then(Value::as_str) == Some("custom")
            && entry.get("customType").and_then(Value::as_str) == Some(STATE_ENTRY)
    });
    let Some((index, entry)) = found else {
        return PlanState::default();
    };
    let Some(data) = entry.get("data").filter(|d| d.is_object()) else {
        return PlanState::default();
    };
    let enabled = data.get("enabled") == Some(&Value::Bool(true));
    let persisted_plan = enabled.then(|| plan_text(data.get("latestPlan"))).flatten();
    let recovered = if enabled && persisted_plan.is_none() {
        latest_completion(&branch[index + 1..])
    } else {
        None
    };
    let latest_plan_source = if persisted_plan.is_some() {
        is_source(data.get("latestPlanSource"))
    } else {
        recovered.as_ref().map(|_| COMPLETE_TOOL.to_owned())
    };
    let latest_plan = persisted_plan.or(recovered);
    let active = (!enabled)
        .then(|| active_implementation(data.get("activeImplementation")))
        .flatten();
    let saved = (!enabled && active.is_none())
        .then(|| saved_plan(data.get("savedPlan")))
        .flatten();
    let interaction_mode = (enabled && data.get("interactionMode").is_some()).then(|| {
        match data.get("interactionMode").and_then(Value::as_str) {
            Some("discussion") => Interaction::Discussion,
            _ => Interaction::Questions,
        }
    });
    let when_enabled = |f: fn(Option<&Value>) -> Option<String>, key: &str| {
        enabled.then(|| f(data.get(key))).flatten()
    };
    PlanState {
        enabled,
        interaction_mode,
        interaction_restorations: anchors(data.get("interactionRestorations")),
        draft: enabled.then(|| draft(data.get("draft"))).flatten(),
        awaiting_action: enabled && latest_plan.is_some(),
        latest_plan_source: enabled.then_some(latest_plan_source).flatten(),
        latest_plan,
        saved_plan: saved,
        active_implementation: active,
        selected_tool_names: strings(data.get("selectedToolNames")),
        workflow_tool_policy: enabled
            .then(|| tool_policy(data.get("workflowToolPolicy")))
            .flatten(),
        previous_thinking_level: when_enabled(fixed_level, "previousThinkingLevel"),
        applied_thinking_level: when_enabled(fixed_level, "appliedThinkingLevel"),
        manual_thinking_level: when_enabled(fixed_level, "manualThinkingLevel"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(data: Value) -> Value {
        json!({ "type": "custom", "customType": STATE_ENTRY, "data": data })
    }

    #[test]
    fn restores_the_latest_entry_and_recovers_a_completed_plan() {
        let branch = vec![
            entry(json!({ "enabled": false, "awaitingAction": false })),
            entry(
                json!({ "enabled": true, "awaitingAction": false, "interactionMode": "discussion" }),
            ),
            json!({ "type": "message", "message": {
                "role": "toolResult", "toolName": COMPLETE_TOOL,
                "details": { "version": 1, "source": COMPLETE_TOOL, "plan": "# Plan" }
            }}),
        ];
        let state = restore(&branch);
        assert!(state.enabled && state.awaiting_action);
        assert_eq!(state.latest_plan.as_deref(), Some("# Plan"));
        assert_eq!(state.latest_plan_source.as_deref(), Some(COMPLETE_TOOL));
        assert_eq!(state.interaction(), Interaction::Discussion);
    }

    #[test]
    fn drops_invalid_fields() {
        let state = restore(&[entry(json!({
            "enabled": false, "awaitingAction": false,
            "savedPlan": { "plan": "  ", "source": COMPLETE_TOOL },
            "pendingImplementationRuntime": { "version": 1, "thinkingLevel": "huge" },
            "workflowToolPolicy": { "kind": "explicit" }
        }))]);
        assert_eq!(state, PlanState::default());
        let state = restore(&[entry(json!({
            "enabled": true, "awaitingAction": false,
            "workflowToolPolicy": { "kind": "explicit", "allowedNames": ["read"], "resolved": true }
        }))]);
        let policy = state.workflow_tool_policy.unwrap();
        assert!(
            policy.allowed_names.is_empty() && policy.resolved,
            "a malformed policy denies all"
        );
    }

    #[test]
    fn round_trips_through_serde() {
        let state = PlanState {
            enabled: true,
            interaction_mode: Some(Interaction::Questions),
            latest_plan: Some("p".into()),
            latest_plan_source: Some(COMPLETE_TOOL.into()),
            awaiting_action: true,
            ..PlanState::default()
        };
        let restored = restore(&[entry(serde_json::to_value(&state).unwrap())]);
        assert_eq!(restored, state);
    }
}
