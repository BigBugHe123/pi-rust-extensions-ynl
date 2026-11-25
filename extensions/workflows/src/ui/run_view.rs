//! One run: its steps with time, cost, and model, what it does now, and the log, report, and
//! live agent output of the selected step, with the actions that apply to the run.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use pi_ext_sdk::prelude::*;
use pi_ext_sdk::style;

use crate::child::{Entry, EntryKind};
use crate::control::Decision;
use crate::model::format_duration;
use crate::runs::{self, RunState, RunStatus, StepStatus};
use crate::ui::frame::{self, Frame, Tone, accent, bad, muted, warning};
use crate::ui::{Screen, Ui, one_line, short_model, widget, wrap};

const TICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Approve,
    SkipTask,
    Message,
    EndWait,
    Background,
    Foreground,
    Stop,
    Resume,
    Delete,
}

impl Act {
    fn label(self, run: &RunState) -> String {
        let task = run.approval.as_ref().map(|a| a.task.as_str()).unwrap_or("");
        match self {
            Act::Approve => format!("Approve \"{task}\" and run it"),
            Act::SkipTask => format!("Skip \"{task}\""),
            Act::Message => "Send a message to the agent".into(),
            Act::EndWait => "End the wait now".into(),
            Act::Background => "Move to the background (keeps going after pi quits)".into(),
            Act::Foreground => "Move to the foreground (stops when pi quits)".into(),
            Act::Stop => "Stop the run".into(),
            Act::Resume => "Resume at the task that did not finish".into(),
            Act::Delete => "Delete the run".into(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Act::Approve => "a",
            Act::SkipTask => "n",
            Act::Message => "m",
            Act::EndWait => "s",
            Act::Background | Act::Foreground => "b",
            Act::Stop => "x",
            Act::Resume => "r",
            Act::Delete => "d",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Act::Approve => "approve",
            Act::SkipTask => "skip task",
            Act::Message => "message",
            Act::EndWait => "end wait",
            Act::Background => "background",
            Act::Foreground => "foreground",
            Act::Stop => "stop",
            Act::Resume => "resume",
            Act::Delete => "delete",
        }
    }

    fn risky(self) -> bool {
        matches!(self, Act::Stop | Act::Delete)
    }
}

/// The actions that apply to the run now.
fn acts(run: &RunState) -> Vec<Act> {
    let mut acts = Vec::new();
    if run.approval.is_some() && run.status.active() {
        acts.extend([Act::Approve, Act::SkipTask]);
    }
    if run.agent_step().is_some() {
        acts.push(Act::Message);
    }
    if run.status.active() {
        if run.wait_until_ms.is_some() {
            acts.push(Act::EndWait);
        }
        acts.push(if run.background {
            Act::Foreground
        } else {
            Act::Background
        });
        acts.push(Act::Stop);
    } else {
        if run.status != RunStatus::Done {
            acts.push(Act::Resume);
        }
        acts.push(Act::Delete);
    }
    acts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Detail,
    /// The actions pane, with the action under the pointer. It holds the action itself, as the
    /// list changes while the run goes on (an approval adds two actions at the top).
    Actions(Act),
    Confirm(Act),
}

/// The output of one step, read again only when its file changes.
#[derive(Default)]
struct Output {
    path: PathBuf,
    stamp: Option<(u64, SystemTime)>,
    entries: Vec<Entry>,
}

impl Output {
    fn read(&mut self, path: PathBuf) -> &[Entry] {
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        if path != self.path || stamp != self.stamp || stamp.is_none() {
            self.entries = runs::read_output(&path);
            self.path = path;
            self.stamp = stamp;
        }
        &self.entries
    }
}

struct View {
    selected: usize,
    scroll: usize,
    /// How far the detail scrolls, as the last detail drawing found it.
    max_scroll: usize,
    /// Until the user picks a step, the view follows the run's current step.
    picked: bool,
    /// While the selected step's agent works, the view keeps the newest output in sight.
    follow: bool,
    mode: Mode,
    status: Option<(Tone, String)>,
}

pub async fn run(ui: &Ui, screen: &mut Screen, id: &str, message: Option<String>) -> Result<()> {
    let mut view = View {
        selected: ui
            .app
            .runs
            .get(id)
            .and_then(|r| r.current_step())
            .unwrap_or(0),
        scroll: 0,
        max_scroll: 0,
        picked: false,
        follow: true,
        mode: Mode::Detail,
        status: message.map(|m| (Tone::Good, m)),
    };
    let mut output = Output::default();
    loop {
        let Some(run) = ui.app.find(&ui.host, id) else {
            return Ok(());
        };
        let last = run.steps.len().saturating_sub(1);
        if !view.picked
            && let Some(current) = run.current_step()
        {
            view.selected = current;
        }
        view.selected = view.selected.min(last);
        let entries = match ui.app.runs.dir(id) {
            Some(dir) => output.read(runs::output_file(&dir, view.selected)),
            None => &[],
        };
        let running = run
            .steps
            .get(view.selected)
            .is_some_and(|s| s.status == StepStatus::Running);
        if view.follow && running {
            view.scroll = usize::MAX;
        }
        // The actions pane hides the detail, so it keeps the scroll it had.
        if let Some(max_scroll) = view.show(ui, screen, &run, entries) {
            view.max_scroll = max_scroll;
            view.scroll = view.scroll.min(max_scroll);
        }
        let max_scroll = view.max_scroll;
        let Some(key) = screen.next(TICK).await else {
            if screen.closed {
                return Ok(());
            }
            continue;
        };
        let acts = acts(&run);
        if let Mode::Actions(act) = view.mode
            && !acts.contains(&act)
        {
            view.mode = Mode::Detail;
            view.status = Some((
                Tone::Warn,
                "The run changed, so its actions changed. Press enter to see them.".into(),
            ));
            continue;
        }
        match view.mode {
            Mode::Confirm(act) => {
                view.mode = Mode::Detail;
                if key == Key::Enter {
                    if view.apply(ui, act, &run).await? {
                        return Ok(());
                    }
                } else {
                    view.status = Some((Tone::Info, "Cancelled.".into()));
                }
                continue;
            }
            Mode::Actions(act) => {
                let len = acts.len();
                let at = acts.iter().position(|a| *a == act).unwrap_or(0);
                match key {
                    Key::Up => view.mode = Mode::Actions(acts[(at + len - 1) % len]),
                    Key::Down => view.mode = Mode::Actions(acts[(at + 1) % len]),
                    Key::Enter => {
                        view.mode = Mode::Detail;
                        if view.choose(ui, act, &run).await? {
                            return Ok(());
                        }
                    }
                    k if k.is_cancel() || k.is("left") || k.is("backspace") => {
                        view.mode = Mode::Detail;
                    }
                    _ => {}
                }
                continue;
            }
            Mode::Detail => {}
        }
        view.status = None;
        if key.is_cancel() || key.is("left") {
            return Ok(());
        } else if key.is("up") || key.is("k") {
            view.selected = view.selected.saturating_sub(1);
            (view.scroll, view.follow, view.picked) = (0, true, true);
        } else if key.is("down") || key.is("j") {
            view.selected = (view.selected + 1).min(last);
            (view.scroll, view.follow, view.picked) = (0, true, true);
        } else if key.is("pageup") {
            view.scroll = view.scroll.saturating_sub(10);
            view.follow = false;
        } else if key.is("pagedown") || key.is("space") {
            view.scroll = (view.scroll + 10).min(max_scroll);
            view.follow = view.scroll == max_scroll;
        } else if key.is("home") {
            (view.scroll, view.follow) = (0, false);
        } else if key.is("end") {
            (view.scroll, view.follow) = (max_scroll, true);
        } else if key.is("enter") {
            view.mode = Mode::Actions(acts[0]);
        } else if let Some(&act) = acts.iter().find(|a| key.is(a.key()))
            && view.choose(ui, act, &run).await?
        {
            return Ok(());
        }
    }
}

impl View {
    /// Runs `act`, after a question for the risky ones; `true` leaves the view.
    async fn choose(&mut self, ui: &Ui, act: Act, run: &RunState) -> Result<bool> {
        if act.risky() {
            self.mode = Mode::Confirm(act);
            self.status = None;
            return Ok(false);
        }
        self.apply(ui, act, run).await
    }

