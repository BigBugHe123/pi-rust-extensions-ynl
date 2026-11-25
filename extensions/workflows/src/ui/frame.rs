//! The lines around every screen of `/workflows`, laid out like `/hooks`, `/deferred`, and
//! `/claude-plugins`: rule, title, tabs, search, body, status, hints, rule.

use pi_ext_sdk::style::{self, Color};

use crate::runs::now_ms;
use crate::ui::wrap;

pub fn accent(text: &str) -> String {
    style::fg(Color::Cyan, text)
}

pub fn muted(text: &str) -> String {
    style::fg(Color::Gray, text)
}

pub fn warning(text: &str) -> String {
    style::fg(Color::Yellow, text)
}

pub fn good(text: &str) -> String {
    style::fg(Color::Green, text)
}

pub fn bad(text: &str) -> String {
    style::fg(Color::Red, text)
}

/// The pointer in front of the selected row, or the indent of the other rows.
pub fn pointer(selected: bool) -> String {
    if selected {
        accent("→ ")
    } else {
        "  ".to_owned()
    }
}

/// A selected choice of an actions list.
pub fn choice(label: &str, selected: bool, risky: bool) -> String {
    match (selected, risky) {
        (true, _) => format!("{}{}", accent(" → "), accent(&style::bold(label))),
        (false, true) => format!("   {}", warning(label)),
        (false, false) => format!("   {label}"),
    }
}

/// `label` in a muted column of 13, then `value` wrapped under itself.
pub fn field(lines: &mut Vec<String>, label: &str, value: &str, width: usize) {
    for (i, line) in wrap(value, width.saturating_sub(16))
        .into_iter()
        .enumerate()
    {
        let head = if i == 0 { label } else { "" };
        lines.push(format!(" {} {line}", muted(&format!("{head:<13}"))));
    }
}

/// The tab line: `[Active (2)]  Other (5)`.
pub fn tabs(labels: &[String], active: usize) -> String {
    let tabs: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if i == active {
                accent(&style::bold(&format!("[{label}]")))
            } else {
                muted(label)
            }
        })
        .collect();
    format!(" {}", tabs.join("  "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Warn,
    Error,
    /// Work in progress, with a spinner.
    Busy,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Rows pi keeps outside a panel: the footer lines, and the top row in fullscreen.
pub const HOST_ROWS: usize = 3;
/// The body keeps this many lines before the chrome around it shrinks.
const MIN_BODY: usize = 5;

/// What a line is, so a short panel drops chrome before body lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Gap,
    Search,
    Rule,
    Title,
    Tabs,
    Status,
    Hints,
    Body,
}

/// Parts in the order a short panel drops them.
const DROP: [Part; 7] = [
    Part::Gap,
    Part::Search,
    Part::Rule,
    Part::Title,
    Part::Tabs,
    Part::Status,
    Part::Hints,
];

#[derive(Default)]
pub struct Frame {
    /// The bold title; `summary` follows it, muted.
    pub title: String,
    pub summary: String,
    pub tabs: Option<String>,
    /// `Some` shows the search line with this query.
    pub search: Option<String>,
    pub search_hint: String,
    pub status: Option<(Tone, String)>,
    pub hints: Vec<String>,
}

impl Frame {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            search_hint: "type to search".into(),
            ..Self::default()
        }
    }

    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn tabs(mut self, tabs: String) -> Self {
        self.tabs = Some(tabs);
        self
    }

    pub fn search(mut self, query: &str, hint: impl Into<String>) -> Self {
        self.search = Some(query.to_owned());
        self.search_hint = hint.into();
        self
    }

    pub fn status(mut self, status: Option<(Tone, String)>) -> Self {
        self.status = status;
        self
    }

    /// Hints such as `("enter", "open")`.
    pub fn hints(mut self, hints: &[(&str, &str)]) -> Self {
        self.hints = hints.iter().map(|(k, a)| format!("{k} {a}")).collect();
        self
    }

    fn status_lines(&self, width: usize) -> Vec<String> {
        let Some((tone, text)) = &self.status else {
            return Vec::new();
        };
        let mut lines = wrap(text, width.saturating_sub(4));
        if lines.len() > 6 {
            let more = lines.len() - 5;
            lines.truncate(5);
            lines.push(format!("… {more} more lines"));
        }
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| match tone {
                Tone::Busy if i == 0 => {
                    let frame = SPINNER[(now_ms() / 100) as usize % SPINNER.len()];
                    accent(&format!(" {frame} {line}"))
                }
                Tone::Busy => accent(&format!("   {line}")),
                Tone::Info => muted(&format!(" {line}")),
                Tone::Good => good(&format!(" {line}")),
                Tone::Warn => warning(&format!(" {line}")),
                Tone::Error => bad(&format!(" {line}")),
            })
            .collect()
    }

    /// Joins hints with ` · ` and starts a new line before one would overflow `width`.
    fn hint_lines(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for hint in &self.hints {
            let next = if line.is_empty() {
                format!(" {hint}")
            } else {
                format!("{line} · {hint}")
            };
            if style::visible_width(&next) > width && !line.is_empty() {
                lines.push(muted(&line));
                line = format!(" {hint}");
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            lines.push(muted(&line));
        }
        lines
    }

    fn chrome(&self, width: usize) -> usize {
        // Two rules, the title, and the gaps before and after the body.
        let mut lines = 5 + self.status_lines(width).len() + self.hint_lines(width).len();
        lines += usize::from(self.tabs.is_some()) + usize::from(self.search.is_some());
        lines
    }

    /// The body lines that fit in `height` rows.
    pub fn budget(&self, width: usize, height: usize) -> usize {
        let small = MIN_BODY.min(height.saturating_sub(1));
        height.saturating_sub(self.chrome(width)).max(small).max(1)
    }

    /// The panel lines. A body taller than the budget keeps the lines around `focus`.
    pub fn render(
        &self,
        body: Vec<String>,
        focus: usize,
        width: usize,
        height: usize,
    ) -> Vec<String> {
        let width = width.max(20);
        let rule = muted(&"─".repeat(width));
        let mut title = format!(" {}", accent(&style::bold(&self.title)));
        if !self.summary.is_empty() {
            title.push_str(&muted(&format!("  {}", self.summary)));
        }
        let mut parts = vec![(Part::Rule, rule.clone()), (Part::Title, title)];
        if let Some(tabs) = &self.tabs {
            parts.push((Part::Tabs, tabs.clone()));
        }
        if let Some(query) = &self.search {
            let line = if query.is_empty() {
                muted(&format!(" › {}", self.search_hint))
            } else {
                format!(" › {query}{}", accent("▏"))
            };
            parts.push((Part::Search, line));
        }
        parts.push((Part::Gap, String::new()));
        let body = fit(body, self.budget(width, height), focus);
        parts.extend(body.into_iter().map(|line| (Part::Body, line)));
        parts.push((Part::Gap, String::new()));
        parts.extend(
            self.status_lines(width)
                .into_iter()
                .map(|line| (Part::Status, line)),
        );
        parts.extend(
            self.hint_lines(width)
                .into_iter()
                .map(|line| (Part::Hints, line)),
        );
        parts.push((Part::Rule, rule));
        for part in DROP {
            while parts.len() > height
                && let Some(at) = parts.iter().rposition(|(p, _)| *p == part)
            {
                parts.remove(at);
            }
        }
        parts.truncate(height.max(1));
        parts
            .into_iter()
            .map(|(_, line)| style::truncate(&line, width))
            .collect()
    }
}

