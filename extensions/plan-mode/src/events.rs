//! Tool handlers and pi events: the Plan policy on `tool_call`, the `context` pipeline,
//! completion detection, and the ready menu after `agent_settled`.

use pi_ext_sdk::{EventCtx, EventReply, Result, ToolCtx, ToolResult};
use serde_json::{Value, json};

use crate::app::App;
use crate::complete::{self, Parsed, invalid_plan_message, parse_proposed_plan};
use crate::contracts::{
    INTERACTION_CONTRACT_TYPE, Interaction, MODE_CONTRACT_TYPE, Mode, message_text,
    reconcile_mode_contract, role, unwrap,
};
use crate::draft::{self, Authorization};
use crate::policy::{
    Availability, Policy, Route, blocked_bash_segment, blocked_powershell_segment, read_command,
};
use crate::question::{self, Asked, QUESTION_TOOL};
use crate::state::{COMPLETE_TOOL, LEGACY_SOURCE, PlanState};

fn text<'a>(ev: &'a EventCtx, key: &str) -> &'a str {
    ev.get(key).and_then(Value::as_str).unwrap_or("")
}

pub async fn question_tool(app: App, params: Value, ctx: ToolCtx) -> Result<ToolResult> {
    let host = ctx.host().clone();
    if !app.lock().state.enabled {
        return Ok(question::cancelled(
            &[],
            "plan_mode_inactive",
            "Error: plan_mode_question is only available while Plan mode is active.",
        ));
    }
    let questions = match question::normalize(&params) {
        Ok(questions) => questions,
        Err(error) => {
            return Ok(question::cancelled(
                &[],
                "invalid_input",
                &format!("Error: {error}"),
            ));
        }
    };
    app.finalization_satisfied();
    if app.questions_paused() {
        return Ok(question::cancelled(
            &questions,
            "questions_paused",
            "Questionnaires are paused. Continue in plain text; only the user can enable questionnaires.",
        ));
    }
    if !ctx.snapshot().has_ui {
        return Ok(question::cancelled(
            &questions,
            "ui_unavailable",
            "Unable to ask Plan-mode questions because interactive UI is not available.",
        ));
    }
    let rpc = ctx.snapshot().mode.as_deref() == Some("rpc");
    let (generation, f2) = {
        let core = app.lock();
        (core.workflow_gen, core.f2 && !rpc)
    };
    let token = ctx.cancel_token().child_token();
    let id = app.track_question(token.clone());
    let asked = question::ask(&host, &token, &questions, rpc, f2).await;
    app.untrack_question(id);
    let (enabled, current) = {
        let core = app.lock();
        (core.state.enabled, core.workflow_gen == generation)
    };
    if !enabled {
        return Ok(question::cancelled(
            &questions,
            "plan_mode_inactive",
            "Plan-mode question cancelled because Plan mode is no longer active.",
        ));
    }
    if !current {
        return Ok(question::cancelled(
            &questions,
            "cancelled",
            "Plan-mode question cancelled because the session changed.",
        ));
    }
    Ok(match asked? {
        Asked::Answered(answers) => question::answered(&questions, &answers),
        Asked::Cancelled => question::cancelled(
            &questions,
            "cancelled",
            "User cancelled the Plan-mode question prompt.",
        ),
        Asked::Discussion if !app.toggle_interaction(&host, true).await? => question::cancelled(
            &questions,
            "cancelled",
            "User cancelled the Plan-mode question prompt.",
        ),
        Asked::Discussion => {
            app.abort_after(ctx.tool_call_id());
            question::cancelled(
                &questions,
                "discussion_requested",
                "The user switched to free discussion without submitting answers. Wait for the user to speak; do not reopen the questionnaire.",
            )
            .terminate()
        }
    })
}

