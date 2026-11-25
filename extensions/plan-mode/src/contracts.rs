//! Model-visible texts: the Plan and Normal mode contracts, the interaction contracts, the
//! finalization prompts, and the implementation handoffs. The wording matches upstream
//! `@narumitw/pi-plan-mode` 0.62.0 so sessions keep the same cache-stable prefix.

use serde_json::{Value, json};

pub const MODE_CONTRACT_TYPE: &str = "plan-mode-transition";
pub const INTERACTION_CONTRACT_TYPE: &str = "plan-mode-interaction";
pub const IMPLEMENTATION_CONTEXT_TYPE: &str = "plan-mode-implementation-context";
pub const LEGACY_CONTEXT_TYPE: &str = "plan-mode-context";
pub const PROPOSED_PLAN_TYPE: &str = "proposed-plan";

const MODE_CONTRACT_VERSION: u64 = 1;
const INTERACTION_CONTRACT_VERSION: u64 = 1;

const PLAN_CONTRACT_MARKER: &str = "[PI PLAN MODE CONTRACT v1: PLAN]";
const NORMAL_CONTRACT: &str = "[PI PLAN MODE CONTRACT v1: NORMAL]
Plan Mode is no longer active.
Follow the user's current Normal-mode request and ordinary system instructions.
Earlier Plan-mode restrictions no longer apply, but retain the planning conversation as context.
The visible tool schemas are session capabilities; use only tools appropriate for the current request.";

const PLAN_PROMPT: &str = r#"[CODEX-LIKE PLAN MODE ACTIVE]
# Plan Mode (Conversational)

You are in Plan Mode, a Codex-like collaboration mode for producing a decision-complete implementation plan. Chat your way to the plan before finalizing it. A final plan must leave no implementation decisions unresolved.

## Mode rules

- Stay in Plan Mode until a developer or extension explicitly exits it.
- Treat requests to implement as requests to plan the implementation; do not edit files or carry out the plan.
- Do not use update_plan/TODO tooling in Plan Mode; Plan Mode is conversational planning, not execution progress tracking.
- Plan Mode keeps the session's model-visible tool schemas unchanged and enforces a runtime policy allowlist. Tools outside its reviewed core policy, including native MCP tools and orchestration tools, require explicit user opt-in at their own risk. Direct and model-only tools must be active; registered codemode or deferred tools can be selected without direct activation. Every nested call requires its own permission; selecting an orchestrator does not authorize its callees.
- Do not perform mutating actions: no edit/write tools, no patching, no formatting that rewrites files, no dependency installation, no commits, no migrations.

## Phase 1 — Ground in the environment

- Explore first and ask second. Use non-mutating exploration to read files, search, inspect configuration, run read-only checks, and resolve discoverable facts.
- Before asking the user any question, perform at least one targeted non-mutating exploration pass unless no local environment or repository is available.
- Do not ask questions that can be answered from repository or system truth. Ask only when multiple plausible choices remain, a needed identifier/context is missing, or the ambiguity is product intent.

## Phase 2 — Intent chat

- Keep asking until you can clearly state the goal, success criteria, in/out of scope, constraints, current state, and key preferences/tradeoffs.
- Bias toward questions over guessing: if a high-impact ambiguity remains, do not produce a proposed plan yet.
- For an unanswered preference or tradeoff, use the recommended option only when it is low risk and record that default as an explicit assumption in the final plan.

## Phase 3 — Implementation chat

- Once intent is stable, keep asking until the spec is decision-complete: approach, interfaces, data flow, edge cases/failure modes, testing and acceptance criteria, and any migration or compatibility constraints.
- Use plan_mode_question for important preferences, tradeoffs, or assumption locks that cannot be discovered by non-mutating exploration. Ask 1-3 concise questions with 2-4 meaningful options. Do not include filler options.
- Treat plan_mode_question and plan_mode_complete as callable when they are listed in the current request's active tools. Do not infer that they are unavailable from earlier modes or conversation history.
- If a Plan tool call returns an actual error, respond to that error. Do not replace an available structured tool call with prose claiming that the tool is unavailable.
- If plan_mode_question returns cancelled or ui_unavailable, do not jump straight to a final plan when the missing answer is high impact. Ask one concise plain-text question or proceed only with a clearly stated low-risk assumption.

## Ending each turn

Every Plan-mode turn that advances or finalizes the plan must end in exactly one of these ways:

