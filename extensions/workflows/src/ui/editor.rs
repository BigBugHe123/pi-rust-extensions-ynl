//! The workflow editor: the step tree on the left and the form of the selected row on the right
//! (stacked on narrow terminals).

use std::path::PathBuf;

use pi_ext_sdk::InputOptions;
use pi_ext_sdk::prelude::*;
use pi_ext_sdk::style;

use crate::model::{Step, Task, Workflow, slug};
use crate::store::{Source, Stored};
use crate::ui::fields::{self, Edit, Field, Node};
use crate::ui::frame::{Frame, Tone, accent, muted, warning};
use crate::ui::{IDLE, Screen, Ui, one_line, run_view, short_model, start, window, wrap};

/// The editor puts the tree and the form side by side from this width.
const SIDE_BY_SIDE: usize = 96;

/// A question in the status line that the next key answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    DeleteStep(usize),
    Leave,
}

pub struct Editor {
    workflow: Workflow,
    source: Source,
    /// The file the workflow came from; a rename removes it after the save.
    path: Option<PathBuf>,
    dirty: bool,
    in_form: bool,
    row: usize,
    field: usize,
    message: Option<(Tone, String)>,
    pending: Option<Pending>,
}

impl Editor {
    fn new(workflow: Workflow, source: Source, path: Option<PathBuf>, dirty: bool) -> Self {
        Self {
            workflow,
            source,
            path,
            dirty,
            in_form: false,
            row: 0,
            field: 0,
            message: None,
            pending: None,
        }
    }

    pub fn blank() -> Self {
        let step = Step {
            name: "step-1".into(),
            tasks: vec![Task::default()],
            ..Step::default()
        };
        let mut editor = Self::new(
            Workflow {
                steps: vec![step],
                ..Workflow::default()
            },
            Source::Global,
            None,
            true,
        );
        editor.in_form = true;
        editor
    }

    pub fn from_stored(stored: Stored) -> Self {
        Self::new(stored.workflow, stored.source, Some(stored.path), false)
    }

    pub fn copy_of(stored: &Stored) -> Self {
        let mut workflow = stored.workflow.clone();
        workflow.name = format!("{}-copy", workflow.name);
        Self::new(workflow, stored.source, None, true)
    }

    pub fn draft(workflow: Workflow) -> Self {
        Self::new(workflow, Source::Global, None, true)
    }

    fn nodes(&self) -> Vec<Node> {
        let mut nodes = vec![Node::Settings];
        for (i, step) in self.workflow.steps.iter().enumerate() {
            nodes.push(Node::Step(i));
            nodes.extend((0..step.tasks.len()).map(|t| Node::Task(i, t)));
        }
        nodes.push(Node::AddStep);
        nodes
    }

    fn node(&self) -> Node {
        let nodes = self.nodes();
        nodes[self.row.min(nodes.len() - 1)]
    }

    fn select(&mut self, node: Node) {
        if let Some(row) = self.nodes().iter().position(|n| *n == node) {
            self.row = row;
        }
    }

    fn step_of(&self) -> Option<usize> {
        match self.node() {
            Node::Step(i) | Node::Task(i, _) => Some(i),
            _ => None,
        }
    }
}

pub async fn run(ui: &Ui, screen: &mut Screen, mut editor: Editor) -> Result<()> {
    loop {
        editor.show(ui, screen);
        let Some(key) = screen.next(IDLE).await else {
            if screen.closed {
                return Ok(());
            }
            continue;
        };
        editor.message = None;
        let leave = match editor.pending.take() {
            Some(pending) => editor.answer(ui, pending, &key).await?,
            None if editor.in_form => editor.form_key(ui, screen, &key).await?,
            None => editor.tree_key(ui, screen, &key).await?,
        };
        if leave || screen.closed {
            return Ok(());
        }
    }
}