pub async fn complete_tool(app: App, params: Value, ctx: ToolCtx) -> Result<ToolResult> {
    let host = ctx.host().clone();
    let (enabled, managed) = {
        let core = app.lock();
        (core.state.enabled, core.managed_draft())
    };
    if !enabled {
        return Ok(ToolResult::error(
            "plan_mode_complete is only available while Plan mode is active",
        ));
    }
    let plan = if managed {
        app.lock().draft.snapshot()
    } else {
        complete::plan_argument(&params)
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return Ok(ToolResult::error(error)),
    };
    app.accept_completion(&host, &plan, COMPLETE_TOOL).await?;
    Ok(complete::completed(&plan))
}

fn block(reason: String) -> Result<EventReply> {
    Ok(EventReply::block(reason))
}

pub async fn tool_call(app: App, ev: EventCtx) -> Result<EventReply> {
    let host = ev.host();
    let name = text(&ev, "toolName").to_owned();
    let call_id = text(&ev, "toolCallId").to_owned();
    let nested = ev.get("parentToolCallId").is_some_and(|v| !v.is_null());
    let input = ev.get("input").cloned().unwrap_or(Value::Null);
    let helper = name == QUESTION_TOOL || name == COMPLETE_TOOL;
    let (enabled, managed, awaiting) = {
        let core = app.lock();
        (
            core.state.enabled,
            core.managed_draft(),
            core.state.awaiting_action,
        )
    };
    if !enabled {
        if helper {
            return block(format!(
                "{name} is only available while Plan mode is active."
            ));
        }
        return Ok(EventReply::none());
    }
    if managed && (name == "write" || name == "edit") {
        let (tools, active) = app.tools(host).await;
        let native = tools.iter().any(|t| {
            t.name == name
                && t.has_native_source()
                && matches!(t.exposure.as_deref(), None | Some("direct"))
                && active.contains(&t.name)
        });
        if awaiting || nested || !native {
            return block(
                "Only direct, active Pi native tools may change an unfinished Plan draft.".into(),
            );
        }
        let path = input.get("path").and_then(Value::as_str);
        return match app.lock().draft.admit(&call_id, path) {
            Ok(()) => Ok(EventReply::none()),
            Err(reason) => block(reason),
        };
    }
    if managed && name == COMPLETE_TOOL {
        let branch = host.branch(None).await?;
        if nested || !draft::standalone_call(&branch, &call_id, &name) {
            return block(
                "Call plan_mode_complete alone, directly, after the draft is ready.".into(),
            );
        }
    }
    match name.as_str() {
        "update_plan" => {
            return block("Plan mode blocks update_plan because it tracks execution progress rather than conversational planning.".into());
        }
        "edit" | "write" => return block(format!("Plan mode blocks mutating tool '{name}'.")),
        _ if helper => return Ok(EventReply::none()),
        _ => {}
    }
    let (tools, active) = app.tools(host).await;
    let is_active = active.contains(&name);
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        return block(if is_active {
            format!(
                "Plan mode blocks tool '{name}' because its safe policy metadata is unavailable."
            )
        } else {
            format!(
                "Plan mode blocks tool '{name}' because it is not registered or active. Register and activate it before starting the next Plan workflow."
            )
        });
    };
    if tool.source.is_none() {
        return block(format!(
            "Plan mode blocks tool '{name}' because its safe policy metadata is unavailable."
        ));
    }
    if tool.classify() == Policy::Blocked {
        return block(format!(
            "Plan mode blocks tool '{name}' because its built-in policy is blocked and settings cannot enable it."
        ));
    }
    let (allowed, desired) = {
        let core = app.lock();
        let allowed = core.allowed.clone().unwrap_or_default();
        let desired = match &core.state.workflow_tool_policy {
            Some(p) if p.automatic() => crate::policy::default_tool_names(&tools, None),
            Some(p) => p.desired_names.clone().unwrap_or_default(),
            None => Vec::new(),
        };
        (allowed, desired)
    };
    let route = if nested { Route::Nested } else { Route::Model };
    let reason = match tool.availability(&active, route) {
        Availability::Available => None,
        Availability::Inactive if allowed.contains(&name) => Some(
            "it was admitted to the active Plan workflow but is currently inactive. Reactivate it to continue without restarting.",
        ),
        Availability::Inactive => Some(
            "it is registered but inactive. Activate it before starting the next Plan workflow.",
        ),
        Availability::Hidden => {
            Some("it is hidden in Pi. Stored selection cannot enable hidden tools.")
        }
        Availability::ModelOnly => Some("it is model-only and cannot be called by another tool."),
        Availability::Unsupported => Some("its tool exposure is unsupported."),
    };
    if let Some(reason) = reason {
        return block(format!("Plan mode blocks tool '{name}' because {reason}"));
    }
    if !allowed.contains(&name) {
        return block(if desired.contains(&name) {
            format!(
                "Plan mode blocks tool '{name}' because it was not available when the active Plan workflow froze its tool policy. Exit Plan mode, then start again after the tool is available."
            )
        } else {
            format!(
                "Plan mode blocks tool '{name}' because it is not selected by the Plan policy. Exit Plan mode, then enable it with /plan tools or defaultPlanTools before starting again."
            )
        });
    }
    let shell = name == "bash" || name == "powershell";
    if !shell {
        return Ok(EventReply::none());
    }
    let safe = app.settings().safe_subcommands;
    let cwd = ev.snapshot().cwd.clone();
    let command = read_command(&input);
    if name == "bash"
        && let Some(blocked) = blocked_bash_segment(command, &safe, Some(&cwd), cfg!(windows))
    {
        return block(format!(
            "Plan mode blocks bash commands outside its reviewed inspection policy or containing explicitly unsafe arguments.\nBlocked command: {blocked}"
        ));
    }
    if name == "powershell"
        && let Some(blocked) = blocked_powershell_segment(command, &safe, Some(&cwd))
    {
        return block(format!(
            "Plan mode blocks PowerShell commands outside its reviewed inspection policy or containing explicitly unsafe syntax.\nBlocked command: {blocked}"
        ));
    }
    Ok(EventReply::none())
}