/// Cuts `body` to `budget` lines around `focus`.
fn fit(body: Vec<String>, budget: usize, focus: usize) -> Vec<String> {
    if body.len() <= budget {
        return body;
    }
    let focus = focus.min(body.len() - 1);
    let start = focus.saturating_sub(budget / 2).min(body.len() - budget);
    body[start..start + budget].to_vec()
}

/// A list window: the rows around `selected` with the pointer, and `(i/n)` when they do not
/// all fit. Returns the lines and the line of the selected row.
pub fn list(rows: &[String], selected: usize, budget: usize) -> (Vec<String>, usize) {
    if rows.is_empty() {
        return (Vec::new(), 0);
    }
    let selected = selected.min(rows.len() - 1);
    let more = rows.len() > budget;
    let visible = if more {
        budget.saturating_sub(1).max(1)
    } else {
        rows.len()
    };
    let start = selected
        .saturating_sub(visible / 2)
        .min(rows.len() - visible);
    let mut lines: Vec<String> = rows[start..start + visible]
        .iter()
        .enumerate()
        .map(|(i, row)| format!("{}{row}", pointer(start + i == selected)))
        .collect();
    if more {
        lines.push(muted(&format!("  ({}/{})", selected + 1, rows.len())));
    }
    (lines, selected - start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| style::strip(l)).collect()
    }

    #[test]
    fn frame_lays_out_chrome_around_the_body() {
        let frame = Frame::new("Workflows")
            .summary("2 saved")
            .tabs(tabs(&["Workflows (2)".into(), "Runs (1)".into()], 0))
            .search("", "type to search")
            .hints(&[("enter", "open"), ("esc", "close")]);
        let lines = plain(&frame.render(vec!["→ a".into(), "  b".into()], 0, 40, 30));
        assert_eq!(lines[1], " Workflows  2 saved");
        assert_eq!(lines[2], " [Workflows (2)]  Runs (1)");
        assert_eq!(lines[3], " › type to search");
        assert_eq!(&lines[5..7], ["→ a", "  b"]);
        assert_eq!(lines[8], " enter open · esc close");
        assert!(lines[0].starts_with('─') && lines[9].starts_with('─'));
    }

    #[test]
    fn short_panels_drop_chrome_and_keep_the_focus() {
        let frame = Frame::new("T")
            .search("x", "type")
            .hints(&[("esc", "close")]);
        let body: Vec<String> = (0..50).map(|i| format!("row {i}")).collect();
        let lines = plain(&frame.render(body, 40, 60, 8));
        assert_eq!(lines.len(), 8);
        assert!(lines.iter().any(|l| l == "row 40"), "{lines:?}");
        assert!(lines.iter().any(|l| l == " esc close"), "{lines:?}");
    }

    #[test]
    fn lists_show_the_pointer_and_the_position() {
        let rows: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
        let (lines, focus) = list(&rows, 7, 4);
        assert_eq!(plain(&lines), ["  r6", "→ r7", "  r8", "  (8/10)"]);
        assert_eq!(focus, 1);
        let (lines, _) = list(&rows[..2], 0, 4);
        assert_eq!(plain(&lines), ["→ r0", "  r1"]);
    }
}