impl Editor {
    /// `true` leaves the editor.
    async fn tree_key(&mut self, ui: &Ui, screen: &mut Screen, key: &Key) -> Result<bool> {
        let last = self.nodes().len() - 1;
        match key {
            Key::Char('K') => self.shift(false),
            Key::Char('J') => self.shift(true),
            k if k.is("up") || k.is("k") => self.row = self.row.saturating_sub(1),
            k if k.is("down") || k.is("j") => self.row = (self.row + 1).min(last),
            k if k.is("pageup") => self.row = self.row.saturating_sub(10),
            k if k.is("pagedown") => self.row = (self.row + 10).min(last),
            k if k.is("enter") || k.is("right") || k.is("tab") || k.is("l") => {
                if self.node() == Node::AddStep {
                    self.add_step(ui).await?;
                } else {
                    self.in_form = true;
                    self.field = 0;
                }
            }
            k if k.is("a") => self.add_step(ui).await?,
            k if k.is("t") => self.add_task(ui).await?,
            k if k.is("d") || k.is("delete") => self.delete(),
            k if k.is("c") => self.duplicate(),
            k if k.is("s") || k.is("ctrl+s") => {
                self.save(ui).await?;
            }
            k if k.is("r") => self.run_it(ui, screen).await?,
            k if k.is_cancel() || k.is("left") => return Ok(self.leave()),
            _ => {}
        }
        Ok(false)
    }

    async fn form_key(&mut self, ui: &Ui, screen: &mut Screen, key: &Key) -> Result<bool> {
        let node = self.node();
        let fields = fields::fields(node, &self.workflow);
        if fields.is_empty() {
            self.in_form = false;
            return Ok(false);
        }
        self.field = self.field.min(fields.len() - 1);
        let field = fields[self.field];
        // An edit that ends with the value it had leaves the workflow saved.
        let before = self.workflow.clone();
        match key {
            k if k.is("up") || k.is("k") => self.field = self.field.saturating_sub(1),
            k if k.is("down") || k.is("j") => self.field = (self.field + 1).min(fields.len() - 1),
            k if k.is("enter") || k.is("space") => {
                match fields::edit(field, node, &mut self.workflow, ui, screen).await? {
                    Edit::Changed => self.dirty |= self.workflow != before,
                    Edit::Invalid(message) => self.message = Some((Tone::Error, message)),
                    Edit::Unchanged => {}
                }
            }
            k if k.is("x") || k.is("delete") => {
                fields::reset(field, node, &mut self.workflow);
                self.dirty |= self.workflow != before;
            }
            k if k.is("s") || k.is("ctrl+s") => {
                self.save(ui).await?;
            }
            k if k.is("r") => self.run_it(ui, screen).await?,
            k if k.is("ctrl+c") => return Ok(self.leave()),
            k if k.is_cancel() || k.is("left") || k.is("h") || k.is("tab") => self.in_form = false,
            _ => {}
        }
        Ok(false)
    }

    /// The answer to a question in the status line; `true` leaves the editor.
    async fn answer(&mut self, ui: &Ui, pending: Pending, key: &Key) -> Result<bool> {
        match pending {
            Pending::DeleteStep(i) if *key == Key::Enter && i < self.workflow.steps.len() => {
                let step = self.workflow.steps.remove(i);
                self.dirty = true;
                self.row = self.row.min(self.nodes().len() - 1);
                self.message = Some((Tone::Good, format!("Deleted step {}.", step.name)));
            }
            Pending::Leave if key.is("s") => return self.save(ui).await,
            Pending::Leave if key.is("d") => return Ok(true),
            _ => self.message = Some((Tone::Info, "Cancelled.".into())),
        }
        Ok(false)
    }