pub async fn tool_execution_end(app: App, ev: EventCtx) -> Result<()> {
    let id = text(&ev, "toolCallId").to_owned();
    if app.take_pending_abort(&id) {
        ev.host().abort().await?;
    }
    if matches!(text(&ev, "toolName"), "write" | "edit") {
        app.lock().draft.finish(&id);
    }
    Ok(())
}

fn draft_authorization(app: &App, branch: &[Value], state: &PlanState) -> Option<Authorization> {
    let core = app.lock();
    if !state.enabled {
        return None;
    }
    if !core.managed_draft() {
        let forked = state
            .draft
            .as_ref()
            .is_some_and(|d| Some(&d.session_id) != core.session_id.as_ref());
        return (forked || draft::has_historical_grant(branch)).then_some(Authorization::ReadOnly);
    }
    Some(match core.draft.path() {
        Some(path) => Authorization::Managed {
            path: path.to_string_lossy().into_owned(),
        },
        None => Authorization::Unavailable,
    })
}

pub async fn context(app: App, ev: EventCtx) -> Result<EventReply> {
    let host = ev.host();
    app.resolve_pending_policy(host).await?;
    app.resolve_draft_admission(host).await?;
    let (state, relevant) = {
        let core = app.lock();
        (core.state.clone(), core.contracts_relevant)
    };
    if !state.enabled && !relevant && state.active_implementation.is_none() {
        return Ok(EventReply::none());
    }
    let messages = ev
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (messages, clear) = app.lock().retention.transform(messages, &state);
    if let Some(id) = clear {
        app.clear_active_implementation(host, &id).await?;
    }
    let mode = if state.enabled {
        Mode::Plan
    } else {
        Mode::Normal
    };
    let messages = reconcile_mode_contract(messages, mode);
    let expected = state.enabled.then(|| state.interaction());
    let anchors = app.lock().state.interaction_restorations.clone();
    let (messages, anchor) = crate::context::interaction(messages, &anchors, expected);
    if let Some(anchor) = anchor {
        app.lock().state.interaction_restorations.push(anchor);
        app.persist(host).await?;
    }
    let branch = host.branch(None).await?;
    let expected = draft_authorization(&app, &branch, &state);
    let (messages, entry) = draft::reconcile(messages, &branch, expected.as_ref());
    if let Some(entry) = entry {
        host.append_entry(draft::CONTRACT_ENTRY, &entry).await?;
    }
    Ok(EventReply::result(json!({ "messages": messages })))
}

