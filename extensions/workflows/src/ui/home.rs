//! The first screen: saved workflows and runs on two tabs, with type-to-search, and the details
//! and actions of a workflow.

use std::path::PathBuf;
use std::time::Duration;

use pi_ext_sdk::prelude::*;
use pi_ext_sdk::style;

use crate::model::{Mode, Workflow, format_duration};
use crate::planner;
use crate::runs::{RunState, RunStatus, now_ms};
use crate::store::Stored;
use crate::ui::editor::{self, Editor};
use crate::ui::frame::{self, Frame, Tone, accent, muted, warning};
use crate::ui::{IDLE, Screen, Ui, one_line, run_view, start, widget};

const PAGE: usize = 10;
const TICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Workflows,
    Runs,
}

/// A row of the workflows tab.
#[derive(Clone, Copy)]
enum Entry<'a> {
    New,
    Generate,
    Workflow(&'a Stored),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Run,
    Edit,
    Copy,
    Runs,
    Delete,
}

impl Choice {
    fn label(self, runs: usize) -> String {
        match self {
            Choice::Run => "Run".into(),
            Choice::Edit => "Edit".into(),
            Choice::Copy => "Copy to a new workflow".into(),
            Choice::Runs => format!("Show its runs ({runs})"),
            Choice::Delete => "Delete".into(),
        }
    }
}

/// The menu and the question hold the file of their workflow, so a list that changes under
/// them (another session saves or deletes a workflow) does not change their target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    List,
    /// The details and actions of a workflow, with the action under the pointer.
    Menu {
        choice: Choice,
        path: PathBuf,
    },
    /// Delete a workflow on enter.
    Confirm {
        path: PathBuf,
    },
}

struct Home {
    tab: Tab,
    query: String,
    /// The Runs tab shows only the runs of this workflow.
    only: Option<String>,
    /// The selected row of each tab.
    selected: [usize; 2],
    view: View,
    status: Option<(Tone, String)>,
}

/// What the screen holds while it draws and handles one key.
struct Data {
    workflows: Vec<Stored>,
    errors: Vec<String>,
    runs: Vec<RunState>,
}

impl Data {
    fn load(ui: &Ui) -> Self {
        let (workflows, errors) = ui.app.dirs(&ui.host).load();
        Self {
            workflows,
            errors,
            runs: ui.app.runs.all(),
        }
    }

    fn runs_of(&self, name: &str) -> usize {
        self.runs.iter().filter(|r| r.workflow.name == name).count()
    }
}

pub async fn run(
    ui: &Ui,
    screen: &mut Screen,
    tab: Tab,
    status: Option<(Tone, String)>,
) -> Result<()> {
    let mut home = Home {
        tab,
        query: String::new(),
        only: None,
        selected: [0, 0],
        view: View::List,
        status,
    };
    loop {
        let data = Data::load(ui);
        home.draw(screen, &data);
        let tick = if home.tab == Tab::Runs || ui.app.runs.active() > 0 {
            TICK
        } else {
            IDLE
        };
        let Some(key) = screen.next(tick).await else {
            if screen.closed {
                return Ok(());
            }
            continue;
        };
        if !home.key(ui, screen, &key, &data).await? || screen.closed {
            return Ok(());
        }
    }
}

impl Home {
    fn index(&self) -> usize {
        match self.tab {
            Tab::Workflows => 0,
            Tab::Runs => 1,
        }
    }

    fn selected(&self) -> usize {
        self.selected[self.index()]
    }

    fn select(&mut self, row: usize) {
        let index = self.index();
        self.selected[index] = row;
    }

