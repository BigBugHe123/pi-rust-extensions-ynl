//! Status key, editor widget, and status texts (upstream `presentation.ts`).

use pi_ext_sdk::{CustomMessage, Host, Placement, Result, style};

use crate::contracts::{Interaction, PROPOSED_PLAN_TYPE};
use crate::state::PlanState;

pub const STATUS_KEY: &str = "plan-mode";
pub const WIDGET_KEY: &str = "plan-mode-plan";

pub fn tool_summary(names: &[String]) -> String {
    let names = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    };
    format!("Plan policy allows: {names}. Model-visible tools stay unchanged.")
}

pub fn status(state: &PlanState) -> Option<&'static str> {
    if state.enabled {
        if state.awaiting_action || state.latest_plan.is_some() {
            return Some("plan ready");
        }
        return Some(match state.interaction() {
            Interaction::Discussion => "plan discussion",
            Interaction::Questions => "plan active",
        });
    }
    if state.saved_plan.is_some() {
        return Some("plan saved");
    }
    state
        .active_implementation
        .as_ref()
        .map(|_| "plan implementing")
}

fn interaction_hint(state: &PlanState, shortcut: Option<&str>) -> String {
    let action = match state.interaction() {
        Interaction::Discussion => "enable questionnaires",
        Interaction::Questions => "switch to free discussion",
    };
    match shortcut {
        Some(key) => format!("{key}: {action}."),
        None => format!("Use /plan to {action}."),
    }
}

/// The widget summary and body lines; `None` hides the widget.
pub fn widget(
    state: &PlanState,
    tools: &str,
    shortcut: Option<&str>,
    managed_draft: bool,
) -> Option<(String, Vec<String>)> {
    let discussion = state.interaction() == Interaction::Discussion;
    let hint = interaction_hint(state, shortcut);
    if state.enabled && state.latest_plan.is_some() {
        let summary = if discussion {
            "Ready · Free discussion"
        } else {
            "Ready"
        };
        let body = vec![
            "File edits remain blocked.".to_owned(),
            "Use /plan to implement, save, revise, or exit Plan mode.".to_owned(),
            hint,
        ];
        return Some((summary.to_owned(), body));
    }
    if state.enabled {
        let edits = if managed_draft {
            "Only the managed Markdown draft can be edited."
        } else {
            "File edits remain blocked."
        };
        let mut body = vec![edits.to_owned(), tools.to_owned()];
        if discussion {
            body.push("Plain-text questions are allowed; questionnaires are paused.".into());
        }
        body.push(hint);
        let summary = if discussion {
            "Free discussion"
        } else {
            "Planning"
        };
        return Some((summary.to_owned(), body));
    }
    if state.saved_plan.is_some() {
        let body = vec!["Use /plan to show, implement, or clear it.".to_owned()];
        return Some(("Saved".to_owned(), body));
    }
    state.active_implementation.as_ref().map(|_| {
        let body = vec!["Use /plan to show, replace, or clear it.".to_owned()];
        ("Implementation active".to_owned(), body)
    })
}

/// Text for the `/plan` menus and for `/plan` without a UI.
pub fn status_text(state: &PlanState, tools: &str, managed_draft: bool) -> String {
    if state.enabled {
        let ready = if state.latest_plan.is_some() {
            " A proposed plan is ready."
        } else {
            ""
        };
        let interaction = match state.interaction() {
            Interaction::Discussion => {
                "Free discussion is active; questionnaires are paused and plain-text questions are allowed."
            }
            Interaction::Questions => {
                "Explore, ask, and finish with plan_mode_complete when decision-ready."
            }
        };
        let edits = if managed_draft && state.latest_plan.is_none() {
            "Only the managed Markdown draft can be edited."
        } else {
            "File edits remain blocked."
        };
        return format!("Plan mode is active.{ready} {interaction} {edits} {tools}");
    }
    if state.saved_plan.is_some() {
        return "A plan is saved for later.".into();
    }
    if state.active_implementation.is_some() {
        return "An implementation plan is active.".into();
    }
    "Plan mode is off.".into()
}

/// Removes control characters and bidirectional overrides from text shown in the terminal.
pub fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            let code = c as u32;
            if code <= 0x1f
                || (0x7f..=0x9f).contains(&code)
                || (0x202a..=0x202e).contains(&code)
                || (0x2066..=0x2069).contains(&code)
            {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

pub async fn publish(
    host: &Host,
    state: &PlanState,
    tools: &str,
    shortcut: Option<&str>,
    managed_draft: bool,
) -> Result<()> {
    host.set_status(STATUS_KEY, status(state)).await?;
    let tui = host.snapshot().and_then(|s| s.mode.clone()).as_deref() == Some("tui");
    match widget(state, tools, shortcut.filter(|_| tui), managed_draft) {
        None => host.clear_widget(WIDGET_KEY).await,
        Some((summary, body)) => {
            let mut lines = vec![format!("{} · {}", style::bold("Plan mode"), summary)];
            lines.extend(body.iter().map(|line| style::dim(&safe_text(line))));
            host.set_widget(WIDGET_KEY, lines, Placement::AboveEditor)
                .await
        }
    }
}

pub async fn clear(host: &Host) -> Result<()> {
    host.set_status(STATUS_KEY, None).await?;
    host.clear_widget(WIDGET_KEY).await
}

/// Shows a plan in the conversation as a `proposed-plan` message without starting a turn.
pub async fn show_plan(host: &Host, title: &str, plan: &str) -> Result<()> {
    let message = CustomMessage::new(PROPOSED_PLAN_TYPE, format!("**{title}**\n\n{plan}"))
        .display(true)
        .trigger_turn(false);
    host.send_message(message).await
}