    async fn add_step(&mut self, ui: &Ui) -> Result<()> {
        let options = InputOptions {
            placeholder: Some("review".into()),
            ..InputOptions::default()
        };
        let Some(name) = ui.host.input_with("Name of the new step", options).await? else {
            return Ok(());
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Ok(());
        }
        let mut task = Task::default();
        let title = format!("First task of {name} ({{{{name}}}} becomes an input)");
        if !fields::long_text(ui, &title, &mut task.prompt).await? {
            return Ok(());
        }
        let at = match self.node() {
            Node::Settings => 0,
            Node::Step(i) | Node::Task(i, _) => i + 1,
            Node::AddStep => self.workflow.steps.len(),
        };
        self.workflow.steps.insert(
            at,
            Step {
                name,
                tasks: vec![task],
                ..Step::default()
            },
        );
        self.dirty = true;
        self.select(Node::Step(at));
        Ok(())
    }

    async fn add_task(&mut self, ui: &Ui) -> Result<()> {
        let Some(step) = self.step_of() else {
            self.message = Some((
                Tone::Warn,
                "Select a step, then press t to add a task to it.".into(),
            ));
            return Ok(());
        };
        let mut task = Task::default();
        let title = format!("New task of {}", self.workflow.steps[step].name);
        if !fields::long_text(ui, &title, &mut task.prompt).await? {
            return Ok(());
        }
        let at = match self.node() {
            Node::Task(_, t) => t + 1,
            _ => self.workflow.steps[step].tasks.len(),
        };
        self.workflow.steps[step].tasks.insert(at, task);
        self.dirty = true;
        self.select(Node::Task(step, at));
        Ok(())
    }

    fn delete(&mut self) {
        match self.node() {
            Node::Step(i) => self.pending = Some(Pending::DeleteStep(i)),
            Node::Task(s, _) if self.workflow.steps[s].tasks.len() == 1 => {
                self.message = Some((
                    Tone::Warn,
                    "A step needs a task. Delete the step instead.".into(),
                ));
            }
            Node::Task(s, t) => {
                self.workflow.steps[s].tasks.remove(t);
                self.dirty = true;
                self.row = self.row.min(self.nodes().len() - 1);
            }
            _ => {}
        }
    }

    fn shift(&mut self, down: bool) {
        let target = |i: usize, len: usize| {
            if down {
                (i + 1 < len).then_some(i + 1)
            } else {
                i.checked_sub(1)
            }
        };
        match self.node() {
            Node::Step(i) => {
                if let Some(j) = target(i, self.workflow.steps.len()) {
                    self.workflow.steps.swap(i, j);
                    self.dirty = true;
                    self.select(Node::Step(j));
                }
            }
            Node::Task(s, t) => {
                if let Some(j) = target(t, self.workflow.steps[s].tasks.len()) {
                    self.workflow.steps[s].tasks.swap(t, j);
                    self.dirty = true;
                    self.select(Node::Task(s, j));
                }
            }
            _ => {}
        }
    }

    fn duplicate(&mut self) {
        match self.node() {
            Node::Step(i) => {
                let mut step = self.workflow.steps[i].clone();
                step.name = format!("{}-copy", step.name);
                self.workflow.steps.insert(i + 1, step);
                self.select(Node::Step(i + 1));
            }
            Node::Task(s, t) => {
                let task = self.workflow.steps[s].tasks[t].clone();
                self.workflow.steps[s].tasks.insert(t + 1, task);
                self.select(Node::Task(s, t + 1));
            }
            _ => return,
        }
        self.dirty = true;
    }