    /// `true` leaves the view.
    async fn apply(&mut self, ui: &Ui, act: Act, run: &RunState) -> Result<bool> {
        let app = &ui.app;
        let id = run.id.as_str();
        let result = match act {
            Act::Approve => app.approve(id, Decision::Run),
            Act::SkipTask => app.approve(id, Decision::Skip),
            Act::Message => {
                let title = "Message to the agent (it reads it after its current tool call)";
                let Some(message) = ui.host.input(title, None).await? else {
                    return Ok(false);
                };
                if message.trim().is_empty() {
                    return Ok(false);
                }
                app.steer(id, &message)
                    .map(|()| "Sent. The agent reads it after its current tool call.".into())
            }
            Act::EndWait => app.skip_wait(id).map(|()| "Ended the wait.".into()),
            Act::Background => app
                .set_background(id, true)
                .map(|()| "The run keeps going after pi quits.".into()),
            Act::Foreground => app
                .set_background(id, false)
                .map(|()| "The run stops when this pi quits; resume continues it.".into()),
            Act::Stop => app.stop(id).map(|()| "Stopping the run.".into()),
            Act::Resume => app
                .resume(&ui.host, id)
                .await
                .map(|()| "Resumed the run at the task that did not finish.".into()),
            Act::Delete => match app.delete(&ui.host, id) {
                Ok(()) => return Ok(true),
                Err(err) => Err(err),
            },
        };
        self.status = Some(match result {
            Ok(text) => (Tone::Good, text),
            Err(err) => (Tone::Error, err),
        });
        Ok(false)
    }

