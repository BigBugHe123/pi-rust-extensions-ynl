//! Workflow state and transitions: start, exit, save, implement, completion, interaction toggle,
//! finalization, and thinking level (upstream `plan-mode.ts`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use pi_ext_sdk::{CancellationToken, CustomMessage, DeliverAs, Error, Host, Result};
use serde_json::{Value, json};

use crate::context::Retention;
use crate::contracts::{
    DISCUSSION_FINALIZE_PLAN_PROMPT, FINALIZE_PLAN_PROMPT, HISTORY_IMPLEMENTATION_PROMPT,
    INTERACTION_CONTRACT_TYPE, Interaction, MODE_CONTRACT_TYPE, Mode, draft_finalization_prompt,
    implementation_handoff, interaction_contract_content, interaction_contract_details,
    mode_contract_content, mode_contract_details, transferred_plan_prompt,
};
use crate::draft::Draft;
use crate::policy::{Availability, Route, ToolInfo, default_tool_names, unique};
use crate::question::QUESTION_TOOL;
use crate::settings::{self, Settings};
use crate::state::{
    ActiveImplementation, COMPLETE_TOOL, DraftIdentity, LEGACY_SOURCE, PlanState, STATE_ENTRY,
    SavedPlan, ToolPolicy, validate_plan,
};
use crate::ui;

const PLAN_ENABLED: &str =
    "Plan mode enabled. Project files remain read-only; a managed draft may be available.";
const DEFAULT_ACTIVE_TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];
const DEFERRED_SEARCH: &str = "deferred_search";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyStep {
    Resolve,
    Revalidate,
}

/// A completed plan waiting for `agent_settled` to show the ready menu.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadyIntent {
    pub plan: String,
    pub source: String,
    pub show_menu: bool,
}

/// An explicit `/plan finalize` request that may be retried once.
#[derive(Debug, Clone, Copy)]
struct Finalization {
    generation: u64,
    retried: bool,
    awaiting: bool,
}

pub struct Core {
    pub state: PlanState,
    persisted: Option<String>,
    pub settings: settings::Snapshot,
    settings_stamp: Vec<Option<(std::time::SystemTime, u64)>>,
    /// The frozen allowlist of the active workflow.
    pub allowed: Option<Vec<String>>,
    pending_policy: Option<(u64, PolicyStep)>,
    published: Option<Mode>,
    pub contracts_relevant: bool,
    pub ready: Option<ReadyIntent>,
    finalization: Option<Finalization>,
    pub workflow_gen: u64,
    pub draft: Draft,
    pub retention: Retention,
    pending_aborts: Vec<String>,
    questions: Vec<(u64, CancellationToken)>,
    next_question: u64,
    interaction_failed: bool,
    pub session_id: Option<String>,
    shutdown: bool,
    /// Levels plan-mode set whose `thinking_level_select` event has not arrived yet.
    thinking_echoes: Vec<String>,
    /// Warnings already shown in this workflow.
    warned: Vec<String>,
    /// The branch has `plan-mode-draft-contract` entries, so the `context` hook reads it.
    pub draft_contracts: bool,
    /// F2 toggles questionnaires; registered at load when no pi keybinding uses it.
    pub f2: bool,
    /// The toggle shortcut registered at load.
    pub toggle_shortcut: Option<String>,
}

impl Core {
    pub fn settings(&self) -> Settings {
        self.settings.settings()
    }

    pub fn managed_draft(&self) -> bool {
        let state = &self.state;
        state.enabled
            && !self.shutdown
            && state
                .workflow_tool_policy
                .as_ref()
                .is_some_and(ToolPolicy::automatic)
            && state.draft.as_ref().is_some_and(|d| {
                d.status == "active" && Some(&d.session_id) == self.session_id.as_ref()
            })
    }

    fn advance_workflow(&mut self) {
        self.draft.reset();
        self.warned.clear();
        self.ready = None;
        self.finalization = None;
        self.pending_policy = None;
        self.workflow_gen += 1;
        for (_, token) in self.questions.drain(..) {
            token.cancel();
        }
    }

    pub fn disable_notification(&self) -> &'static str {
        if self.state.active_implementation.is_some() {
            "Active implementation plan cleared."
        } else if self.state.saved_plan.is_some() {
            "Saved plan cleared."
        } else if self.state.latest_plan.is_some() {
            "Plan mode disabled. Proposed plan discarded."
        } else {
            "Plan mode disabled."
        }
    }

    fn explicit_selection(&self) -> bool {
        self.state.selected_tool_names.is_some() || self.settings().default_plan_tools.is_some()
    }

    /// The names the Plan policy allows now: the frozen list while active, else the next one.
    pub fn policy_names(&self, tools: &[ToolInfo], active: &[String]) -> Vec<String> {
        if self.state.enabled {
            return self.allowed.clone().unwrap_or_default();
        }
        resolve_names(tools, active, &self.desired_names(tools, active))
    }

    pub fn desired_names(&self, tools: &[ToolInfo], active: &[String]) -> Vec<String> {
        let available = available_tools(tools, active);
        match &self.state.selected_tool_names {
            Some(names) => unique(names),
            None => default_tool_names(&available, self.settings().default_plan_tools.as_deref()),
        }
    }

    fn begin_tool_policy(&mut self, tools: &[ToolInfo], active: &[String]) {
        let explicit = self.explicit_selection();
        let desired = self.desired_names(tools, active);
        let allowed = resolve_names(tools, active, &desired);
        self.state.workflow_tool_policy = Some(ToolPolicy {
            kind: if explicit { "explicit" } else { "automatic" }.into(),
            desired_names: explicit.then_some(desired),
            allowed_names: allowed.clone(),
            resolved: false,
        });
        self.pending_policy = Some((self.workflow_gen, PolicyStep::Resolve));
        self.allowed = Some(allowed);
    }

    fn policy_target(
        &self,
        policy: &ToolPolicy,
        tools: &[ToolInfo],
        active: &[String],
    ) -> Vec<String> {
        if policy.automatic() {
            default_tool_names(&available_tools(tools, active), None)
        } else {
            policy.desired_names.clone().unwrap_or_default()
        }
    }

    fn revalidate(
        &self,
        policy: &ToolPolicy,
        tools: &[ToolInfo],
        active: &[String],
    ) -> Vec<String> {
        let current = if policy.automatic() {
            resolve_names(tools, active, &self.policy_target(policy, tools, active))
        } else {
            resolve_names(tools, active, &policy.allowed_names)
        };
        policy
            .allowed_names
            .iter()
            .filter(|n| current.contains(n))
            .cloned()
            .collect()
    }

    /// Restores the workflow policy after a reload; returns whether the stored policy changed.
    fn restore_tool_policy(&mut self, tools: &[ToolInfo], active: &[String]) -> bool {
        let previous = self.state.workflow_tool_policy.clone();
        let next = match &previous {
            None => {
                let explicit = self.explicit_selection();
                let desired = self.desired_names(tools, active);
                ToolPolicy {
                    kind: if explicit { "explicit" } else { "automatic" }.into(),
                    allowed_names: resolve_names(tools, active, &desired),
                    desired_names: explicit.then_some(desired),
                    resolved: true,
                }
            }
            Some(policy) if policy.resolved => policy.clone(),
            Some(policy) => ToolPolicy {
                allowed_names: resolve_names(
                    tools,
                    active,
                    &self.policy_target(policy, tools, active),
                ),
                ..policy.clone()
            },
        };
        self.allowed = Some(if next.resolved {
            self.revalidate(&next, tools, active)
        } else {
            next.allowed_names.clone()
        });
        let step = if next.resolved {
            PolicyStep::Revalidate
        } else {
            PolicyStep::Resolve
        };
        self.pending_policy = Some((self.workflow_gen, step));
        let changed = previous.as_ref() != Some(&next);
        self.state.workflow_tool_policy = Some(next);
        changed
    }
}

