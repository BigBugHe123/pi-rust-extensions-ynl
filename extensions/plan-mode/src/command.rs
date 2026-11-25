//! `/plan` routes and menus (upstream `plan-mode.ts` command handler, `plan-action-menus.ts`,
//! `plan-launch-menu.ts`, `saved-plan-menu.ts`, `active-implementation-menu.ts`).

use pi_ext_sdk::{CommandCtx, CompletionItem, Error, Host, Result};

use crate::app::{App, ReadyIntent, helpers_available, selectable_tools};
use crate::contracts::Interaction;
use crate::fresh::{self, Runtime};
use crate::policy::{Availability, Route, ToolInfo};
use crate::state::{ModelRef, THINKING_LEVELS};
use crate::{export, settings_ui, ui};

const COMPLETIONS: [(&str, &str); 10] = [
    ("start", "Start Plan mode without sending a prompt"),
    ("show", "Show the ready, saved, or active plan"),
    ("finalize", "Request a completed plan"),
    ("implement", "Implement the completed or saved plan"),
    ("save", "Save the completed plan for later"),
    ("settings", "Open Plan mode settings"),
    ("export", "Export the stored plan to a Markdown file"),
    ("exit", "Leave Plan mode or clear a saved/active plan"),
    ("off", "Leave Plan mode or clear a saved/active plan"),
    ("tools", "Choose tools before starting this Plan workflow"),
];
const IMPLEMENTATION_LINES: [&str; 2] = [
    "Implement here keeps this planning conversation.",
    "Start fresh transfers only the approved plan to a new session.",
];