    /// `true` when the workflow is saved.
    async fn save(&mut self, ui: &Ui) -> Result<bool> {
        if let Some(problem) = self.workflow.problems().first() {
            self.message = Some((Tone::Error, format!("Fix this first: {problem}")));
            return Ok(false);
        }
        let dirs = ui.app.dirs(&ui.host);
        if self.path.is_none() && dirs.project.is_some() {
            let options = [
                "Global · every project",
                "Project · .pi folder of this project",
            ];
            let Some(choice) = ui.host.select("Save the workflow to", &options).await? else {
                return Ok(false);
            };
            self.source = if choice.starts_with("Project") {
                Source::Project
            } else {
                Source::Global
            };
        }
        let name = slug(&self.workflow.name);
        let target = dirs
            .dir(self.source)
            .map(|dir| dir.join(format!("{name}.json")));
        if let Some(target) = target.filter(|t| t.exists() && Some(t) != self.path.as_ref()) {
            let question = format!("Replace the workflow {name}?");
            if !ui
                .host
                .confirm(&question, &target.display().to_string())
                .await?
            {
                return Ok(false);
            }
        }
        match dirs.save(&self.workflow, self.source) {
            Ok(path) => {
                if let Some(old) = self.path.take().filter(|old| *old != path) {
                    let _ = std::fs::remove_file(old);
                }
                self.message = Some((Tone::Good, format!("Saved {}.", path.display())));
                self.path = Some(path);
                self.workflow.name = name;
                self.dirty = false;
                Ok(true)
            }
            Err(err) => {
                self.message = Some((Tone::Error, err));
                Ok(false)
            }
        }
    }

    async fn run_it(&mut self, ui: &Ui, screen: &mut Screen) -> Result<()> {
        if self.dirty && !self.save(ui).await? {
            return Ok(());
        }
        match start::run_dialog(ui, screen, self.workflow.clone()).await? {
            Ok(Some(started)) => {
                run_view::run(ui, screen, &started.id, Some(started.message)).await?
            }
            Ok(None) => {}
            Err(err) => self.message = Some((Tone::Error, err)),
        }
        Ok(())
    }

    /// `true` leaves now; with unsaved changes the status line asks first.
    fn leave(&mut self) -> bool {
        if self.dirty {
            self.pending = Some(Pending::Leave);
        }
        !self.dirty
    }

    fn show(&self, ui: &Ui, screen: &mut Screen) {
        let width = screen.width();
        let name = if self.workflow.name.is_empty() {
            "New workflow"
        } else {
            &self.workflow.name
        };
        let state = if self.dirty {
            "unsaved changes".to_owned()
        } else {
            format!("saved · {}", self.source.label())
        };
        let keys: &[(&str, &str)] = match (self.pending, self.in_form) {
            (Some(Pending::DeleteStep(_)), _) => {
                &[("enter", "delete"), ("any other key", "cancels")]
            }
            (Some(Pending::Leave), _) => &[
                ("s", "save and close"),
                ("d", "discard and close"),
                ("any other key", "keeps editing"),
            ],
            (None, true) => &[
                ("↑↓", "field"),
                ("enter", "change"),
                ("x", "reset"),
                ("esc", "steps"),
                ("s", "save"),
                ("r", "run"),
            ],
            (None, false) => &[
                ("↑↓", "move"),
                ("enter", "edit"),
                ("a", "add step"),
                ("t", "add task"),
                ("J/K", "reorder"),
                ("c", "copy"),
                ("d", "delete"),
                ("s", "save"),
                ("r", "run"),
                ("esc", "back"),
            ],
        };
        let frame = Frame::new(name)
            .summary(state)
            .status(Some(self.status()))
            .hints(keys);
        let budget = frame.budget(width, screen.height());
        let mut body = Vec::new();
        if width >= SIDE_BY_SIDE {
            let left = (width * 2 / 5).clamp(32, 56);
            let right = width - left - 3;
            let tree = self.tree_lines(left, budget);
            let (form, _) = self.form_lines(ui, right, budget);
            for i in 0..tree.len().max(form.len()) {
                let l = style::fit(tree.get(i).map_or("", String::as_str), left);
                let r = form.get(i).cloned().unwrap_or_default();
                body.push(format!("{l}{}{r}", muted(" │ ")));
            }
        } else {
            // The form gets the larger part while its fields are edited.
            let share = if self.in_form { budget / 3 } else { budget / 2 };
            body.extend(self.tree_lines(width, share.max(1)));
            body.push(String::new());
            let rest = budget.saturating_sub(body.len()).max(1);
            let (form, focus) = self.form_lines(ui, width, rest);
            let focus = body.len() + focus;
            body.extend(form);
            screen.show(&frame, body, focus);
            return;
        }
        screen.show(&frame, body, 0);
    }