pub fn selectable_tools(tools: &[ToolInfo]) -> Vec<ToolInfo> {
    let mut list: Vec<ToolInfo> = tools
        .iter()
        .filter(|t| t.name != QUESTION_TOOL && t.name != COMPLETE_TOOL)
        .cloned()
        .collect();
    list.sort_by(|a, b| {
        b.is_builtin()
            .cmp(&a.is_builtin())
            .then(a.name.cmp(&b.name))
    });
    list
}

fn available_tools(tools: &[ToolInfo], active: &[String]) -> Vec<ToolInfo> {
    selectable_tools(tools)
        .into_iter()
        .filter(|t| t.availability(active, Route::Selection) == Availability::Available)
        .collect()
}

fn resolve_names(tools: &[ToolInfo], active: &[String], desired: &[String]) -> Vec<String> {
    available_tools(tools, active)
        .into_iter()
        .filter(|t| desired.contains(&t.name) && t.selectable())
        .map(|t| t.name)
        .collect()
}

/// Why `name` cannot join the Plan allowlist.
fn unavailable_reason(tools: &[ToolInfo], active: &[String], name: &str) -> &'static str {
    match tools.iter().find(|t| t.name == name) {
        None => "not registered",
        Some(tool) if !tool.selectable() => "blocked by the Plan policy",
        Some(tool) => match tool.availability(active, Route::Selection) {
            Availability::Hidden => "hidden in pi",
            Availability::Unsupported => "unsupported exposure",
            _ => "inactive in pi",
        },
    }
}

fn it_or_them(count: usize) -> &'static str {
    if count == 1 { "it" } else { "them" }
}

pub fn helpers_available(active: &[String]) -> bool {
    [QUESTION_TOOL, COMPLETE_TOOL]
        .iter()
        .all(|name| active.iter().any(|a| a == name))
}

/// The process-wide plan-mode state; cheap to clone into handlers.
#[derive(Clone)]
pub struct App {
    core: Arc<Mutex<Core>>,
    pub agent_dir: PathBuf,
}