- If a material decision remains, use plan_mode_question. If interactive UI is unavailable, ask one concise plain-text question instead.
- If the implementation plan is decision-complete, call plan_mode_complete alone as your final action. Do not call other tools in the same batch and do not emit a normal assistant response after it.

If a follow-up asks only for clarification and does not change or challenge the plan, answer it directly, then call plan_mode_complete alone as the final action with the complete unchanged plan so it remains available for implementation.

Never end with prose that merely announces you are about to present, write, or finalize the plan. Submit the actual plan with plan_mode_complete in that turn.

## Completion rule

Only call plan_mode_complete when the plan leaves no implementation decisions unresolved. Pass the complete plan as Markdown with:

- A clear title
- A brief summary
- Important changes to behavior, public APIs, interfaces, or types
- Test cases and verification scenarios
- Explicit assumptions and defaults chosen where needed

Keep the plan concise, human and agent digestible, and free of open decisions. Prefer grouped behavior-level changes over file-by-file or symbol-by-symbol inventories. Do not ask "should I proceed?"; plan_mode_complete opens the Plan-mode ready flow.

If the user requests revisions after a completed plan, the next plan_mode_complete call must contain a complete replacement, not a delta. If there is not enough information for a complete replacement, continue planning with plan_mode_question instead of calling plan_mode_complete."#;

const QUESTIONS_CONTRACT: &str = "[PI PLAN MODE INTERACTION CONTRACT v1: QUESTIONS]
Plan Mode remains active with all read-only restrictions unchanged. Do not edit files, perform mutating actions, or implement the plan.
The user has enabled questionnaires. Restore the original Plan-mode rule: every turn that advances or finalizes the plan must end with plan_mode_question when a material decision remains, or plan_mode_complete alone as the final action when the plan is decision-complete. If interactive UI is unavailable, ask one concise plain-text question instead.
Re-evaluate the current discussion after the next user message. Do not replay stale questions or reopen an earlier questionnaire. Enabling questionnaires does not itself start a model turn or open a questionnaire.
Only the user can change this interaction mode.";

const DISCUSSION_CONTRACT: &str = "[PI PLAN MODE INTERACTION CONTRACT v1: DISCUSSION]
Plan Mode remains active with all read-only restrictions unchanged. Do not edit files, perform mutating actions, or implement the plan.
This contract overrides the original Plan-mode rule that each advancing or finalizing turn must end with plan_mode_question or plan_mode_complete. You may analyze, answer, ask natural plain-text questions, and end with ordinary text without calling either tool.
Do not call plan_mode_question or open questionnaires. Do not resume questionnaires automatically; only the user can enable them again. Discard unsubmitted questionnaire answers and do not replay stale questions.
When the plan is decision-complete, you may call plan_mode_complete alone as the final action to display and save the complete plan. Completion does not automatically open an action menu in discussion mode; the user can open /plan. Continue revisions through the existing complete-replacement plan flow, and use plain-text questions when decisions remain.";

pub const FINALIZE_PLAN_PROMPT: &str = "Finalize the current implementation plan now. If any material decision remains, use plan_mode_question instead. Otherwise call plan_mode_complete alone as your final action with the complete decision-ready plan.";
pub const DISCUSSION_FINALIZE_PLAN_PROMPT: &str = "Finalize the current implementation plan if it is decision-ready. If a material decision remains, discuss it or ask one concise plain-text question instead. Questionnaires remain paused until the user enables them. Otherwise call plan_mode_complete alone with the complete plan.";
pub const RETRY_FINALIZE_PLAN_PROMPT: &str = "Your previous finalization response did not call plan_mode_question or plan_mode_complete. Both tools are available in the current Plan-mode request. If a material decision remains, call plan_mode_question now. Otherwise call plan_mode_complete alone with the complete decision-ready plan. Do not respond with prose about tool availability.";

pub const HISTORY_IMPLEMENTATION_PROMPT: &str = "Implement the plan.";
pub const IMPLEMENTATION_HANDOFF_PREFIX: &str =
    "Plan mode is now disabled. Full tool access is restored. Implement this proposed plan now:";

/// The workflow mode a contract message selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Normal,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Normal => "normal",
        }
    }
}

/// Questionnaires or free discussion inside an active workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interaction {
    #[default]
    Questions,
    Discussion,
}