fn latest_assistant(messages: &[Value]) -> Option<&Value> {
    messages
        .iter()
        .rev()
        .map(unwrap)
        .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
}

pub async fn agent_end(app: App, ev: EventCtx) -> Result<()> {
    let host = ev.host();
    let (enabled, managed, questions) = {
        let core = app.lock();
        let questions = core.state.interaction() == Interaction::Questions;
        (core.state.enabled, core.managed_draft(), questions)
    };
    if !enabled {
        return Ok(());
    }
    let messages = ev
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = messages
        .iter()
        .rev()
        .filter(|m| role(m) == Some("assistant"))
        .map(message_text)
        .find(|t| !t.is_empty())
        .unwrap_or_default();
    let parsed = if managed {
        Parsed::Absent
    } else {
        parse_proposed_plan(&text)
    };
    if let Parsed::Valid(plan) = parsed {
        return app.accept_completion(host, &plan, LEGACY_SOURCE).await;
    }
    if questions {
        let stop = latest_assistant(&messages)
            .and_then(|m| m.get("stopReason"))
            .and_then(Value::as_str);
        app.observe_run_end(stop.is_none_or(|s| s == "stop"));
    }
    if let Some(message) = invalid_plan_message(&parsed) {
        app.warn(host, message).await;
    }
    app.persist(host).await?;
    app.update_ui(host).await;
    Ok(())
}

pub async fn agent_settled(app: App, ev: EventCtx) -> Result<()> {
    let host = ev.host().clone();
    let clear = {
        let mut core = app.lock();
        let active = core.state.active_implementation.clone();
        if core.managed_draft() {
            core.draft.settle();
        }
        core.retention.settled(active.as_ref())
    };
    if let Some(id) = clear {
        app.clear_active_implementation(&host, &id).await?;
    }
    app.settle_finalization(&host).await?;
    let Some(intent) = app.take_ready() else {
        return Ok(());
    };
    if intent.source == LEGACY_SOURCE {
        crate::ui::show_plan(&host, "Proposed Plan", &intent.plan).await?;
    }
    if intent.show_menu && host.has_ui() && app.ready_is_current(&intent) {
        // The menu waits for the user; the event answers now.
        tokio::spawn(async move {
            if let Err(err) = crate::command::ready_menu(&app, &host, &intent).await {
                pi_ext_sdk::warn!("plan-mode: ready menu failed: {}", err.message());
            }
        });
    }
    Ok(())
}

pub async fn before_agent_start(app: App, ev: EventCtx) -> Result<()> {
    if text(&ev, "messageType") == "custom" {
        return Ok(());
    }
    app.clear_ready_for_revision(ev.host()).await
}

pub fn message_start(app: &App, ev: &EventCtx) {
    let Some(message) = ev.get("message") else {
        return;
    };
    if role(message) == Some("user") {
        app.observe_user_message(&message_text(message));
    }
}

pub async fn session_before_tree(ev: EventCtx) -> Result<EventReply> {
    let host = ev.host();
    let target = ev
        .event()
        .get("preparation")
        .and_then(|p| p.get("targetId"))
        .cloned()
        .unwrap_or(Value::Null);
    if target.is_null() {
        return Ok(EventReply::none());
    }
    let entry = host
        .pi_call("ctx.sessionManager", "getEntry", vec![target])
        .await
        .unwrap_or(Value::Null);
    let custom = entry.get("type").and_then(Value::as_str) == Some("custom_message");
    let kind = entry.get("customType").and_then(Value::as_str);
    if !custom || !matches!(kind, Some(MODE_CONTRACT_TYPE | INTERACTION_CONTRACT_TYPE)) {
        return Ok(EventReply::none());
    }
    let _ = host
        .notify_warning("Plan mode transition markers are internal. Select the adjacent conversation entry instead.")
        .await;
    Ok(EventReply::cancel())
}