impl App {
    pub fn new(agent_dir: PathBuf, f2: bool, toggle_shortcut: Option<String>) -> Self {
        let snapshot = settings::read(&agent_dir);
        let core = Core {
            state: PlanState::default(),
            persisted: None,
            settings: snapshot,
            settings_stamp: settings::stamp(&agent_dir),
            allowed: None,
            pending_policy: None,
            published: None,
            contracts_relevant: false,
            ready: None,
            finalization: None,
            workflow_gen: 0,
            draft: Draft::default(),
            retention: Retention::default(),
            pending_aborts: Vec::new(),
            questions: Vec::new(),
            next_question: 0,
            interaction_failed: false,
            session_id: None,
            shutdown: false,
            thinking_echoes: Vec::new(),
            warned: Vec::new(),
            draft_contracts: false,
            f2,
            toggle_shortcut,
        };
        Self {
            core: Arc::new(Mutex::new(core)),
            agent_dir,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Core> {
        self.core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reloads the settings file when it changed on disk.
    pub fn refresh_settings(&self) {
        let stamp = settings::stamp(&self.agent_dir);
        let mut core = self.lock();
        if core.settings_stamp != stamp {
            core.settings_stamp = stamp;
            core.settings = settings::read(&self.agent_dir);
        }
    }

    pub fn settings(&self) -> Settings {
        self.refresh_settings();
        self.lock().settings()
    }

    pub fn state(&self) -> PlanState {
        self.lock().state.clone()
    }

    pub async fn tools(&self, host: &Host) -> (Vec<ToolInfo>, Vec<String>) {
        let tools: Vec<ToolInfo> = host
            .all_tools()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(ToolInfo::from_value)
            .collect();
        let active = host
            .active_tools()
            .await
            .unwrap_or_else(|_| DEFAULT_ACTIVE_TOOLS.map(String::from).to_vec());
        (tools, active)
    }

    /// Shows `message` once per workflow for `key`.
    pub async fn warn_once(&self, host: &Host, key: &str, message: impl Into<String>) {
        {
            let mut core = self.lock();
            if core.warned.iter().any(|k| k == key) {
                return;
            }
            core.warned.push(key.to_owned());
        }
        self.warn(host, message).await;
    }

    /// Turns the Plan tools back on when something took them out of the active tools, for example
    /// the deferred extension's `auto` mode. Returns the active tools afterwards, and whether it
    /// changed them.
    pub async fn ensure_helpers(&self, host: &Host, active: Vec<String>) -> (Vec<String>, bool) {
        if helpers_available(&active) {
            return (active, false);
        }
        let mut names = active;
        for helper in [QUESTION_TOOL, COMPLETE_TOOL] {
            if !names.iter().any(|n| n == helper) {
                names.push(helper.to_owned());
            }
        }
        let _ = host.set_active_tools(&names).await;
        let (_, active) = self.tools(host).await;
        let changed = helpers_available(&active);
        (active, changed)
    }

    /// Tells the user that the Plan tools were hidden and turned back on.
    pub async fn warn_helpers_restored(&self, host: &Host, tools: &[ToolInfo]) {
        let deferred = tools.iter().any(|t| t.name == DEFERRED_SEARCH);
        let fix = if deferred {
            "The deferred extension lists tools by name only in its auto mode or with a names rule. To keep these two listed, add the rule \"plan_mode_*\": \"on\" to its tools."
        } else {
            "Check the tool settings of other extensions if this happens again."
        };
        let message = format!(
            "plan_mode_question and plan_mode_complete were hidden from the model, so Plan mode turned them back on. {fix}"
        );
        self.warn_once(host, "helpers", message).await;
    }

    pub async fn persist(&self, host: &Host) -> Result<()> {
        let (value, serialized, same, recheck) = {
            let core = self.lock();
            let serialized = serde_json::to_string(&core.state)?;
            let same = core.persisted.as_deref() == Some(serialized.as_str());
            let recheck = same && core.state.enabled && core.state.latest_plan.is_none();
            (
                serde_json::to_value(&core.state)?,
                serialized,
                same,
                recheck,
            )
        };
        if same {
            // A repeated no-plan entry still stops recovery of a later completion result.
            if !recheck
                || crate::state::restore(&host.branch(None).await?)
                    .latest_plan
                    .is_none()
            {
                return Ok(());
            }
        }
        host.append_entry(STATE_ENTRY, &value).await?;
        self.lock().persisted = Some(serialized);
        Ok(())
    }

    pub async fn update_ui(&self, host: &Host) {
        let (state, tools, shortcut, managed) = {
            let core = self.lock();
            let tools = ui::tool_summary(core.allowed.as_deref().unwrap_or_default());
            let shortcut = core.f2.then_some("F2");
            (core.state.clone(), tools, shortcut, core.managed_draft())
        };
        if let Err(err) = ui::publish(host, &state, &tools, shortcut, managed).await {
            pi_ext_sdk::warn!("plan-mode: cannot update the UI: {}", err.message());
        }
    }

    /// Tells the user; without a UI the message becomes the command error, as upstream throws.
    pub async fn report(&self, host: &Host, message: impl Into<String>, error: bool) -> Result<()> {
        let message = message.into();
        if !host.has_ui() {
            return Err(Error::msg(message));
        }
        if error {
            host.notify_error(message).await
        } else {
            host.notify_warning(message).await
        }
    }

    pub async fn notify(&self, host: &Host, message: impl Into<String>) {
        let _ = host.notify_info(message).await;
    }

    pub async fn warn(&self, host: &Host, message: impl Into<String>) {
        let _ = host.notify_warning(message).await;
    }

    async fn is_idle(&self, host: &Host) -> bool {
        host.is_idle().await.unwrap_or(true)
    }

    /// State-changing transitions need an idle run.
    pub async fn allow_transition(&self, host: &Host, action: &str) -> Result<bool> {
        if self.is_idle(host).await {
            return Ok(true);
        }
        let message = format!(
            "Cannot {action} while an agent run is active. Wait for the run to settle, then retry."
        );
        self.report(host, message, false).await?;
        Ok(false)
    }

    async fn publish_mode(&self, host: &Host, mode: Mode) -> Result<bool> {
        if self.lock().published == Some(mode) {
            return Ok(true);
        }
        let message = CustomMessage::new(MODE_CONTRACT_TYPE, mode_contract_content(mode))
            .display(false)
            .details(mode_contract_details(mode))
            .trigger_turn(false);
        match host.send_message(message).await {
            Ok(()) => {
                let mut core = self.lock();
                core.published = Some(mode);
                core.contracts_relevant = true;
                Ok(true)
            }
            Err(err) => {
                let name = if mode == Mode::Plan { "Plan" } else { "Normal" };
                let message = format!(
                    "Unable to publish the {name} mode contract: {}",
                    ui::safe_text(err.message())
                );
                self.report(host, message, true).await?;
                Ok(false)
            }
        }
    }

    pub async fn publish_normal(&self, host: &Host) -> Result<()> {
        if self.publish_mode(host, Mode::Normal).await? {
            Ok(())
        } else {
            Err(Error::msg("the Normal mode contract was not published"))
        }
    }

    async fn publish_interaction(&self, host: &Host, mode: Interaction) -> Result<()> {
        let message = CustomMessage::new(
            INTERACTION_CONTRACT_TYPE,
            interaction_contract_content(mode),
        )
        .display(false)
        .details(interaction_contract_details(mode))
        .trigger_turn(false);
        host.send_message(message).await
    }

    /// Sends a user message, as a follow-up while the agent runs.
    pub async fn send_user(&self, host: &Host, text: &str) -> bool {
        let deliver = (!self.is_idle(host).await).then_some(DeliverAs::FollowUp);
        match host.send_user_message(text, deliver).await {
            Ok(()) => true,
            Err(err) => {
                let message = format!(
                    "Unable to send Plan-mode message: {}",
                    ui::safe_text(err.message())
                );
                let _ = host.notify_error(message).await;
                false
            }
        }
    }

    async fn thinking(&self, host: &Host) -> Option<String> {
        host.thinking_level().await.ok().flatten()
    }

    /// Sets the level and remembers it, so its `thinking_level_select` echo is not a manual change.
    async fn set_thinking(&self, host: &Host, level: &str) {
        let before = self.thinking(host).await;
        self.lock().thinking_echoes.push(level.to_owned());
        let _ = host.set_thinking_level(level).await;
        if self.thinking(host).await == before {
            let mut core = self.lock();
            if let Some(index) = core.thinking_echoes.iter().rposition(|l| l == level) {
                core.thinking_echoes.remove(index);
            }
        }
    }

    async fn apply_plan_thinking(&self, host: &Host) {
        let (manual, configured, applied) = {
            let core = self.lock();
            let configured = core.settings().plan_thinking().map(str::to_owned);
            (
                core.state.manual_thinking_level.clone(),
                configured,
                core.state.applied_thinking_level.clone(),
            )
        };
        let current = self.thinking(host).await;
        if let Some(manual) = manual {
            if current.as_deref() != Some(manual.as_str()) {
                self.set_thinking(host, &manual).await;
            }
            return;
        }
        let Some(level) = configured else {
            let mut core = self.lock();
            core.state.previous_thinking_level = None;
            core.state.applied_thinking_level = None;
            return;
        };
        if applied.is_none() {
            self.lock().state.previous_thinking_level = current.clone();
        }
        if current.as_deref() != Some(level.as_str()) {
            self.set_thinking(host, &level).await;
        }
        let effective = self.thinking(host).await;
        self.lock().state.applied_thinking_level = effective;
    }

    async fn capture_manual_thinking(&self, host: &Host) {
        let Some(applied) = self.lock().state.applied_thinking_level.clone() else {
            return;
        };
        let current = self.thinking(host).await;
        if current.as_deref() == Some(applied.as_str()) {
            return;
        }
        let mut core = self.lock();
        core.state.manual_thinking_level = current;
        core.state.previous_thinking_level = None;
        core.state.applied_thinking_level = None;
    }

    async fn restore_thinking(&self, host: &Host) {
        self.capture_manual_thinking(host).await;
        let (applied, previous) = {
            let mut core = self.lock();
            (
                core.state.applied_thinking_level.take(),
                core.state.previous_thinking_level.take(),
            )
        };
        if let (Some(applied), Some(previous)) = (applied, previous)
            && self.thinking(host).await.as_deref() == Some(applied.as_str())
        {
            self.set_thinking(host, &previous).await;
        }
    }

    /// Starts a workflow and says so, then warns about tools. `selected` is a `/plan tools`
    /// choice; `None` keeps the stored one.
    pub async fn enter(&self, host: &Host, selected: Option<Vec<String>>) -> Result<bool> {
        if self.lock().state.enabled {
            return Ok(true);
        }
        if !self.allow_transition(host, "start Plan mode").await? {
            return Ok(false);
        }
        self.refresh_settings();
        let (tools, active) = self.tools(host).await;
        let (active, restored) = self.ensure_helpers(host, active).await;
        if !helpers_available(&active) {
            let message = "Cannot start Plan mode: plan_mode_question and plan_mode_complete are unavailable; include them in the active tool allowlist or leave the restrictive tool mode before starting Plan mode.";
            self.report(host, message, true).await?;
            return Ok(false);
        }
        if !self.publish_mode(host, Mode::Plan).await? {
            return Ok(false);
        }
        let explicit_choice = selected.is_some();
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.contracts_relevant = true;
            if let Some(selected) = selected {
                core.state.selected_tool_names = Some(selected);
            }
            let explicit = core.explicit_selection();
            let draft = (!explicit)
                .then(|| core.session_id.clone())
                .flatten()
                .map(|session_id| DraftIdentity {
                    version: 1,
                    workflow_id: crate::ids::uuid(),
                    session_id,
                    status: "pending".into(),
                });
            let state = &mut core.state;
            state.enabled = true;
            state.draft = draft;
            state.interaction_mode = Some(Interaction::Questions);
            state.latest_plan = None;
            state.latest_plan_source = None;
            state.awaiting_action = false;
            state.saved_plan = None;
            state.active_implementation = None;
            core.begin_tool_policy(&tools, &active);
        }
        self.apply_plan_thinking(host).await;
        self.persist(host).await?;
        self.publish_interaction(host, Interaction::Questions)
            .await?;
        self.update_ui(host).await;
        let notice = if explicit_choice {
            "Plan mode enabled with the selected tools."
        } else {
            PLAN_ENABLED
        };
        self.notify(host, notice).await;
        if restored {
            self.warn_helpers_restored(host, &tools).await;
        }
        self.warn_unresolved(host, &tools, &active).await;
        Ok(true)
    }

    /// Warns when selected tools did not make the allowlist, or when it is empty.
    async fn warn_unresolved(&self, host: &Host, tools: &[ToolInfo], active: &[String]) {
        let (desired, allowed) = {
            let core = self.lock();
            let desired = core
                .state
                .workflow_tool_policy
                .as_ref()
                .and_then(|p| p.desired_names.clone())
                .unwrap_or_default();
            (desired, core.allowed.clone().unwrap_or_default())
        };
        let missing: Vec<String> = desired
            .iter()
            .filter(|name| !allowed.contains(name))
            .map(|name| {
                let reason = unavailable_reason(tools, active, name);
                format!("{} ({reason})", ui::safe_text(name))
            })
            .collect();
        if !missing.is_empty() {
            let message = format!(
                "Plan mode will not allow {}. Plan mode does not register or turn on these tools; make {} available in pi, then start a new Plan workflow.",
                missing.join(", "),
                it_or_them(missing.len())
            );
            self.warn_once(host, "unresolved", message).await;
        }
        if allowed.is_empty() {
            let message = "Plan mode allows no tools, so the agent cannot read the project. Turn on read or bash in pi, or choose tools with /plan tools.";
            self.warn_once(host, "empty", message).await;
        }
    }

    /// Warns once per tool and workflow when the model cannot see a tool that the Plan policy allows.
    pub async fn warn_hidden(&self, host: &Host, tools: &[ToolInfo], active: &[String]) {
        let hidden: Vec<String> = {
            let mut core = self.lock();
            let hidden: Vec<String> = core
                .allowed
                .iter()
                .flatten()
                .filter(|name| !active.contains(name))
                .filter(|name| !core.warned.contains(&format!("hidden:{name}")))
                .filter(|name| {
                    tools.iter().any(|t| {
                        t.name == **name
                            && matches!(t.exposure.as_deref(), None | Some("direct" | "model-only"))
                    })
                })
                .cloned()
                .collect();
            core.warned
                .extend(hidden.iter().map(|name| format!("hidden:{name}")));
            hidden
        };
        let Some(first) = hidden.first() else { return };
        let them = it_or_them(hidden.len());
        let subject = if hidden.len() == 1 {
            "it is"
        } else {
            "they are"
        };
        let fix = if tools.iter().any(|t| t.name == DEFERRED_SEARCH) {
            format!(
                "If the deferred extension lists {them} by name, add rules such as \"{first}\": \"on\" to its tools, or allow deferred_search with /plan tools."
            )
        } else {
            format!("Turn {them} on in pi to use {them} in this workflow.")
        };
        let names = ui::safe_text(&hidden.join(", "));
        let message = format!(
            "The model cannot see {names}, which Plan mode allows: {subject} not active. {fix}"
        );
        self.warn(host, message).await;
    }

    /// Starts Plan mode with a first prompt, or sends a follow-up while active.
    pub async fn enter_with_prompt(&self, host: &Host, prompt: &str) -> Result<()> {
        let was_enabled = self.lock().state.enabled;
        if !self.enter(host, None).await? {
            return Ok(());
        }
        if !self.send_user(host, prompt).await && !was_enabled {
            self.exit(host).await?;
        }
        Ok(())
    }

    /// Leaves Plan mode, or clears a saved or active implementation plan.
    pub async fn exit(&self, host: &Host) -> Result<bool> {
        if !self
            .allow_transition(host, "leave or clear Plan mode")
            .await?
        {
            return Ok(false);
        }
        let (was_enabled, relevant) = {
            let core = self.lock();
            (core.state.enabled, core.contracts_relevant)
        };
        if (was_enabled || relevant) && !self.publish_mode(host, Mode::Normal).await? {
            return Ok(false);
        }
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.allowed = None;
            core.state.end_workflow();
        }
        if was_enabled {
            self.restore_thinking(host).await;
            self.lock().state.manual_thinking_level = None;
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        Ok(true)
    }

    pub async fn exit_and_notify(&self, host: &Host) -> Result<()> {
        let notification = self.lock().disable_notification();
        if self.exit(host).await? {
            self.notify(host, notification).await;
        }
        Ok(())
    }

    pub async fn toggle_plan_mode(&self, host: &Host) -> Result<()> {
        let (enabled, saved) = {
            let core = self.lock();
            (core.state.enabled, core.state.saved_plan.is_some())
        };
        if enabled {
            return self.exit_and_notify(host).await;
        }
        if saved {
            return self.saved_plan_blocks(host).await;
        }
        self.enter(host, None).await.map(drop)
    }

    pub async fn saved_plan_blocks(&self, host: &Host) -> Result<()> {
        let message = "A plan is saved for later. Implement or clear it before starting another Plan-mode workflow.";
        self.report(host, message, false).await
    }

    /// Stores the ready plan in this session and leaves Plan mode.
    pub async fn save(&self, host: &Host) -> Result<()> {
        let ready = {
            let core = self.lock();
            core.state.ready_plan().map(|plan| {
                let source = core.state.latest_plan_source.clone();
                (
                    plan.to_owned(),
                    source.unwrap_or_else(|| LEGACY_SOURCE.into()),
                )
            })
        };
        let Some((plan, source)) = ready else {
            return self
                .report(host, "No completed plan is available to save.", false)
                .await;
        };
        if !self
            .allow_transition(host, "save the plan and leave Plan mode")
            .await?
            || !self.publish_mode(host, Mode::Normal).await?
        {
            return Ok(());
        }
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.allowed = None;
            core.state.end_workflow();
            core.state.saved_plan = Some(SavedPlan { plan, source });
        }
        self.restore_thinking(host).await;
        self.lock().state.manual_thinking_level = None;
        self.persist(host).await?;
        self.update_ui(host).await;
        self.notify(host, "Plan saved for later. Plan mode disabled.")
            .await;
        Ok(())
    }

    /// Checks the mode, model, and auth before a saved plan is implemented.
    async fn preflight_saved(&self, host: &Host) -> Result<bool> {
        let mode = host.snapshot().and_then(|s| s.mode.clone());
        if matches!(mode.as_deref(), Some("print" | "json")) {
            return Err(Error::msg(
                "Saved plan implementation is unavailable in print/JSON mode. Use TUI or RPC.",
            ));
        }
        let Some(model) = host.model().await? else {
            self.warn(
                host,
                "Unable to implement saved plan: no model is selected.",
            )
            .await;
            return Ok(false);
        };
        if let Err(error) = model_auth(host, &model.provider, &model.id).await {
            self.warn(host, format!("Unable to implement saved plan: {error}"))
                .await;
            return Ok(false);
        }
        Ok(true)
    }

    /// Implements the ready or saved plan in this session.
    pub async fn implement_here(&self, host: &Host) -> Result<()> {
        let (saved, plan, source, was_enabled) = {
            let core = self.lock();
            let state = &core.state;
            if state.enabled {
                let source = state.latest_plan_source.clone();
                (false, state.latest_plan.clone(), source, true)
            } else {
                let saved = state.saved_plan.as_ref();
                let source = saved.map(|s| s.source.clone());
                (
                    saved.is_some(),
                    saved.map(|s| s.plan.clone()),
                    source,
                    false,
                )
            }
        };
        let Some(plan) = plan else {
            self.warn(
                host,
                "Plan mode disabled. No proposed plan is available to implement.",
            )
            .await;
            return Ok(());
        };
        if !self
            .allow_transition(host, "start plan implementation")
            .await?
        {
            return Ok(());
        }
        if saved && !self.preflight_saved(host).await? {
            return Ok(());
        }
        let previous = self.state();
        if !self.publish_mode(host, Mode::Normal).await? {
            return Ok(());
        }
        let retention = self.settings().retention().to_owned();
        let history = retention == "clear-on-start";
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.allowed = None;
            core.state.end_workflow();
            core.state.active_implementation = (!history).then(|| ActiveImplementation {
                id: crate::ids::uuid(),
                plan: plan.clone(),
                source: source.unwrap_or_else(|| LEGACY_SOURCE.into()),
                started_at: crate::ids::now_ms(),
                retention: retention.clone(),
            });
        }
        if was_enabled {
            self.restore_thinking(host).await;
            self.lock().state.manual_thinking_level = None;
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        let handoff = match (history, was_enabled) {
            (true, true) => HISTORY_IMPLEMENTATION_PROMPT.to_owned(),
            (true, false) => transferred_plan_prompt(&plan, false),
            (false, _) => implementation_handoff(&plan),
        };
        if self.send_user(host, &handoff).await {
            return Ok(());
        }
        // Delivery failed: restore the ready or saved plan.
        self.lock().state = previous;
        if was_enabled {
            let (tools, active) = self.tools(host).await;
            self.lock().restore_tool_policy(&tools, &active);
            self.publish_mode(host, Mode::Plan).await?;
            let mode = self.lock().state.interaction();
            self.publish_interaction(host, mode).await?;
            self.apply_plan_thinking(host).await;
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        Ok(())
    }

    pub async fn clear_active_implementation(&self, host: &Host, id: &str) -> Result<()> {
        {
            let mut core = self.lock();
            if core
                .state
                .active_implementation
                .as_ref()
                .map(|a| a.id.as_str())
                != Some(id)
            {
                return Ok(());
            }
            core.advance_workflow();
            core.state.active_implementation = None;
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        Ok(())
    }

    /// Stores a completed plan and arms the ready menu.
    pub async fn accept_completion(&self, host: &Host, plan: &str, source: &str) -> Result<()> {
        let plan = match validate_plan(plan) {
            Ok(plan) => plan,
            Err(error) => {
                self.warn(host, format!("Proposed plan is not ready: {error}."))
                    .await;
                self.persist(host).await?;
                self.update_ui(host).await;
                return Ok(());
            }
        };
        {
            let mut core = self.lock();
            core.finalization = None;
            let state = &core.state;
            if state.enabled
                && state.awaiting_action
                && state.latest_plan.as_deref() == Some(plan.as_str())
                && state.latest_plan_source.as_deref() == Some(source)
            {
                return Ok(());
            }
            core.state.latest_plan = Some(plan.clone());
            core.state.latest_plan_source = Some(source.to_owned());
            core.state.awaiting_action = true;
            let show_menu = core.state.interaction() == Interaction::Questions;
            core.ready = (show_menu || source == LEGACY_SOURCE).then(|| ReadyIntent {
                plan,
                source: source.to_owned(),
                show_menu,
            });
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        Ok(())
    }

    /// Switches between questionnaires and free discussion. `from_question` is the F2 key in an
    /// open questionnaire, which is allowed while the agent runs.
    pub async fn toggle_interaction(&self, host: &Host, from_question: bool) -> Result<bool> {
        let (enabled, failed, previous) = {
            let core = self.lock();
            (
                core.state.enabled,
                core.interaction_failed,
                core.state.clone(),
            )
        };
        if !enabled {
            return Ok(false);
        }
        if failed {
            let _ = host
                .notify_error("Questionnaires are blocked after a failed interaction transition. Run /reload before continuing.")
                .await;
            return Ok(false);
        }
        if !from_question && !self.is_idle(host).await {
            self.warn(
                host,
                "Wait for the agent run to settle before changing Plan interaction mode.",
            )
            .await;
            return Ok(false);
        }
        let next = previous.interaction().toggled();
        self.lock().state.interaction_mode = Some(next);
        let applied = match self.persist(host).await {
            Ok(()) => self.publish_interaction(host, next).await,
            Err(err) => Err(err),
        };
        if applied.is_err() {
            self.lock().state = previous.clone();
            let restored = match self.persist(host).await {
                Ok(()) => self.publish_interaction(host, previous.interaction()).await,
                Err(err) => Err(err),
            };
            if restored.is_err() {
                self.fail_interaction(host).await;
                return Ok(false);
            }
            let _ = host
                .notify_error("Unable to change Plan interaction mode. The previous mode and questionnaire are unchanged.")
                .await;
            return Ok(false);
        }
        {
            let mut core = self.lock();
            core.finalization = None;
            core.ready = core
                .ready
                .take()
                .filter(|r| r.source == LEGACY_SOURCE)
                .map(|r| ReadyIntent {
                    show_menu: false,
                    ..r
                });
        }
        self.update_ui(host).await;
        Ok(true)
    }

    async fn fail_interaction(&self, host: &Host) {
        {
            let mut core = self.lock();
            core.interaction_failed = true;
            core.finalization = None;
            core.ready = None;
            for (_, token) in core.questions.drain(..) {
                token.cancel();
            }
        }
        let _ = host.abort().await;
        let _ = host
            .notify_error("Plan interaction state could not be recovered. Questionnaires are blocked; run /reload before continuing.")
            .await;
    }

    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutdown
    }

    pub fn questions_paused(&self) -> bool {
        let core = self.lock();
        core.interaction_failed || core.state.interaction() == Interaction::Discussion
    }

    /// Registers an open questionnaire; a workflow change cancels it. Returns its id.
    pub fn track_question(&self, token: CancellationToken) -> u64 {
        let mut core = self.lock();
        core.next_question += 1;
        let id = core.next_question;
        core.questions.push((id, token));
        id
    }

    pub fn untrack_question(&self, id: u64) {
        self.lock().questions.retain(|(q, _)| *q != id);
    }

    /// Asks pi to stop the run when the tool call `id` ends (a questionnaire left for discussion).
    pub fn abort_after(&self, id: &str) {
        self.lock().pending_aborts.push(id.to_owned());
    }

    pub fn finalization_satisfied(&self) {
        self.lock().finalization = None;
    }

    fn finalize_prompt(&self, prompt: &str) -> String {
        if self.lock().managed_draft() {
            draft_finalization_prompt(prompt)
        } else {
            prompt.to_owned()
        }
    }

    pub async fn finalize(&self, host: &Host) -> Result<()> {
        let (enabled, mode, generation) = {
            let core = self.lock();
            (
                core.state.enabled,
                core.state.interaction(),
                core.workflow_gen,
            )
        };
        if !enabled {
            self.warn(host, "Plan mode is not active. Use /plan first.")
                .await;
            return Ok(());
        }
        if mode == Interaction::Discussion {
            self.lock().finalization = None;
            let prompt = self.finalize_prompt(DISCUSSION_FINALIZE_PLAN_PROMPT);
            self.send_user(host, &prompt).await;
            return Ok(());
        }
        self.request_finalization(generation);
        let prompt = self.finalize_prompt(FINALIZE_PLAN_PROMPT);
        if !self.send_user(host, &prompt).await {
            self.lock().finalization = None;
        }
        Ok(())
    }

    fn request_finalization(&self, generation: u64) {
        self.lock().finalization = Some(Finalization {
            generation,
            retried: false,
            awaiting: false,
        });
    }

    /// `message_start`: a typed finalize prompt counts as an explicit request.
    pub fn observe_user_message(&self, text: &str) {
        let generation = {
            let core = self.lock();
            let active = core.state.enabled && core.state.interaction() == Interaction::Questions;
            let prompts = [
                FINALIZE_PLAN_PROMPT.to_owned(),
                draft_finalization_prompt(FINALIZE_PLAN_PROMPT),
            ];
            if !active || !prompts.iter().any(|p| p == text.trim()) {
                return;
            }
            core.workflow_gen
        };
        self.request_finalization(generation);
    }

    /// Records how a run ended for a pending finalization request.
    pub fn observe_run_end(&self, normal: bool) {
        let mut core = self.lock();
        let generation = core.workflow_gen;
        let Some(request) = core
            .finalization
            .as_mut()
            .filter(|f| f.generation == generation)
        else {
            return;
        };
        if normal {
            request.awaiting = true;
        } else {
            core.finalization = None;
        }
    }

    /// `agent_settled` for a finalization request: retry once, then warn. pi sends the event
    /// only when no queued message continues the run, and reports busy while its handlers run.
    pub async fn settle_finalization(&self, host: &Host) -> Result<()> {
        let retry = {
            let mut core = self.lock();
            let generation = core.workflow_gen;
            if !core.state.enabled || core.state.interaction() != Interaction::Questions {
                return Ok(());
            }
            match core.finalization.as_mut() {
                Some(f) if f.generation == generation && f.awaiting && !f.retried => {
                    f.retried = true;
                    f.awaiting = false;
                    Some(true)
                }
                Some(f) if f.generation == generation && f.awaiting => {
                    core.finalization = None;
                    Some(false)
                }
                _ => None,
            }
        };
        match retry {
            Some(true) => {
                let prompt = self.finalize_prompt(crate::contracts::RETRY_FINALIZE_PLAN_PROMPT);
                if !self.send_user(host, &prompt).await {
                    self.lock().finalization = None;
                }
            }
            Some(false) => {
                self.warn(host, "Plan finalization ended twice without a structured question or completed plan. Plan mode remains active; revise the plan or run /plan finalize again.").await;
            }
            None => {}
        }
        Ok(())
    }

    /// A new user prompt during a ready plan starts a revision and clears the ready plan.
    pub async fn clear_ready_for_revision(&self, host: &Host) -> Result<()> {
        {
            let mut core = self.lock();
            if !core.state.enabled
                || (core.state.latest_plan.is_none() && !core.state.awaiting_action)
            {
                return Ok(());
            }
            core.ready = None;
            core.state.latest_plan = None;
            core.state.latest_plan_source = None;
            core.state.awaiting_action = false;
        }
        self.persist(host).await?;
        self.update_ui(host).await;
        Ok(())
    }

    /// Takes the ready intent when it still matches the current plan.
    pub fn take_ready(&self) -> Option<ReadyIntent> {
        let mut core = self.lock();
        let intent = core.ready.clone()?;
        let state = &core.state;
        let current = state.enabled
            && state.awaiting_action
            && state.latest_plan.as_deref() == Some(intent.plan.as_str())
            && state.latest_plan_source.as_deref() == Some(intent.source.as_str());
        if !current {
            return None;
        }
        core.ready = None;
        Some(intent)
    }

    /// Whether `intent`'s plan is still the ready plan and questionnaires are on.
    pub fn ready_is_current(&self, intent: &ReadyIntent) -> bool {
        let core = self.lock();
        let state = &core.state;
        state.enabled
            && state.awaiting_action
            && state.interaction() == Interaction::Questions
            && state.latest_plan.as_deref() == Some(intent.plan.as_str())
            && state.latest_plan_source.as_deref() == Some(intent.source.as_str())
    }

    pub fn take_pending_abort(&self, id: &str) -> bool {
        let mut core = self.lock();
        let before = core.pending_aborts.len();
        core.pending_aborts.retain(|p| p != id);
        let found = core.pending_aborts.len() != before;
        if found {
            core.pending_aborts.clear();
        }
        found
    }

    /// Resolves or revalidates the workflow policy at the first context of the workflow.
    pub async fn resolve_pending_policy(&self, host: &Host) -> Result<()> {
        let pending = {
            let mut core = self.lock();
            match core.pending_policy {
                Some((generation, step))
                    if generation == core.workflow_gen && core.state.enabled =>
                {
                    let resolved = core.state.workflow_tool_policy.as_ref().map(|p| p.resolved);
                    if resolved != Some(step == PolicyStep::Revalidate) {
                        core.pending_policy = None;
                        None
                    } else {
                        Some(step)
                    }
                }
                Some(_) => {
                    core.pending_policy = None;
                    None
                }
                None => None,
            }
        };
        let Some(step) = pending else { return Ok(()) };
        let (tools, active) = self.tools(host).await;
        let (changed, dropped) = {
            let mut core = self.lock();
            let Some(policy) = core.state.workflow_tool_policy.clone() else {
                return Ok(());
            };
            let previous = core.allowed.clone().unwrap_or_default();
            let allowed = match step {
                PolicyStep::Resolve => resolve_names(
                    &tools,
                    &active,
                    &core.policy_target(&policy, &tools, &active),
                ),
                PolicyStep::Revalidate => core.revalidate(&policy, &tools, &active),
            };
            let changed = !policy.resolved || policy.allowed_names != allowed;
            let dropped: Vec<String> = previous
                .into_iter()
                .filter(|name| !allowed.contains(name))
                .map(|name| {
                    format!(
                        "{} ({})",
                        ui::safe_text(&name),
                        unavailable_reason(&tools, &active, &name)
                    )
                })
                .collect();
            core.allowed = Some(allowed.clone());
            core.state.workflow_tool_policy = Some(ToolPolicy {
                allowed_names: allowed,
                resolved: true,
                ..policy
            });
            core.pending_policy = None;
            (changed, dropped)
        };
        if changed {
            self.persist(host).await?;
        }
        self.update_ui(host).await;
        if !dropped.is_empty() {
            let (it, was) = if dropped.len() == 1 {
                ("it", "it was")
            } else {
                ("them", "they were")
            };
            let message = format!(
                "Plan mode will not allow {} in this workflow, because {was} unavailable when the first request froze the tool policy. Make {it} available in pi, then start a new Plan workflow.",
                dropped.join(", ")
            );
            self.warn_once(host, "dropped", message).await;
        }
        Ok(())
    }

    /// Admits the managed draft at the first context when native read, write, and edit are active.
    pub async fn resolve_draft_admission(&self, host: &Host) -> Result<()> {
        let identity = {
            let core = self.lock();
            let state = &core.state;
            let ready = state.enabled
                && state
                    .workflow_tool_policy
                    .as_ref()
                    .is_some_and(|p| p.resolved)
                && state.draft.as_ref().is_some_and(|d| {
                    d.status == "pending" && Some(&d.session_id) == core.session_id.as_ref()
                });
            if !ready {
                return Ok(());
            }
            state.draft.clone()
        };
        let Some(identity) = identity else {
            return Ok(());
        };
        let (tools, active) = self.tools(host).await;
        let native = crate::draft::TOOL_NAMES.iter().all(|name| {
            tools.iter().any(|t| {
                t.name == *name
                    && t.has_native_source()
                    && matches!(t.exposure.as_deref(), None | Some("direct"))
                    && active.contains(&t.name)
            })
        });
        let admitted = {
            let mut core = self.lock();
            let automatic = core
                .state
                .workflow_tool_policy
                .as_ref()
                .is_some_and(ToolPolicy::automatic);
            let reads = core
                .allowed
                .as_ref()
                .is_some_and(|a| a.iter().any(|n| n == "read"));
            let admitted =
                automatic && reads && native && core.draft.bind(&self.agent_dir, &identity);
            core.state.draft = admitted.then(|| DraftIdentity {
                status: "active".into(),
                ..identity
            });
            admitted
        };
        if let Err(err) = self.persist(host).await {
            if admitted {
                self.lock().draft.rollback();
            }
            return Err(err);
        }
        self.update_ui(host).await;
        Ok(())
    }

    /// Installs restored state; an enabled workflow needs the helper tools.
    pub async fn install_restored(&self, host: &Host, candidate: PlanState) -> Result<bool> {
        let was_enabled = self.lock().state.enabled;
        let (tools, active) = self.tools(host).await;
        let (active, restored) = if candidate.enabled {
            self.ensure_helpers(host, active).await
        } else {
            (active, false)
        };
        if candidate.enabled && !helpers_available(&active) {
            {
                let mut core = self.lock();
                core.state = PlanState::default();
                // Keep the stored workflow for a later session that has the helpers.
                core.persisted = serde_json::to_string(&core.state).ok();
                core.allowed = None;
                core.pending_policy = None;
            }
            let _ = host
                .notify_warning("Plan mode was not restored because its helper tools are unavailable under the active tool policy.")
                .await;
            return Ok(false);
        }
        if was_enabled && !candidate.enabled {
            self.lock().ready = None;
            self.restore_thinking(host).await;
        }
        let changed = {
            let mut core = self.lock();
            core.state = candidate;
            if core.state.enabled {
                core.restore_tool_policy(&tools, &active)
            } else {
                core.allowed = None;
                core.pending_policy = None;
                false
            }
        };
        if changed {
            self.persist(host).await?;
        }
        if self.lock().state.enabled {
            self.apply_plan_thinking(host).await;
            {
                let mut core = self.lock();
                if core.managed_draft()
                    && let Some(identity) = core.state.draft.clone()
                {
                    core.draft.bind(&self.agent_dir, &identity);
                }
            }
            if restored {
                self.warn_helpers_restored(host, &tools).await;
            }
        }
        Ok(true)
    }

    /// Reads the branch and installs its state (session start, tree navigation).
    pub async fn restore_from_branch(&self, host: &Host) -> Result<()> {
        let branch = host.branch(None).await?;
        let restored = crate::state::restore(&branch);
        {
            let mut core = self.lock();
            core.persisted = serde_json::to_string(&restored).ok();
            core.published = crate::contracts::latest_mode_contract(&branch).map(|(_, mode)| mode);
            core.draft_contracts = branch
                .iter()
                .any(|e| crate::draft::parse_entry(e).is_some());
            core.contracts_relevant = crate::contracts::has_mode_contract_artifact(&branch)
                || restored.enabled
                || restored.saved_plan.is_some()
                || restored.active_implementation.is_some();
        }
        if !self.install_restored(host, restored).await? {
            return Ok(());
        }
        {
            let mut core = self.lock();
            let active = core.state.active_implementation.clone();
            core.retention.restore(active.as_ref());
        }
        self.update_ui(host).await;
        Ok(())
    }

    pub async fn session_start(&self, host: &Host) -> Result<()> {
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.retention.reset();
            core.allowed = None;
            core.shutdown = false;
            core.interaction_failed = false;
            core.pending_aborts.clear();
            core.state = PlanState::default();
            core.session_id = host.snapshot().and_then(|s| s.session_id.clone());
            core.settings_stamp = settings::stamp(&self.agent_dir);
            core.settings = settings::read(&self.agent_dir);
        }
        let (loaded, notice) = {
            let core = self.lock();
            (core.settings.loaded.clone(), core.settings.notice.clone())
        };
        if let settings::Loaded::Invalid(reason) = loaded {
            self.warn(host, format!("plan-mode settings ignored: {reason}"))
                .await;
        }
        if let Some(notice) = notice {
            self.warn(host, notice).await;
        }
        self.restore_from_branch(host).await
    }

    pub async fn session_tree(&self, host: &Host) -> Result<()> {
        {
            let mut core = self.lock();
            core.advance_workflow();
            core.retention.reset();
        }
        self.restore_from_branch(host).await
    }

    pub async fn thinking_selected(&self, host: &Host, level: &str) -> Result<()> {
        {
            let mut core = self.lock();
            if let Some(index) = core.thinking_echoes.iter().position(|l| l == level) {
                core.thinking_echoes.remove(index);
                return Ok(());
            }
            if core.shutdown || !core.state.enabled {
                return Ok(());
            }
            let state = &mut core.state;
            let Some(applied) = &state.applied_thinking_level else {
                return Ok(());
            };
            if applied == level {
                return Ok(());
            }
            state.manual_thinking_level = Some(level.to_owned());
            state.previous_thinking_level = None;
            state.applied_thinking_level = None;
        }
        self.persist(host).await
    }

    pub async fn shutdown(&self, host: &Host) -> Result<()> {
        self.lock().shutdown = true;
        self.capture_manual_thinking(host).await;
        let persisted = self.persist(host).await;
        if self.lock().state.enabled {
            self.restore_thinking(host).await;
        }
        let _ = ui::clear(host).await;
        let mut core = self.lock();
        core.advance_workflow();
        persisted
    }
}

/// `Ok` when pi has credentials for the model.
pub async fn model_auth(host: &Host, provider: &str, id: &str) -> std::result::Result<(), String> {
    let model = host
        .pi_call(
            "ctx.modelRegistry",
            "find",
            vec![json!(provider), json!(id)],
        )
        .await
        .map_err(|e| e.message().to_owned())?;
    if model.is_null() {
        return Err(format!("model {provider}/{id} is no longer available"));
    }
    let auth = host
        .pi_call("ctx.modelRegistry", "getApiKeyAndHeaders", vec![model])
        .await
        .map_err(|e| e.message().to_owned())?;
    if auth.get("ok") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        let error = auth
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("authentication failed");
        Err(ui::safe_text(error))
    }
}