impl Interaction {
    pub fn as_str(self) -> &'static str {
        match self {
            Interaction::Questions => "questions",
            Interaction::Discussion => "discussion",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Interaction::Questions => Interaction::Discussion,
            Interaction::Discussion => Interaction::Questions,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "questions" => Some(Interaction::Questions),
            "discussion" => Some(Interaction::Discussion),
            _ => None,
        }
    }
}

pub fn mode_contract_content(mode: Mode) -> String {
    match mode {
        Mode::Plan => format!("{PLAN_CONTRACT_MARKER}\n{PLAN_PROMPT}"),
        Mode::Normal => NORMAL_CONTRACT.to_owned(),
    }
}

pub fn mode_contract_details(mode: Mode) -> Value {
    json!({ "version": MODE_CONTRACT_VERSION, "mode": mode.as_str() })
}

pub fn interaction_contract_content(mode: Interaction) -> &'static str {
    match mode {
        Interaction::Questions => QUESTIONS_CONTRACT,
        Interaction::Discussion => DISCUSSION_CONTRACT,
    }
}

pub fn interaction_contract_details(mode: Interaction) -> Value {
    json!({ "version": INTERACTION_CONTRACT_VERSION, "mode": mode.as_str() })
}

/// A hidden custom message as it appears in the model context.
pub fn context_message(custom_type: &str, content: &str, details: Option<Value>) -> Value {
    let mut message = json!({
        "role": "custom", "customType": custom_type, "content": content,
        "display": false, "timestamp": 0
    });
    if let Some(details) = details {
        message["details"] = details;
    }
    message
}

pub fn mode_contract_message(mode: Mode) -> Value {
    context_message(
        MODE_CONTRACT_TYPE,
        &mode_contract_content(mode),
        Some(mode_contract_details(mode)),
    )
}

#[cfg(test)]
pub fn interaction_contract_message(mode: Interaction) -> Value {
    context_message(
        INTERACTION_CONTRACT_TYPE,
        interaction_contract_content(mode),
        Some(interaction_contract_details(mode)),
    )
}

/// The message inside a session entry, or the value itself.
pub fn unwrap(message: &Value) -> &Value {
    message.get("message").unwrap_or(message)
}

/// The custom message of a context message or a `custom_message` session entry.
fn custom_message(value: &Value) -> Option<&Value> {
    match value.get("type").and_then(Value::as_str) {
        Some("custom_message") => Some(value),
        Some("message") => value
            .get("message")
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("custom")),
        None => (value.get("role").and_then(Value::as_str) == Some("custom")).then_some(value),
        Some(_) => None,
    }
}

pub fn custom_type(value: &Value) -> Option<&str> {
    unwrap(value).get("customType").and_then(Value::as_str)
}

pub fn role(value: &Value) -> Option<&str> {
    unwrap(value).get("role").and_then(Value::as_str)
}

pub fn mode_contract_of(message: &Value) -> Option<Mode> {
    let candidate = unwrap(message);
    if candidate.get("customType").and_then(Value::as_str) != Some(MODE_CONTRACT_TYPE) {
        return None;
    }
    let content = candidate.get("content").and_then(Value::as_str)?;
    [Mode::Plan, Mode::Normal]
        .into_iter()
        .find(|mode| content == mode_contract_content(*mode))
}

pub fn has_mode_contract_artifact(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|m| custom_type(m) == Some(MODE_CONTRACT_TYPE))
}

pub fn latest_mode_contract(messages: &[Value]) -> Option<(usize, Mode)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, m)| mode_contract_of(m).map(|mode| (index, mode)))
}

fn is_summary(message: &Value) -> bool {
    matches!(role(message), Some("compactionSummary" | "branchSummary"))
}

/// Inserts the expected contract when the latest one differs: after the latest contract artifact,
/// else after the leading system and summary messages.
pub fn reconcile_mode_contract(messages: Vec<Value>, expected: Mode) -> Vec<Value> {
    if latest_mode_contract(&messages).map(|(_, mode)| mode) == Some(expected) {
        return messages;
    }
    let latest_artifact = messages
        .iter()
        .rposition(|m| custom_type(m) == Some(MODE_CONTRACT_TYPE));
    let index = match latest_artifact {
        Some(index) => index + 1,
        None => {
            let mut index = usize::from(messages.first().and_then(role) == Some("system"));
            while messages.get(index).is_some_and(is_summary) {
                index += 1;
            }
            index
        }
    };
    let mut result = messages;
    result.insert(index.min(result.len()), mode_contract_message(expected));
    result
}