    /// Draws the view; returns how far the detail can scroll, or `None` while the actions pane
    /// hides the detail.
    fn show(
        &self,
        ui: &Ui,
        screen: &mut Screen,
        run: &RunState,
        output: &[Entry],
    ) -> Option<usize> {
        let width = screen.width();
        let mode = match (run.status.active(), run.background) {
            (true, true) => " · background",
            (true, false) => " · foreground",
            (false, _) => "",
        };
        let summary = format!(
            "{}{mode} · {} · {}",
            run.status.label(),
            run.stats(),
            run.id
        );
        let acts = acts(run);
        let mut hints: Vec<(&str, &str)> = Vec::new();
        match self.mode {
            Mode::Detail => {
                hints.extend([
                    ("↑↓", "step"),
                    ("pgup/pgdn", "scroll"),
                    ("enter", "actions"),
                ]);
                hints.extend(acts.iter().map(|a| (a.key(), a.hint())));
                hints.push(("esc", "back"));
            }
            Mode::Actions(_) => {
                hints.extend([("↑↓", "choose"), ("enter", "run"), ("esc", "back")]);
            }
            Mode::Confirm(act) => {
                hints.extend([("enter", act.hint()), ("any other key", "cancels")])
            }
        }
        let status = match self.mode {
            Mode::Confirm(Act::Stop) => Some((
                Tone::Warn,
                format!(
                    "Stop the run of {}? You can resume it later at the task that did not finish. Enter stops it; any other key cancels.",
                    run.workflow.name
                ),
            )),
            Mode::Confirm(_) => Some((
                Tone::Warn,
                format!(
                    "Delete run {} and its files? Enter deletes it; any other key cancels.",
                    run.id
                ),
            )),
            _ => self.status.clone(),
        };
        let frame = Frame::new(run.workflow.name.clone())
            .summary(summary)
            .status(status)
            .hints(&hints);
        let budget = frame.budget(width, screen.height());
        // Short panels drop the blank lines and keep one line of the error.
        let roomy = budget >= 14;
        let mut lines = self.steps(ui, run, width, (budget / 3).max(1));
        if roomy {
            lines.push(String::new());
        }
        if let Some(approval) = run.approval.as_ref().filter(|_| run.status.active()) {
            let ask = format!(
                " ? \"{}\" waits for your approval. a approves it, n skips it.",
                approval.task
            );
            lines.push(warning(&ask));
        } else {
            let now = widget::now_line(run);
            if run.status.active() && !now.is_empty() {
                lines.push(accent(&format!(" {now}")));
            }
        }
        if let Some(error) = &run.error {
            lines.extend(
                wrap(error, width.saturating_sub(4))
                    .iter()
                    .take(if roomy { 4 } else { 1 })
                    .map(|l| bad(&format!(" ✗ {l}"))),
            );
        }
        if let Mode::Actions(chosen) = self.mode {
            lines.push(String::new());
            lines.push(accent(&style::bold(" Actions")));
            let mut focus = lines.len();
            for act in &acts {
                if *act == chosen {
                    focus = lines.len();
                }
                lines.push(frame::choice(&act.label(run), *act == chosen, act.risky()));
            }
            screen.show(&frame, lines, focus);
            return None;
        }
        if roomy && lines.last().is_some_and(|l| !l.is_empty()) {
            lines.push(String::new());
        }
        let detail = self.detail(ui, run, output, width);
        // Two lines stay for the markers of the lines above and below.
        let room = budget.saturating_sub(lines.len() + 2).max(1);
        let max_scroll = detail.len().saturating_sub(room);
        let scroll = self.scroll.min(max_scroll);
        if scroll > 0 {
            lines.push(muted(&format!("  ↑ {scroll} earlier lines · pgup")));
        }
        let first = lines.len();
        lines.extend(detail.iter().skip(scroll).take(room).cloned());
        if scroll + room < detail.len() {
            let below = detail.len() - scroll - room;
            lines.push(muted(&format!("  ↓ {below} more lines · pgdn")));
        }
        // When the lines above leave the detail no room, the panel cuts around the output.
        let focus = if scroll == max_scroll && max_scroll > 0 {
            lines.len() - 1
        } else {
            first
        };
        screen.show(&frame, lines, focus);
        Some(max_scroll)
    }