    fn status(&self) -> (Tone, String) {
        let deleting = match self.pending {
            Some(Pending::DeleteStep(i)) => self.workflow.steps.get(i),
            _ => None,
        };
        if let Some(step) = deleting {
            return (
                Tone::Warn,
                format!(
                    "Delete step {} and its {} task(s)? Enter deletes it; any other key cancels.",
                    step.name,
                    step.tasks.len()
                ),
            );
        }
        if self.pending == Some(Pending::Leave) {
            return (
                Tone::Warn,
                "The workflow has unsaved changes. s saves and closes it, d closes it without saving, any other key keeps editing.".into(),
            );
        }
        if let Some(message) = &self.message {
            return message.clone();
        }
        match self.workflow.problems().as_slice() {
            [] => {
                let inputs = self.workflow.inputs();
                let inputs = if inputs.is_empty() {
                    String::new()
                } else {
                    format!(" · inputs: {}", inputs.join(", "))
                };
                (Tone::Good, format!("✓ Ready to run{inputs}"))
            }
            [only] => (Tone::Warn, format!("⚠ {only}")),
            [first, rest @ ..] => (Tone::Warn, format!("⚠ {first} (+{} more)", rest.len())),
        }
    }

    /// The tree rows around the selected row, with `(i/n)` when they do not all fit.
    fn tree_lines(&self, width: usize, rows: usize) -> Vec<String> {
        let nodes = self.nodes();
        let row = self.row.min(nodes.len() - 1);
        let more = nodes.len() > rows;
        let fit = if more {
            rows.saturating_sub(1).max(1)
        } else {
            rows
        };
        let mut lines = Vec::new();
        for i in window(row, nodes.len(), fit) {
            let (text, meta) = self.tree_row(nodes[i]);
            let inner = width.saturating_sub(2);
            let meta = muted(&meta);
            let room = inner.saturating_sub(style::visible_width(&meta) + 1);
            let text = style::truncate(&text, room);
            let gap =
                inner.saturating_sub(style::visible_width(&text) + style::visible_width(&meta));
            let line = format!("{text}{}{meta}", " ".repeat(gap));
            let pointer = match (i == row, self.in_form) {
                (true, false) => accent("→ "),
                (true, true) => muted("› "),
                _ => "  ".to_owned(),
            };
            lines.push(format!("{pointer}{line}"));
        }
        if more {
            lines.push(muted(&format!("  ({}/{})", row + 1, nodes.len())));
        }
        lines
    }

    fn tree_row(&self, node: Node) -> (String, String) {
        let wf = &self.workflow;
        match node {
            Node::Settings => {
                let model = wf
                    .agent
                    .model
                    .as_deref()
                    .map(short_model)
                    .unwrap_or("session model");
                (
                    style::bold("≡ Workflow"),
                    format!("{} · {} · {model}", wf.mode.label(), wf.context.label()),
                )
            }
            Node::Step(i) => {
                let step = &wf.steps[i];
                let scope = &step.agent;
                let mut meta: Vec<&str> = Vec::new();
                meta.extend(scope.model.as_deref().map(short_model));
                meta.extend(scope.thinking.as_deref());
                let meta = if meta.is_empty() {
                    "inherits".to_owned()
                } else {
                    meta.join(" · ")
                };
                (
                    format!(
                        "{} {}",
                        accent(&format!("{:>2}", i + 1)),
                        style::bold(&step.name)
                    ),
                    meta,
                )
            }
            Node::Task(s, t) => {
                let task = &wf.steps[s].tasks[t];
                let letter = char::from(b'a' + (t % 26) as u8);
                let mut badges: Vec<String> = Vec::new();
                if let Some(skill) = &task.skill {
                    badges.push(accent(&format!("/{skill}")));
                }
                if let Some(delay) = &task.delay {
                    badges.push(warning(&format!("◷{delay}")));
                }
                if task.until.is_some() {
                    badges.push(warning("↻goal"));
                } else if task.max_runs() > 1 {
                    let every = task
                        .every
                        .as_deref()
                        .map(|e| format!("/{e}"))
                        .unwrap_or_default();
                    badges.push(warning(&format!("×{}{every}", task.max_runs())));
                }
                if task.confirm {
                    badges.push(warning("asks"));
                }
                if task.optional {
                    badges.push(muted("optional"));
                }
                let prompt = if task.prompt.trim().is_empty() {
                    warning("(empty prompt)")
                } else {
                    muted(&one_line(&task.prompt, 200))
                };
                let badges = if badges.is_empty() {
                    String::new()
                } else {
                    format!("{} ", badges.join(" "))
                };
                (
                    format!("   {} {badges}{prompt}", muted(&letter.to_string())),
                    String::new(),
                )
            }
            Node::AddStep => (accent("+ Add a step…"), String::new()),
        }
    }