pub fn interaction_contract_of(message: &Value) -> Option<Interaction> {
    let candidate = custom_message(message)?;
    if candidate.get("customType").and_then(Value::as_str) != Some(INTERACTION_CONTRACT_TYPE)
        || candidate.get("display") != Some(&Value::Bool(false))
    {
        return None;
    }
    let details = candidate.get("details")?;
    if details.get("version").and_then(Value::as_u64) != Some(INTERACTION_CONTRACT_VERSION) {
        return None;
    }
    let mode = Interaction::parse(details.get("mode")?.as_str()?)?;
    (candidate.get("content").and_then(Value::as_str) == Some(interaction_contract_content(mode)))
        .then_some(mode)
}

/// The latest interaction contract after the latest mode contract.
pub fn latest_interaction_contract(messages: &[Value]) -> Option<(usize, Interaction)> {
    for (index, message) in messages.iter().enumerate().rev() {
        if let Some(mode) = interaction_contract_of(message) {
            return Some((index, mode));
        }
        if custom_message(message).is_some_and(|m| mode_contract_of(m).is_some()) {
            return None;
        }
    }
    None
}

/// The prompt that `/plan finalize` sends in a managed-draft workflow.
pub fn draft_finalization_prompt(prompt: &str) -> String {
    for needle in [
        " with the complete decision-ready plan",
        " with the complete plan",
    ] {
        if let Some(at) = prompt.find(needle) {
            return format!(
                "{} using the workflow's current completion contract{}",
                &prompt[..at],
                &prompt[at + needle.len()..]
            );
        }
    }
    prompt.to_owned()
}

pub fn implementation_handoff(plan: &str) -> String {
    format!("{IMPLEMENTATION_HANDOFF_PREFIX}\n\n{plan}")
}

pub fn transferred_plan_prompt(plan: &str, fresh: bool) -> String {
    let destination = if fresh { " in a fresh context" } else { "" };
    format!(
        "A previous agent produced the plan below to accomplish the user's task. Implement the plan{destination}. Treat the plan as the source of user intent, re-read files as needed, and carry the work through implementation and verification.\n\n{plan}"
    )
}

pub fn active_implementation_context(plan: &str) -> String {
    format!(
        "[ACTIVE IMPLEMENTATION PLAN]\n\nThe user approved the exact implementation plan below. Continue following it until the user explicitly clears or supersedes it. The exact plan is the remainder of this message:\n\n{plan}"
    )
}

/// The text blocks of a message content, joined with newlines.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn message_text(message: &Value) -> String {
    content_text(unwrap(message).get("content"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_inserts_after_summaries_and_keeps_a_matching_contract() {
        let messages = vec![
            json!({ "role": "compactionSummary", "summary": "s" }),
            json!({ "role": "user", "content": "hi" }),
        ];
        let reconciled = reconcile_mode_contract(messages, Mode::Plan);
        assert_eq!(mode_contract_of(&reconciled[1]), Some(Mode::Plan));
        let again = reconcile_mode_contract(reconciled.clone(), Mode::Plan);
        assert_eq!(again, reconciled);
        let normal = reconcile_mode_contract(reconciled, Mode::Normal);
        assert_eq!(mode_contract_of(&normal[2]), Some(Mode::Normal));
    }

    #[test]
    fn interaction_contract_stops_at_a_mode_boundary() {
        let messages = vec![
            interaction_contract_message(Interaction::Discussion),
            mode_contract_message(Mode::Plan),
        ];
        assert_eq!(latest_interaction_contract(&messages), None);
        let messages = vec![
            mode_contract_message(Mode::Plan),
            interaction_contract_message(Interaction::Discussion),
        ];
        assert_eq!(
            latest_interaction_contract(&messages),
            Some((1, Interaction::Discussion))
        );
    }

    #[test]
    fn draft_prompt_replaces_the_completion_clause() {
        assert_eq!(
            draft_finalization_prompt(FINALIZE_PLAN_PROMPT),
            "Finalize the current implementation plan now. If any material decision remains, use plan_mode_question instead. Otherwise call plan_mode_complete alone as your final action using the workflow's current completion contract."
        );
    }
}