    fn entries<'a>(&self, data: &'a Data) -> Vec<Entry<'a>> {
        let query = self.query.to_lowercase();
        let mut entries = Vec::new();
        if query.is_empty() {
            entries.extend([Entry::New, Entry::Generate]);
        }
        entries.extend(
            data.workflows
                .iter()
                .filter(|s| {
                    s.workflow.name.to_lowercase().contains(&query)
                        || s.workflow.description.to_lowercase().contains(&query)
                })
                .map(Entry::Workflow),
        );
        entries
    }

    fn runs<'a>(&self, data: &'a Data) -> Vec<&'a RunState> {
        let query = self.query.to_lowercase();
        data.runs
            .iter()
            .filter(|r| {
                self.only
                    .as_ref()
                    .is_none_or(|name| &r.workflow.name == name)
            })
            .filter(|r| {
                r.workflow.name.to_lowercase().contains(&query)
                    || r.id.contains(&query)
                    || r.status.label().contains(&query)
            })
            .collect()
    }

    /// The workflow of the menu or the question.
    fn target<'a>(&self, data: &'a Data) -> Option<&'a Stored> {
        match &self.view {
            View::Menu { path, .. } | View::Confirm { path } => {
                data.workflows.iter().find(|s| &s.path == path)
            }
            View::List => None,
        }
    }

    fn choices(&self, data: &Data) -> Vec<Choice> {
        let Some(stored) = self.target(data) else {
            return Vec::new();
        };
        let mut choices = vec![Choice::Run, Choice::Edit, Choice::Copy];
        if data.runs_of(&stored.workflow.name) > 0 {
            choices.push(Choice::Runs);
        }
        choices.push(Choice::Delete);
        choices
    }

    fn len(&self, data: &Data) -> usize {
        match self.tab {
            Tab::Workflows => self.entries(data).len(),
            Tab::Runs => self.runs(data).len(),
        }
    }

    /// `false` closes the panel.
    async fn key(&mut self, ui: &Ui, screen: &mut Screen, key: &Key, data: &Data) -> Result<bool> {
        let len = self.len(data);
        let row = self.selected().min(len.saturating_sub(1));
        self.select(row);
        let view = self.view.clone();
        if view != View::List && self.target(data).is_none() {
            self.view = View::List;
            self.status = Some((Tone::Warn, "The workflow is gone.".into()));
            return Ok(true);
        }
        match view {
            View::Confirm { .. } => {
                let target = self.target(data);
                self.view = View::List;
                self.status = Some(match (key, target) {
                    (Key::Enter, Some(stored)) => match std::fs::remove_file(&stored.path) {
                        Ok(()) => (Tone::Good, format!("Deleted {}.", stored.workflow.name)),
                        Err(err) => (Tone::Error, format!("Cannot delete it: {err}")),
                    },
                    _ => (Tone::Info, "Cancelled.".into()),
                });
                return Ok(true);
            }
            View::Menu { choice, path } => {
                let choices = self.choices(data);
                let len = choices.len().max(1);
                let at = choices.iter().position(|c| *c == choice).unwrap_or(0);
                let moved = |at: usize| choices.get(at).copied().unwrap_or(Choice::Run);
                match key {
                    Key::Up => {
                        let choice = moved((at + len - 1) % len);
                        self.view = View::Menu { choice, path };
                    }
                    Key::Down => {
                        let choice = moved((at + 1) % len);
                        self.view = View::Menu { choice, path };
                    }
                    Key::Enter => {
                        if let (true, Some(stored)) = (choices.contains(&choice), self.target(data))
                        {
                            self.act(ui, screen, choice, stored).await?;
                        }
                    }
                    k if k.is_cancel() || k.is("left") || k.is("backspace") => {
                        self.view = View::List;
                    }
                    _ => {}
                }
                return Ok(true);
            }
            View::List => {}
        }
        self.status = None;
        match key {
            Key::Escape | Key::Ctrl('c') if !self.query.is_empty() => self.query.clear(),
            Key::Escape | Key::Ctrl('c') if self.only.is_some() && self.tab == Tab::Runs => {
                self.only = None;
            }
            Key::Escape | Key::Ctrl('c') => return Ok(false),
            Key::Tab | Key::BackTab | Key::Left | Key::Right => {
                self.tab = match self.tab {
                    Tab::Workflows => Tab::Runs,
                    Tab::Runs => Tab::Workflows,
                };
                self.only = None;
            }
            Key::Up if len > 0 => self.select((row + len - 1) % len),
            Key::Down if len > 0 => self.select((row + 1) % len),
            Key::PageUp => self.select(row.saturating_sub(PAGE)),
            Key::PageDown => self.select((row + PAGE).min(len.saturating_sub(1))),
            Key::Home => self.select(0),
            Key::End => self.select(len.saturating_sub(1)),
            Key::Enter => self.open(ui, screen, data).await?,
            Key::Backspace => {
                self.query.pop();
                self.select(0);
            }
            Key::Char(c) => {
                self.query.push(*c);
                self.select(0);
            }
            Key::Text(text) => {
                self.query.push_str(text);
                self.select(0);
            }
            _ => {}
        }
        Ok(true)
    }

    async fn open(&mut self, ui: &Ui, screen: &mut Screen, data: &Data) -> Result<()> {
        self.status = None;
        if self.tab == Tab::Runs {
            if let Some(run) = self.runs(data).get(self.selected()) {
                run_view::run(ui, screen, &run.id, None).await?;
            }
            return Ok(());
        }
        match self.entries(data).get(self.selected()).copied() {
            Some(Entry::New) => editor::run(ui, screen, Editor::blank()).await?,
            Some(Entry::Generate) => {
                if let Some(workflow) = self.generate(ui, screen).await? {
                    editor::run(ui, screen, Editor::draft(workflow)).await?;
                }
            }
            Some(Entry::Workflow(stored)) => {
                self.view = View::Menu {
                    choice: Choice::Run,
                    path: stored.path.clone(),
                }
            }
            None => {}
        }
        Ok(())
    }

    async fn act(
        &mut self,
        ui: &Ui,
        screen: &mut Screen,
        choice: Choice,
        stored: &Stored,
    ) -> Result<()> {
        match choice {
            Choice::Run => match start::run_dialog(ui, screen, stored.workflow.clone()).await? {
                Ok(Some(started)) => {
                    self.view = View::List;
                    run_view::run(ui, screen, &started.id, Some(started.message)).await?;
                }
                Ok(None) => {}
                Err(err) => self.status = Some((Tone::Error, err)),
            },
            Choice::Edit => {
                editor::run(ui, screen, Editor::from_stored(stored.clone())).await?;
                self.view = View::List;
            }
            Choice::Copy => {
                editor::run(ui, screen, Editor::copy_of(stored)).await?;
                self.view = View::List;
            }
            Choice::Runs => {
                self.tab = Tab::Runs;
                self.only = Some(stored.workflow.name.clone());
                self.view = View::List;
                self.select(0);
            }
            Choice::Delete => {
                self.view = View::Confirm {
                    path: stored.path.clone(),
                }
            }
        }
        Ok(())
    }

    /// A draft from the session model, with a spinner; esc cancels it.
    async fn generate(&mut self, ui: &Ui, screen: &mut Screen) -> Result<Option<Workflow>> {
        let title = "Describe the workflow: the steps, who does each, models, approvals, and waits";
        let Some(description) = ui
            .host
            .editor(title, None)
            .await?
            .filter(|d| !d.trim().is_empty())
        else {
            return Ok(None);
        };
        let work = async {
            let catalog = ui.app.catalog(&ui.host).await;
            let available = planner::available(&ui.host, &catalog).await;
            planner::generate(&ui.host, &description, available).await
        };
        tokio::pin!(work);
        self.status = Some((
            Tone::Busy,
            "The session model drafts the workflow. esc cancels.".into(),
        ));
        let data = Data::load(ui);
        loop {
            self.draw(screen, &data);
            tokio::select! {
                result = &mut work => {
                    self.status = None;
                    return Ok(match result {
                        Ok(workflow) => Some(workflow),
                        Err(err) => {
                            self.status = Some((Tone::Error, format!("The draft failed: {err}")));
                            None
                        }
                    });
                }
                key = screen.next(Duration::from_millis(100)) => {
                    if screen.closed || key.is_some_and(|k| k.is_cancel()) {
                        self.status = Some((Tone::Info, "Cancelled the draft.".into()));
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn draw(&self, screen: &mut Screen, data: &Data) {
        let width = screen.width();
        let active = data.runs.iter().filter(|r| r.status.active()).count();
        let mut summary = format!("{} saved", data.workflows.len());
        if active > 0 {
            summary.push_str(&format!(" · {active} active"));
        }
        let waiting = data.runs.iter().filter(|r| r.approval.is_some()).count();
        if waiting > 0 {
            summary.push_str(&format!(" · {waiting} waiting for you"));
        }
        let labels = [
            format!("Workflows ({})", data.workflows.len()),
            format!("Runs ({})", data.runs.len()),
        ];
        let mut status = self.status.clone();
        if status.is_none() && !data.errors.is_empty() && self.tab == Tab::Workflows {
            let errors: Vec<String> = data
                .errors
                .iter()
                .map(|e| format!("Cannot read {e}"))
                .collect();
            status = Some((Tone::Warn, errors.join("\n")));
        }
        let only = self.only.as_ref().filter(|_| self.tab == Tab::Runs);
        let search_hint = match only {
            Some(name) => format!("runs of {name} · type to search · esc shows all runs"),
            None => "type to search".to_owned(),
        };
        let mut frame = Frame::new("Workflows")
            .summary(summary)
            .tabs(frame::tabs(&labels, self.index()))
            .search(&self.query, search_hint)
            .status(status);
        let busy = matches!(self.status, Some((Tone::Busy, _)));
        let esc = match (self.query.is_empty(), only) {
            (false, _) => ("esc", "clear search"),
            (true, Some(_)) => ("esc", "all runs"),
            (true, None) => ("esc", "close"),
        };
        let hints: Vec<(&str, &str)> = match (&self.view, self.tab) {
            _ if busy => vec![("esc", "cancel")],
            (View::Menu { .. }, _) => vec![("↑↓", "choose"), ("enter", "run"), ("esc", "back")],
            (View::Confirm { .. }, _) => {
                vec![("enter", "delete"), ("any other key", "cancels")]
            }
            (View::List, Tab::Workflows) => vec![
                ("↑↓", "move"),
                ("tab", "runs"),
                ("enter", "actions"),
                ("type", "to search"),
                esc,
            ],
            (View::List, Tab::Runs) => vec![
                ("↑↓", "move"),
                ("tab", "workflows"),
                ("enter", "open"),
                ("type", "to search"),
                esc,
            ],
        };
        frame = frame.hints(&hints);
        let budget = frame.budget(width, screen.height());
        let (body, focus) = match (&self.view, self.tab) {
            (View::List, Tab::Workflows) => self.workflow_list(data, budget),
            (View::List, Tab::Runs) => self.run_list(data, width, budget),
            (View::Menu { choice, .. }, _) => self.menu(data, Some(*choice), width),
            (View::Confirm { .. }, _) => {
                let (mut lines, _) = self.menu(data, None, width);
                if let Some(stored) = self.target(data) {
                    lines.push(String::new());
                    let question = format!(
                        "Delete {} ({})? Enter deletes it; any other key cancels.",
                        stored.workflow.name,
                        stored.path.display()
                    );
                    lines.extend(
                        crate::ui::wrap(&question, width.saturating_sub(2))
                            .iter()
                            .map(|l| warning(&format!(" {l}"))),
                    );
                }
                let focus = lines.len().saturating_sub(1);
                (lines, focus)
            }
        };
        screen.show(&frame, body, focus);
    }

    fn workflow_list(&self, data: &Data, budget: usize) -> (Vec<String>, usize) {
        let entries = self.entries(data);
        if entries.is_empty() {
            return (vec![muted("  No workflow matches.")], 0);
        }
        let name_width = data
            .workflows
            .iter()
            .map(|s| s.workflow.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(12, 32);
        let rows: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                Entry::New => accent("+ New workflow…"),
                Entry::Generate => accent("✦ Generate a workflow from a description…"),
                Entry::Workflow(stored) => workflow_row(stored, name_width),
            })
            .collect();
        frame::list(&rows, self.selected(), budget)
    }

    fn run_list(&self, data: &Data, width: usize, budget: usize) -> (Vec<String>, usize) {
        let runs = self.runs(data);
        if runs.is_empty() {
            let text = if self.query.is_empty() {
                "  No runs yet. Pick a workflow on the Workflows tab and choose Run."
            } else {
                "  No run matches."
            };
            return (vec![muted(text)], 0);
        }
        let name_width = runs
            .iter()
            .map(|r| r.workflow.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(10, 28);
        let rows: Vec<String> = runs
            .iter()
            .map(|run| run_row(run, name_width, width))
            .collect();
        frame::list(&rows, self.selected(), budget)
    }

    /// The details of the selected workflow and, with `choice`, its actions.
    fn menu(&self, data: &Data, choice: Option<Choice>, width: usize) -> (Vec<String>, usize) {
        let mut lines = Vec::new();
        let Some(stored) = self.target(data) else {
            return (lines, 0);
        };
        let wf = &stored.workflow;
        frame::field(&mut lines, "workflow", &style::bold(&wf.name), width);
        if !wf.description.is_empty() {
            frame::field(&mut lines, "about", &wf.description, width);
        }
        let saved = format!("{} · {}", stored.source.label(), stored.path.display());
        frame::field(&mut lines, "saved in", &saved, width);
        let mode = match wf.mode {
            Mode::Fixed => "fixed · runs the steps as written",
            Mode::Dynamic => "dynamic · a planner writes the steps before each one",
        };
        frame::field(&mut lines, "mode", mode, width);
        let context = format!("{} · {}", wf.context.label(), wf.context.describe());
        frame::field(&mut lines, "context", &context, width);
        let runs_in = if wf.background {
            "the background · keeps going after pi quits"
        } else {
            "the foreground · stops when pi quits"
        };
        frame::field(&mut lines, "runs in", runs_in, width);
        if !wf.steps.is_empty() {
            let steps: Vec<&str> = wf.steps.iter().map(|s| s.name.as_str()).collect();
            frame::field(&mut lines, "steps", &steps.join(" › "), width);
        }
        let inputs = wf.inputs();
        if !inputs.is_empty() {
            frame::field(&mut lines, "inputs", &inputs.join(", "), width);
        }
        let tasks: Vec<_> = wf.steps.iter().flat_map(|s| &s.tasks).collect();
        let asks = tasks.iter().filter(|t| t.confirm).count();
        let timed = tasks
            .iter()
            .filter(|t| t.delay.is_some() || t.until.is_some() || t.max_runs() > 1)
            .count();
        let mut facts = vec![format!("{} task(s)", tasks.len())];
        if asks > 0 {
            facts.push(format!("{asks} ask(s) you first"));
        }
        if timed > 0 {
            facts.push(format!("{timed} wait(s) or repeat(s)"));
        }
        frame::field(&mut lines, "tasks", &facts.join(" · "), width);
        if let Some(last) = data.runs.iter().find(|r| r.workflow.name == wf.name) {
            let text = format!("{} · {} · {}", last.status.label(), ago(last), last.id);
            frame::field(&mut lines, "last run", &text, width);
        }
        let Some(choice) = choice else {
            return (lines, 0);
        };
        lines.push(String::new());
        lines.push(accent(&style::bold(" Actions")));
        let choices = self.choices(data);
        let runs = data.runs_of(&wf.name);
        let mut focus = lines.len();
        for item in &choices {
            if *item == choice {
                focus = lines.len();
            }
            let risky = *item == Choice::Delete;
            lines.push(frame::choice(&item.label(runs), *item == choice, risky));
        }
        (lines, focus)
    }
}

fn workflow_row(stored: &Stored, name_width: usize) -> String {
    let wf = &stored.workflow;
    let steps = match (wf.mode, wf.steps.len()) {
        (Mode::Dynamic, _) => "planned".to_owned(),
        (_, 1) => "1 step".to_owned(),
        (_, n) => format!("{n} steps"),
    };
    let meta = format!("{steps} · {}", wf.context.label());
    let scope = match stored.source {
        crate::store::Source::Global => muted("global "),
        crate::store::Source::Project => accent("project"),
    };
    let about = if wf.description.is_empty() {
        String::new()
    } else {
        format!("  {}", muted(&one_line(&wf.description, 200)))
    };
    format!(
        "{:<name_width$}  {}  {scope}{about}",
        wf.name,
        muted(&format!("{meta:<18}"))
    )
}

fn run_row(run: &RunState, name_width: usize, width: usize) -> String {
    let name = one_line(&run.workflow.name, name_width);
    let label = format!("{:<11}", run.status.label());
    let status = match run.status {
        RunStatus::Running => accent(&label),
        RunStatus::Waiting | RunStatus::Interrupted => warning(&label),
        RunStatus::Done => frame::good(&label),
        RunStatus::Failed => frame::bad(&label),
        RunStatus::Cancelled => muted(&label),
    };
    let mut tags = Vec::new();
    if run.approval.is_some() {
        tags.push(warning("waits for you"));
    } else if run.status.active() && run.background {
        tags.push(accent("bg"));
    }
    let now = if run.status.active() {
        widget::now_line(run)
    } else {
        ago(run)
    };
    tags.push(muted(&one_line(&now, width / 2)));
    format!(
        "{} {name:<name_width$}  {status} {}  {}",
        crate::ui::glyph(run.status),
        muted(&format!("{:<18} {}", run.stats(), run.id)),
        tags.join(" ")
    )
}

/// `ended 2h ago`, or `started 5m ago` for a run without an end.
pub fn ago(run: &RunState) -> String {
    let (word, at) = match run.ended_ms {
        Some(ended) => ("ended", ended),
        None => ("started", run.started_ms),
    };
    let secs = now_ms().saturating_sub(at) / 1000;
    let age = if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        format_duration(Duration::from_secs(secs - secs % 60))
    };
    if secs < 60 {
        format!("{word} just now")
    } else {
        format!("{word} {age} ago")
    }
}