    fn steps(&self, ui: &Ui, run: &RunState, width: usize, rows: usize) -> Vec<String> {
        let p = &ui.paint;
        if run.steps.is_empty() {
            return vec![muted("  The planner has not planned any steps yet.")];
        }
        let name_width = run
            .workflow
            .steps
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(8, 24);
        let model_width = (width / 5).clamp(12, 26);
        let mut lines = Vec::new();
        for (step, state) in run.workflow.steps.iter().zip(&run.steps) {
            let time = state
                .elapsed()
                .map(|d| format_duration(Duration::from_secs(d.as_secs())))
                .unwrap_or_default();
            let model = state
                .model
                .as_deref()
                .or(step.agent.model.as_deref())
                .map(short_model)
                .unwrap_or("");
            let model = one_line(model, model_width);
            let cost = if state.cost > 0.0 {
                format!("${:.2}", state.cost)
            } else {
                String::new()
            };
            let runs = match state.runs {
                0 => format!("{} task(s)", step.tasks.len()),
                1 => "1 agent run".to_owned(),
                n => format!("{n} agent runs"),
            };
            let name = one_line(&step.name, name_width);
            lines.push(format!(
                "{} {name:<name_width$}  {}",
                p.step_glyph(state.status),
                muted(&format!(
                    "{model:<model_width$} {time:>7} {cost:>7}  {runs}"
                ))
            ));
        }
        frame::list(&lines, self.selected, rows).0
    }