    /// The form of the selected row, cut to `rows` lines around the selected field, and the
    /// line of that field.
    fn form_lines(&self, ui: &Ui, width: usize, rows: usize) -> (Vec<String>, usize) {
        let p = &ui.paint;
        let wf = &self.workflow;
        let node = self.node();
        let title = match node {
            Node::Settings => "Workflow settings".to_owned(),
            Node::Step(i) => format!("Step {} · {}", i + 1, wf.steps[i].name),
            Node::Task(s, t) => format!(
                "Task {} of {}",
                char::from(b'a' + (t % 26) as u8),
                wf.steps[s].name
            ),
            Node::AddStep => "Add a step".to_owned(),
        };
        let mut head = vec![accent(&style::bold(&title))];
        let fields = fields::fields(node, wf);
        if fields.is_empty() {
            head.push(muted("Press enter to add a step at the end."));
            return (head, 0);
        }
        let selected = self.field.min(fields.len() - 1);
        let mut lines: Vec<String> = Vec::new();
        let mut focus = 0;
        for (i, field) in fields.iter().enumerate() {
            if node == Node::Settings && *field == Field::Model {
                lines.push(muted("Defaults for every step"));
            }
            if *field == Field::PlannerRules {
                lines.push(muted("Planner"));
            }
            let value = fields::value(*field, node, wf, p);
            let label = muted(&format!("{:<15}", fields::label(*field)));
            let on = self.in_form && i == selected;
            if on {
                focus = lines.len();
            }
            let pointer = if on { accent("→ ") } else { "  ".to_owned() };
            lines.push(format!("{pointer}{label}{value}"));
        }
        let mut tail = vec![String::new()];
        let help = if self.in_form {
            fields::help(fields[selected])
        } else {
            "Press enter to edit this row."
        };
        tail.extend(
            wrap(help, width.saturating_sub(2))
                .iter()
                .map(|l| muted(&format!("  {l}"))),
        );
        if let Node::Task(s, t) = node {
            let prompt = &wf.steps[s].tasks[t].prompt;
            if prompt.lines().count() > 1 || prompt.chars().count() > width.saturating_sub(18) {
                tail.push(String::new());
                tail.extend(
                    wrap(prompt, width.saturating_sub(4))
                        .iter()
                        .take(8)
                        .map(|l| format!("  {}", muted(l))),
                );
            }
        }
        // The fields keep the rows that the title, the position line, and the help leave.
        let room = rows.saturating_sub(head.len() + tail.len() + 1).max(1);
        let shown = window(focus, lines.len(), room);
        let line = head.len() + focus - shown.start;
        let mut out = head;
        out.extend(lines[shown.clone()].iter().cloned());
        if shown.end < lines.len() || shown.start > 0 {
            out.push(muted(&if self.in_form {
                format!("  ({}/{} fields)", selected + 1, fields.len())
            } else {
                format!("  ↓ {} more fields", lines.len() - shown.end)
            }));
        }
        out.extend(tail);
        out.truncate(rows);
        (out, line)
    }
}