pub fn complete(prefix: &str) -> Option<Vec<CompletionItem>> {
    let prefix = prefix.trim_start().to_lowercase();
    if prefix.contains(char::is_whitespace) {
        return None;
    }
    let items: Vec<CompletionItem> = COMPLETIONS
        .iter()
        .filter(|(value, _)| value.starts_with(&prefix))
        .map(|(value, description)| {
            CompletionItem::new(*value)
                .label(*value)
                .description(*description)
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

fn mode(host: &Host) -> Option<String> {
    host.snapshot().and_then(|s| s.mode.clone())
}

fn print_mode(host: &Host) -> bool {
    matches!(mode(host).as_deref(), Some("print" | "json"))
}

/// One row of a `ui.select` menu.
struct Item {
    id: &'static str,
    label: String,
}

fn item(id: &'static str, label: impl Into<String>) -> Item {
    Item {
        id,
        label: label.into(),
    }
}

/// Shows `items` and returns the id of the chosen row; `None` when dismissed.
async fn choose(host: &Host, title: &str, items: &[Item]) -> Result<Option<&'static str>> {
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    let Some(choice) = host.select(title, &labels).await? else {
        return Ok(None);
    };
    Ok(items.iter().find(|i| i.label == choice).map(|i| i.id))
}

pub fn retention_preview(retention: &str) -> &'static str {
    match retention {
        "clear-after-first-run" => "Plan reinjection: Through the first implementation run.",
        "keep" => "Plan reinjection: Until /plan exit.",
        _ => "Plan reinjection: Off; use conversation history only.",
    }
}

async fn status_text(app: &App, host: &Host) -> String {
    let (tools, active) = app.tools(host).await;
    let core = app.lock();
    let summary = ui::tool_summary(&core.policy_names(&tools, &active));
    let managed = core.state.enabled
        && core
            .state
            .draft
            .as_ref()
            .is_some_and(|d| d.status == "active")
        && core.draft.path().is_some();
    ui::status_text(&core.state, &summary, managed)
}

pub async fn run(app: App, args: String, ctx: CommandCtx) -> Result<()> {
    let host = ctx.host().clone();
    app.refresh_settings();
    let prompt = args.trim();
    let command = prompt.to_lowercase();
    let (enabled, saved, active, ready) = {
        let state = &app.lock().state;
        (
            state.enabled,
            state.saved_plan.is_some(),
            state.active_implementation.is_some(),
            state.ready_plan().is_some(),
        )
    };
    let blocked_by_saved = saved && !enabled;
    if let Some(path) = export_argument(prompt) {
        export_plan(&app, &host, path).await?;
        return Ok(());
    }
    match command.as_str() {
        "start" => {
            if blocked_by_saved {
                return app.saved_plan_blocks(&host).await;
            }
            if enabled {
                app.notify(&host, "Plan mode is already active.").await;
            } else {
                app.enter(&host, None).await?;
            }
        }
        "show" => show(&app, &host).await?,
        "finalize" => app.finalize(&host).await?,
        "implement" => {
            if !ready && !saved {
                app.warn(&host, "No completed plan is available to implement.")
                    .await;
                return Ok(());
            }
            app.implement_here(&host).await?;
        }
        "save" => app.save(&host).await?,
        "settings" => {
            if !host.has_ui() {
                return Err(Error::msg(
                    "/plan settings requires TUI or RPC mode and is unavailable here.",
                ));
            }
            settings_ui::show(&app, &host).await?;
        }
        "exit" | "off" => app.exit_and_notify(&host).await?,
        "tools" => {
            if blocked_by_saved {
                return app.saved_plan_blocks(&host).await;
            }
            if enabled {
                let message = "Plan-mode tools are locked while Planning is active. Exit Plan mode and choose tools before starting again.";
                return app.report(&host, message, false).await;
            }
            if !host.has_ui() {
                return Err(Error::msg(
                    "/plan tools requires TUI or RPC mode and is unavailable here.",
                ));
            }
            choose_tools_then_start(&app, &host).await?;
        }
        "" => {
            if !host.has_ui() {
                return Err(Error::msg(
                    "The interactive /plan menu is unavailable in print and JSON modes. Use /plan start or /plan <prompt>.",
                ));
            }
            if enabled {
                current_menu(&app, &host).await?;
            } else if active {
                active_menu(&app, &host).await?;
            } else if saved {
                saved_menu(&app, &host).await?;
            } else {
                launch_menu(&app, &host).await?;
            }
        }
        _ => {
            if blocked_by_saved {
                return app.saved_plan_blocks(&host).await;
            }
            app.enter_with_prompt(&host, prompt).await?;
        }
    }
    Ok(())
}

/// `export` or `export <path>` (any case); `Some(None)` is the default destination.
fn export_argument(prompt: &str) -> Option<Option<&str>> {
    let head = prompt.get(..6)?;
    if !head.eq_ignore_ascii_case("export") {
        return None;
    }
    let rest = &prompt[6..];
    if rest.is_empty() {
        return Some(None);
    }
    rest.starts_with(char::is_whitespace)
        .then(|| Some(rest.trim()).filter(|r| !r.is_empty()))
}

pub async fn show(app: &App, host: &Host) -> Result<()> {
    let state = app.state();
    if state.saved_plan.is_some() && print_mode(host) {
        return Err(Error::msg(
            "Saved plan display is unavailable in print/JSON mode. Use TUI or RPC.",
        ));
    }
    let (title, plan) = if let Some(plan) = state.ready_plan() {
        ("Proposed Plan", plan)
    } else if let Some(saved) = &state.saved_plan {
        ("Saved Plan", saved.plan.as_str())
    } else if let Some(active) = &state.active_implementation {
        ("Active Implementation Plan", active.plan.as_str())
    } else {
        app.notify(
            host,
            "No completed plan is available. Use /plan finalize when planning is complete.",
        )
        .await;
        return Ok(());
    };
    if let Err(err) = ui::show_plan(host, title, plan).await {
        let _ = host
            .notify_error(format!("Unable to show completed plan: {}", err.message()))
            .await;
    }
    Ok(())
}

/// Writes the stored plan. A ready plan's export also ends Plan mode.
pub async fn export_plan(app: &App, host: &Host, path: Option<&str>) -> Result<bool> {
    let state = app.state();
    let Some(plan) = state.stored_plan().map(str::to_owned) else {
        app.report(host, "No completed plan is available to export. Use /plan finalize when planning is complete.", false)
            .await?;
        return Ok(false);
    };
    let ends_workflow = state.ready_plan().is_some();
    if ends_workflow
        && !app
            .allow_transition(host, "export the ready plan and leave Plan mode")
            .await?
    {
        return Ok(false);
    }
    let settings = app.settings();
    let written = export::resolve(path, &host.cwd(), settings.export_path())
        .and_then(|target| export::write(&plan, &target).map(|()| target));
    let target = match written {
        Ok(target) => target,
        Err(error) => {
            let message = ui::safe_text(&format!("Unable to export plan: {error}"));
            app.report(host, message, true).await?;
            return Ok(false);
        }
    };
    let ended = ends_workflow && app.exit(host).await?;
    let detail = if ended { " Plan mode disabled." } else { "" };
    let message = format!("Plan exported to {}.{detail}", target.display());
    app.notify(host, ui::safe_text(&message)).await;
    Ok(true)
}

/// Asks for a destination; an empty answer uses the configured one. Retries after a failure.
async fn export_dialog(app: &App, host: &Host) -> Result<bool> {
    loop {
        let settings = app.settings();
        let configured = settings.export_path();
        let resolved = export::resolve(None, &host.cwd(), configured)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let title = format!(
            "Export plan\nExisting paths are never overwritten.\nDefault: {}\nResolves to: {}",
            ui::safe_text(configured),
            ui::safe_text(&resolved)
        );
        let Some(answer) = host.input(&title, Some(configured)).await? else {
            return Ok(false);
        };
        let path = Some(answer.trim()).filter(|p| !p.is_empty());
        if export_plan(app, host, path).await? {
            return Ok(true);
        }
        if app.state().stored_plan().is_none() {
            return Ok(false);
        }
    }
}

async fn launch_menu(app: &App, host: &Host) -> Result<()> {
    loop {
        let (tools, active) = app.tools(host).await;
        let status = if helpers_available(&active) {
            "Status: Off — visible Plan helpers stay inactive until /plan starts."
        } else {
            "Status: Off — required Plan helpers are unavailable under the active tool policy."
        };
        let selected = app.lock().desired_names(&tools, &active);
        let summary = launch_summary(&tools, &active, &selected);
        let title = format!("Plan mode\n{status}\n{summary}");
        let items = [
            item("start", "Start Plan mode"),
            item("tools", "Choose tools, then start…"),
            item("settings", "Settings"),
            item("help", "How Plan mode works"),
        ];
        match choose(host, &title, &items).await? {
            Some("start") => return app.enter(host, None).await.map(drop),
            Some("tools") => return choose_tools_then_start(app, host).await,
            Some("settings") => settings_ui::show(app, host).await?,
            Some("help") => {
                let help = "How Plan mode works\nPlan mode uses read-only exploration to understand the project before implementation.\nThe agent can ask important decision questions, then returns a complete implementation-ready plan.\nProject files stay read-only until implementation. Eligible workflows can edit a private Markdown draft.";
                host.select(help, &["Back"]).await?;
            }
            _ => return Ok(()),
        }
    }
}

fn launch_summary(tools: &[ToolInfo], active: &[String], selected: &[String]) -> String {
    let allowed: Vec<&str> = tools
        .iter()
        .filter(|t| {
            selected.contains(&t.name)
                && t.selectable()
                && t.availability(active, Route::Selection) == Availability::Available
        })
        .map(|t| t.name.as_str())
        .collect();
    let pending: Vec<&str> = selected
        .iter()
        .filter(|n| !tools.iter().any(|t| t.name == **n))
        .map(String::as_str)
        .collect();
    let allowed = if allowed.is_empty() {
        "none".to_owned()
    } else {
        allowed.join(", ")
    };
    let mut summary = format!("Plan policy will allow: {allowed}.");
    if !pending.is_empty() {
        let shown: Vec<&str> = pending.iter().take(3).copied().collect();
        let more = pending.len().saturating_sub(3);
        let suffix = if more > 0 {
            format!(", +{more} more")
        } else {
            String::new()
        };
        summary.push_str(&format!(
            " Pending registration: {}{suffix}.",
            shown.join(", ")
        ));
    }
    summary
}

/// The `/plan tools` picker; starts Plan mode with the chosen allowlist.
async fn choose_tools_then_start(app: &App, host: &Host) -> Result<()> {
    let (tools, active) = app.tools(host).await;
    let initial = app.lock().desired_names(&tools, &active);
    let lines = [
        "Choose Plan policy allowlist",
        "Policy changes apply only when you start Plan mode.",
        "Plan mode never activates tools; tools outside its reviewed core policy run at user risk.",
    ];
    let picked = pick_tools(host, &lines.join("\n"), &tools, &active, initial, None).await?;
    let Some(Picked::Names(names)) = picked else {
        return Ok(());
    };
    app.enter(host, Some(names)).await.map(drop)
}

pub enum Picked {
    Names(Vec<String>),
    Automatic,
}

/// Why a tool row cannot be toggled, as upstream `planModeToolSelection`.
fn tool_row(tool: &ToolInfo, active: &[String]) -> (String, Option<String>) {
    let availability = tool.availability(active, Route::Selection);
    let blocked = !tool.selectable();
    let unavailable = match availability {
        Availability::Inactive => {
            Some("Not active in Pi; Plan mode will not activate it".to_owned())
        }
        Availability::Hidden => {
            Some("Hidden in Pi; retained selection cannot enable it".to_owned())
        }
        Availability::Unsupported => {
            Some("Unsupported tool exposure; Plan mode cannot enable it".to_owned())
        }
        _ => None,
    };
    let label = if blocked {
        format!("{} — blocked by Plan policy", tool.name)
    } else if availability == Availability::Inactive {
        format!("{} — inactive in Pi", tool.name)
    } else {
        format!("{} — {}", tool.name, tool.policy_label())
    };
    let reason = if blocked {
        Some("Blocked by Plan-mode policy".to_owned())
    } else {
        unavailable
    };
    (ui::safe_text(&label), reason)
}

/// A checklist on `ui.select`: each pick toggles a row until Done. `automatic` adds a row that
/// clears the explicit list.
pub async fn pick_tools(
    host: &Host,
    title: &str,
    tools: &[ToolInfo],
    active: &[String],
    initial: Vec<String>,
    automatic: Option<&str>,
) -> Result<Option<Picked>> {
    let tools = selectable_tools(tools);
    let mut selected = initial;
    loop {
        let mut labels = vec!["Done — start with this policy".to_owned()];
        if let Some(label) = automatic {
            labels[0] = "Done — save this allowlist".to_owned();
            labels.push(label.to_owned());
        }
        let mut rows: Vec<(String, Option<String>, String)> = Vec::new();
        for tool in &tools {
            let (label, reason) = tool_row(tool, active);
            let mark = if selected.contains(&tool.name) {
                "[x]"
            } else {
                "[ ]"
            };
            rows.push((format!("{mark} {label}"), reason, tool.name.clone()));
        }
        for name in selected
            .iter()
            .filter(|n| !tools.iter().any(|t| t.name == **n))
        {
            let label = format!("[x] {} — pending registration", ui::safe_text(name));
            rows.push((label, None, name.clone()));
        }
        labels.extend(rows.iter().map(|(label, _, _)| label.clone()));
        let Some(choice) = host.select(title, &labels).await? else {
            return Ok(None);
        };
        if choice == labels[0] {
            return Ok(Some(Picked::Names(selected)));
        }
        if automatic.is_some() && choice == labels[1] {
            return Ok(Some(Picked::Automatic));
        }
        let Some((_, reason, name)) = rows.iter().find(|(label, _, _)| *label == choice) else {
            continue;
        };
        if let Some(reason) = reason
            && !selected.contains(name)
        {
            let _ = host.notify_warning(reason.clone()).await;
            continue;
        }
        if let Some(index) = selected.iter().position(|n| n == name) {
            selected.remove(index);
        } else {
            selected.push(name.clone());
        }
    }
}

fn toggle_item(mode: Interaction) -> Item {
    match mode {
        Interaction::Discussion => item("toggle", "Enable questionnaires"),
        Interaction::Questions => item("toggle", "Switch to free discussion"),
    }
}

async fn current_menu(app: &App, host: &Host) -> Result<()> {
    let status = status_text(app, host).await;
    let (ready, mode) = {
        let state = &app.lock().state;
        (state.latest_plan.is_some(), state.interaction())
    };
    let mut lines = vec![status];
    if ready {
        lines.extend(IMPLEMENTATION_LINES.map(String::from));
        lines.push(retention_preview(app.settings().retention()).to_owned());
    }
    let title = format!("Plan mode\n{}", lines.join("\n"));
    let items = if ready {
        vec![
            item("show", "Show latest proposed plan"),
            item(
                "here",
                "Implement here — Continue in this session with the planning conversation.",
            ),
            item(
                "fresh",
                "Start fresh and implement — Configure one-shot model and thinking choices first.",
            ),
            item("export", "Export plan…"),
            item("save", "Save for later"),
            toggle_item(mode),
            item("stay", "Stay in Plan mode"),
            item("exit", "Discard plan and exit"),
        ]
    } else {
        vec![
            item("finalize", "Request final plan"),
            toggle_item(mode),
            item("stay", "Stay in Plan mode"),
            item("exit", "Exit Plan mode"),
        ]
    };
    match choose(host, &title, &items).await? {
        Some("show") => show(app, host).await?,
        Some("here") => app.implement_here(host).await?,
        Some("fresh") => fresh_flow(app, host).await?,
        Some("export") => {
            export_dialog(app, host).await?;
        }
        Some("save") => app.save(host).await?,
        Some("toggle") => {
            app.toggle_interaction(host, false).await?;
        }
        Some("finalize") => app.finalize(host).await?,
        Some("exit") => app.exit_and_notify(host).await?,
        _ => app.update_ui(host).await,
    }
    Ok(())
}

/// The menu that opens after a completed plan when questionnaires are on.
pub async fn ready_menu(app: &App, host: &Host, intent: &ReadyIntent) -> Result<()> {
    let mut lines = vec!["Proposed plan ready. What next?".to_owned()];
    lines.extend(IMPLEMENTATION_LINES.map(String::from));
    lines.push(retention_preview(app.settings().retention()).to_owned());
    let items = [
        item(
            "here",
            "Implement here — Continue in this session with the planning conversation.",
        ),
        item(
            "fresh",
            "Start fresh and implement — Configure one-shot model and thinking choices first.",
        ),
        item("export", "Export plan…"),
        item("save", "Save for later"),
        item("stay", "Stay in Plan mode"),
        item("exit", "Discard plan and exit"),
    ];
    let choice = choose(host, &lines.join("\n"), &items).await?;
    if !app.ready_is_current(intent) {
        return Ok(());
    }
    match choice {
        Some("here") => app.implement_here(host).await?,
        Some("fresh") => fresh_flow(app, host).await?,
        Some("export") => {
            export_dialog(app, host).await?;
        }
        Some("save") => app.save(host).await?,
        Some("exit") => app.exit_and_notify(host).await?,
        _ => {}
    }
    Ok(())
}

async fn saved_menu(app: &App, host: &Host) -> Result<()> {
    loop {
        let mut lines = vec![
            "Saved plan".to_owned(),
            "A plan is saved for later.".to_owned(),
        ];
        lines.extend(IMPLEMENTATION_LINES.map(String::from));
        lines.push(retention_preview(app.settings().retention()).to_owned());
        let items = [
            item("show", "Show saved plan"),
            item(
                "here",
                "Implement here — Continue in this session with the planning conversation.",
            ),
            item(
                "fresh",
                "Start fresh and implement — Open a new linked session; transfer only the approved plan.",
            ),
            item("export", "Export plan…"),
            item("settings", "Settings"),
            item("clear", "Clear saved plan"),
        ];
        match choose(host, &lines.join("\n"), &items).await? {
            Some("show") => return show(app, host).await,
            Some("here") => return app.implement_here(host).await,
            Some("fresh") => {
                let runtime = configured_runtime(app, host).await;
                return fresh::start(app, host, runtime).await;
            }
            Some("export") => {
                if export_dialog(app, host).await? {
                    return Ok(());
                }
            }
            Some("settings") => settings_ui::show(app, host).await?,
            Some("clear") => {
                if app.exit(host).await? {
                    app.notify(host, "Saved plan cleared.").await;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

async fn active_menu(app: &App, host: &Host) -> Result<()> {
    loop {
        let title = "Active implementation plan\nAn implementation plan is active.";
        let items = [
            item("show", "Show active implementation plan"),
            item("export", "Export plan…"),
            item("settings", "Settings"),
            item("new", "Start a new plan"),
            item("clear", "Clear active implementation plan"),
        ];
        match choose(host, title, &items).await? {
            Some("show") => return show(app, host).await,
            Some("export") => {
                if export_dialog(app, host).await? {
                    return Ok(());
                }
            }
            Some("settings") => settings_ui::show(app, host).await?,
            Some("new") => return app.enter(host, None).await.map(drop),
            Some("clear") => {
                if app.exit(host).await? {
                    app.notify(host, "Active implementation plan cleared.")
                        .await;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

fn model_label(model: &ModelRef) -> String {
    ui::safe_text(&format!("{} [{}]", model.model_id, model.provider))
}

fn current_model(host: &Host) -> Option<ModelRef> {
    let model = host.snapshot()?.model.clone()?;
    Some(ModelRef {
        provider: model.provider,
        model_id: model.id,
    })
}

/// The persistent fresh defaults, with an unavailable model falling back to the plan model.
async fn configured_runtime(app: &App, host: &Host) -> Runtime {
    let settings = app.settings();
    let models = fresh::available_models(host).await;
    let configured = settings
        .implementation_model
        .clone()
        .filter(|m| models.iter().any(|a| a.model == *m));
    if settings.implementation_model.is_some() && configured.is_none() {
        app.warn(
            host,
            "The configured fresh implementation model is unavailable; using the planning model.",
        )
        .await;
    }
    let thinking = match settings.implementation_thinking {
        Some(level) => Some(level),
        None => host.thinking_level().await.ok().flatten(),
    };
    Runtime {
        model: configured.or_else(|| current_model(host)),
        thinking,
    }
}

/// The fresh implementation screen: one-shot model and thinking choices, then the handoff.
async fn fresh_flow(app: &App, host: &Host) -> Result<()> {
    if !fresh::command_context(host).await {
        app.warn(host, "Fresh implementation requires the interactive /plan command. Reopen /plan and try again.")
            .await;
        return Ok(());
    }
    let settings = app.settings();
    let models = fresh::available_models(host).await;
    let plan_model = current_model(host);
    let plan_thinking = host.thinking_level().await.ok().flatten();
    let configured = settings.implementation_model.clone();
    let mut model = configured
        .clone()
        .filter(|m| models.iter().any(|a| a.model == *m));
    let mut unavailable = configured.is_some() && model.is_none();
    let mut thinking = settings.implementation_thinking.clone();
    loop {
        let mut lines = vec![
            "Fresh implementation settings".to_owned(),
            "These choices apply once to the new implementation session.".to_owned(),
        ];
        if unavailable && let Some(configured) = &configured {
            lines.push(format!(
                "Configured default {} is unavailable; using same as plan.",
                model_label(configured)
            ));
        }
        let model_text = match (&model, &plan_model) {
            (Some(m), _) => model_label(m),
            (None, Some(p)) => format!("{} · same as plan", model_label(p)),
            (None, None) => "Same as plan".to_owned(),
        };
        let thinking_text = match (&thinking, &plan_thinking) {
            (Some(t), _) => t.clone(),
            (None, Some(p)) => format!("{p} · same as plan"),
            (None, None) => "Same as plan".to_owned(),
        };
        let items = [
            item(
                "start",
                "Start fresh implementation — Create the linked session and begin implementation.",
            ),
            item("model", format!("Model: {model_text}")),
            item("thinking", format!("Thinking level: {thinking_text}")),
        ];
        match choose(host, &lines.join("\n"), &items).await? {
            Some("start") => {
                let runtime = Runtime {
                    model: model.clone().or_else(|| plan_model.clone()),
                    thinking: thinking.clone().or_else(|| plan_thinking.clone()),
                };
                return fresh::start(app, host, runtime).await;
            }
            Some("model") => {
                if let Some(choice) = pick_model(host, &models, plan_model.as_ref()).await? {
                    model = choice.filter(|m| Some(m) != plan_model.as_ref());
                    unavailable = false;
                }
            }
            Some("thinking") => {
                if let Some(choice) = pick_thinking(
                    host,
                    "Implementation thinking level",
                    plan_thinking.as_deref(),
                )
                .await?
                {
                    thinking = choice;
                }
            }
            _ => return Ok(()),
        }
    }
}

/// `Some(None)` is "Same as plan"; `None` is dismissed.
pub async fn pick_model(
    host: &Host,
    models: &[fresh::AvailableModel],
    plan: Option<&ModelRef>,
) -> Result<Option<Option<ModelRef>>> {
    let mut filter = String::new();
    loop {
        let same = match plan {
            Some(plan) => format!("Same as plan — {}", model_label(plan)),
            None => "Same as plan".to_owned(),
        };
        let filter_label = if filter.is_empty() {
            "Filter models…".to_owned()
        } else {
            format!("Filter models… (now: {})", ui::safe_text(&filter))
        };
        let needle = filter.to_lowercase();
        let shown: Vec<&fresh::AvailableModel> = models
            .iter()
            .filter(|m| needle.is_empty() || m.search.to_lowercase().contains(&needle))
            .collect();
        let mut labels = vec![same, filter_label];
        labels.extend(shown.iter().map(|m| {
            let mark = if Some(&m.model) == plan { "✓ " } else { "" };
            let name = m
                .name
                .as_deref()
                .map(|n| format!(" · {}", ui::safe_text(n)))
                .unwrap_or_default();
            format!("{mark}{}{name}", model_label(&m.model))
        }));
        let Some(choice) = host.select("Implementation model", &labels).await? else {
            return Ok(None);
        };
        if choice == labels[0] {
            return Ok(Some(None));
        }
        if choice == labels[1] {
            filter = host
                .input("Filter models by provider, id, or name", Some("claude"))
                .await?
                .unwrap_or_default();
            continue;
        }
        if let Some(index) = labels.iter().skip(2).position(|l| *l == choice) {
            return Ok(Some(Some(shown[index].model.clone())));
        }
    }
}

const THINKING_DESCRIPTIONS: [&str; 7] = [
    "No reasoning",
    "Very brief reasoning (~1k tokens)",
    "Light reasoning (~2k tokens)",
    "Moderate reasoning (~8k tokens)",
    "Deep reasoning (~16k tokens)",
    "Extra-high reasoning (~32k tokens)",
    "Maximum reasoning",
];

/// `Some(None)` is "Same as plan"; `None` is dismissed.
pub async fn pick_thinking(
    host: &Host,
    title: &str,
    plan: Option<&str>,
) -> Result<Option<Option<String>>> {
    let mut labels = vec!["Same as plan".to_owned()];
    labels.extend(
        THINKING_LEVELS
            .iter()
            .zip(THINKING_DESCRIPTIONS)
            .map(|(level, description)| {
                let mark = if Some(*level) == plan { "✓ " } else { "" };
                format!("{mark}{level} — {description}")
            }),
    );
    let Some(choice) = host.select(title, &labels).await? else {
        return Ok(None);
    };
    if choice == labels[0] {
        return Ok(Some(None));
    }
    let index = labels.iter().position(|l| *l == choice).unwrap_or(1);
    Ok(Some(
        THINKING_LEVELS.get(index - 1).map(|l| (*l).to_owned()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_subcommands() {
        let all = complete("").unwrap();
        assert_eq!(all.len(), COMPLETIONS.len());
        let ex = complete("ex").unwrap();
        assert_eq!(ex.len(), 2);
        assert!(complete("start a").is_none());
        assert!(complete("zzz").is_none());
    }

    #[test]
    fn parses_export_arguments() {
        assert_eq!(export_argument("export"), Some(None));
        assert_eq!(export_argument("Export docs/P.md"), Some(Some("docs/P.md")));
        assert_eq!(export_argument("exporting"), None);
        assert_eq!(export_argument("start"), None);
    }
}