    fn detail(&self, ui: &Ui, run: &RunState, output: &[Entry], width: usize) -> Vec<String> {
        let mut detail: Vec<String> = Vec::new();
        let (Some(step), Some(state)) = (
            run.workflow.steps.get(self.selected),
            run.steps.get(self.selected),
        ) else {
            return detail;
        };
        detail.push(accent(&style::bold(&format!(
            " {} · {}",
            step.name,
            state.status.label()
        ))));
        for entry in &state.log {
            detail.extend(
                wrap(entry, width.saturating_sub(5))
                    .iter()
                    .map(|l| muted(&format!("  · {l}"))),
            );
        }
        if !state.summary.is_empty() {
            detail.push(String::new());
            detail.push(muted(" Report"));
            detail.extend(
                wrap(&state.summary, width.saturating_sub(4))
                    .iter()
                    .map(|l| format!("  {l}")),
            );
        }
        if !output.is_empty() || !state.writing.is_empty() {
            detail.push(String::new());
            detail.push(muted(" Output"));
            let text_width = width.saturating_sub(4);
            for entry in output {
                detail.extend(output_lines(ui, entry, text_width));
            }
            if !state.writing.is_empty() {
                let mut writing = wrap(&state.writing, text_width);
                if let Some(last) = writing.last_mut() {
                    last.push('▍');
                }
                detail.extend(writing.iter().map(|l| format!("  {l}")));
            }
        }
        if detail.len() == 1 {
            detail.push(muted("  Nothing yet."));
        }
        detail
    }
}

/// How many lines of a prompt or a tool error the output shows.
const SHORT_LINES: usize = 3;

fn output_lines(ui: &Ui, entry: &Entry, width: usize) -> Vec<String> {
    let p = &ui.paint;
    let short = |lines: Vec<String>| -> Vec<String> {
        let lines: Vec<String> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
        let more = lines.len() > SHORT_LINES;
        let mut lines: Vec<String> = lines.into_iter().take(SHORT_LINES).collect();
        if more && let Some(last) = lines.last_mut() {
            last.push_str(" …");
        }
        lines
    };
    match entry.kind {
        EntryKind::Prompt => short(wrap(&entry.text, width.saturating_sub(2)))
            .iter()
            .map(|l| p.dim(&format!("  › {l}")))
            .collect(),
        EntryKind::User => wrap(&entry.text, width.saturating_sub(4))
            .iter()
            .enumerate()
            .map(|(i, l)| p.accent(&format!("  {} {l}", if i == 0 { "›" } else { " " })))
            .collect(),
        EntryKind::Assistant => wrap(&entry.text, width)
            .iter()
            .map(|l| format!("  {l}"))
            .collect(),
        EntryKind::Tool => vec![p.muted(&format!(
            "  ⏵ {}",
            one_line(&entry.text, width.saturating_sub(2))
        ))],
        EntryKind::Error => short(wrap(&entry.text, width.saturating_sub(2)))
            .iter()
            .map(|l| p.fg("error", &format!("  ✗ {l}")))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::{Approval, StepRun};

    #[test]
    fn actions_follow_the_run_state() {
        let mut run = RunState {
            status: RunStatus::Waiting,
            approval: Some(Approval::default()),
            steps: vec![StepRun::default()],
            ..RunState::default()
        };
        assert_eq!(
            acts(&run),
            [Act::Approve, Act::SkipTask, Act::Background, Act::Stop]
        );
        run.approval = None;
        run.wait_until_ms = Some(1);
        run.background = true;
        run.steps[0].activity = "bash make".into();
        assert_eq!(
            acts(&run),
            [Act::Message, Act::EndWait, Act::Foreground, Act::Stop]
        );
        run.end(RunStatus::Failed);
        assert_eq!(acts(&run), [Act::Resume, Act::Delete]);
        run.status = RunStatus::Done;
        assert_eq!(acts(&run), [Act::Delete]);
    }
}
