//! Lists in the panel with type-to-filter: a checklist (space checks, enter finishes) and a
//! single choice (enter chooses).

use std::collections::BTreeSet;

use pi_ext_sdk::prelude::*;

use crate::ui::frame::{self, Frame, accent, muted};
use crate::ui::{IDLE, Screen, one_line};

pub struct Item {
    pub value: String,
    pub note: String,
}

impl Item {
    pub fn new(value: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            note: note.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Inherit,
    List(Vec<String>),
}

/// A row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// Unset: what the field inherits.
    Inherit,
    /// An empty list.
    Nothing,
    Item(usize),
    /// The filter text as a value of its own.
    Typed(String),
}

struct List<'a> {
    title: &'a str,
    /// What unset means, such as "all"; `None` for a single choice.
    inherit: Option<&'a str>,
    /// What an empty list means, such as "no tools"; `None` when the list has no such row.
    nothing: Option<&'a str>,
    items: Vec<Item>,
    checked: BTreeSet<String>,
    filter: String,
    row: usize,
}

impl List<'_> {
    fn rows(&self) -> Vec<Row> {
        let needle = self.filter.to_lowercase();
        let mut rows = Vec::new();
        if self.inherit.is_some() && needle.is_empty() {
            rows.push(Row::Inherit);
            if self.nothing.is_some() {
                rows.push(Row::Nothing);
            }
        }
        rows.extend(
            (0..self.items.len())
                .filter(|&i| {
                    let item = &self.items[i];
                    item.value.to_lowercase().contains(&needle)
                        || item.note.to_lowercase().contains(&needle)
                })
                .map(Row::Item),
        );
        let text = self.filter.trim();
        if !text.is_empty() && !self.items.iter().any(|i| i.value == text) {
            rows.push(Row::Typed(text.to_owned()));
        }
        rows
    }

    fn summary(&self) -> String {
        match (self.inherit, self.checked.len()) {
            (Some(_), 0) => "0 checked".to_owned(),
            (Some(_), n) => format!("{n} checked · enter saves them"),
            (None, _) => format!("{} to choose from", self.items.len()),
        }
    }

    fn show(&self, screen: &mut Screen) {
        let width = screen.width();
        let mut frame = Frame::new(self.title)
            .summary(self.summary())
            .search(&self.filter, "type to filter");
        let esc = if self.filter.is_empty() {
            ("esc", "cancel")
        } else {
            ("esc", "clear search")
        };
        frame = match self.inherit {
            Some(_) => frame.hints(&[
                ("↑↓", "move"),
                ("space", "check"),
                ("enter", "done"),
                ("ctrl+a", "all"),
                ("type", "to filter"),
                esc,
            ]),
            None => frame.hints(&[
                ("↑↓", "move"),
                ("enter", "choose"),
                ("type", "to filter"),
                esc,
            ]),
        };
        let rows = self.rows();
        let value_width = self
            .items
            .iter()
            .map(|i| i.value.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(8, 40);
        let lines: Vec<String> = rows
            .iter()
            .map(|row| match row {
                Row::Inherit => {
                    let inherit = self.inherit.unwrap_or_default();
                    accent(&format!("↺ Use the default ({inherit})"))
                }
                Row::Nothing => {
                    let nothing = self.nothing.unwrap_or_default();
                    accent(&format!("∅ None ({nothing})"))
                }
                Row::Typed(text) => accent(&format!("+ Use \"{text}\"")),
                Row::Item(i) => {
                    let item = &self.items[*i];
                    let note = one_line(&item.note, width.saturating_sub(value_width + 12));
                    let text = format!("{:<value_width$}  {}", item.value, muted(&note));
                    match self.inherit {
                        Some(_) if self.checked.contains(&item.value) => {
                            format!("{} {text}", accent("[✓]"))
                        }
                        Some(_) => format!("{} {text}", muted("[ ]")),
                        None => text,
                    }
                }
            })
            .collect();
        let (body, focus) = if lines.is_empty() {
            (vec![muted("  Nothing matches.")], 0)
        } else {
            frame::list(&lines, self.row, frame.budget(width, screen.height()))
        };
        screen.show(&frame, body, focus);
    }
}

/// One value, or `None` when the user cancels. Typed text that matches no item can be chosen.
pub async fn pick_one(
    screen: &mut Screen,
    title: &str,
    items: Vec<Item>,
) -> Result<Option<String>> {
    let list = List {
        title,
        inherit: None,
        nothing: None,
        items,
        checked: BTreeSet::new(),
        filter: String::new(),
        row: 0,
    };
    Ok(match run(screen, list).await? {
        Some(Picked::List(mut values)) => values.pop(),
        _ => None,
    })
}

/// `None` when the user cancels. `inherit` names what unset means, such as "all", and
/// `nothing` what an empty list means, such as "no tools". Enter finishes with the checked
/// items, or with the row under the pointer when none is checked. The pointer starts on the
/// row of the current value, so enter at once changes nothing.
pub async fn pick(
    screen: &mut Screen,
    title: &str,
    mut items: Vec<Item>,
    current: Option<&[String]>,
    inherit: &str,
    nothing: Option<&str>,
) -> Result<Option<Picked>> {
    let checked: BTreeSet<String> = current.into_iter().flatten().cloned().collect();
    for value in &checked {
        if !items.iter().any(|i| &i.value == value) {
            items.push(Item::new(value.clone(), "added by you"));
        }
    }
    let mut list = List {
        title,
        inherit: Some(inherit),
        nothing,
        items,
        checked,
        filter: String::new(),
        row: 0,
    };
    let rows = list.rows();
    let start = match current {
        None => Some(Row::Inherit),
        Some([]) if nothing.is_some() => Some(Row::Nothing),
        Some(_) => None,
    };
    list.row = match start {
        Some(start) => rows.iter().position(|r| *r == start).unwrap_or(0),
        None => rows
            .iter()
            .position(|r| matches!(r, Row::Item(i) if list.checked.contains(&list.items[*i].value)))
            .unwrap_or_else(|| list.first_item()),
    };
    run(screen, list).await
}

async fn run(screen: &mut Screen, mut list: List<'_>) -> Result<Option<Picked>> {
    loop {
        list.row = list.row.min(list.rows().len().saturating_sub(1));
        list.show(screen);
        let Some(key) = screen.next(IDLE).await else {
            if screen.closed {
                return Ok(None);
            }
            continue;
        };
        if let Some(done) = list.key(&key) {
            return Ok(done);
        }
    }
}

impl List<'_> {
    /// The first row that is an item, below the default and none rows.
    fn first_item(&self) -> usize {
        self.rows()
            .iter()
            .position(|r| matches!(r, Row::Item(_)))
            .unwrap_or(0)
    }

    /// Applies a key: `Some(None)` cancels, `Some(Some(picked))` finishes, `None` goes on.
    fn key(&mut self, key: &Key) -> Option<Option<Picked>> {
        let single = self.inherit.is_none();
        let rows = self.rows();
        let target = rows.get(self.row).cloned();
        let len = rows.len().max(1);
        match key {
            Key::Up => self.row = (self.row + len - 1) % len,
            Key::Down => self.row = (self.row + 1) % len,
            Key::PageUp => self.row = self.row.saturating_sub(10),
            Key::PageDown => self.row = (self.row + 10).min(len - 1),
            Key::Escape if !self.filter.is_empty() => {
                self.filter.clear();
                self.row = self.first_item();
            }
            Key::Escape | Key::Ctrl('c') => return Some(None),
            Key::Enter if single => {
                let value = match target {
                    Some(Row::Item(i)) => Some(self.items[i].value.clone()),
                    Some(Row::Typed(text)) => Some(text),
                    _ => None,
                };
                return value.map(|v| Some(Picked::List(vec![v])));
            }
            Key::Ctrl('r') => return Some(Some(Picked::Inherit)),
            Key::Ctrl('a') => {
                let values: Vec<String> = rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Item(i) => Some(self.items[*i].value.clone()),
                        _ => None,
                    })
                    .collect();
                let all = values.iter().all(|v| self.checked.contains(v));
                for value in values {
                    if all {
                        self.checked.remove(&value);
                    } else {
                        self.checked.insert(value);
                    }
                }
            }
            Key::Char(' ') | Key::Enter if matches!(target, Some(Row::Typed(_))) => {
                let value = self.filter.trim().to_owned();
                self.items.push(Item::new(value.clone(), "added by you"));
                self.checked.insert(value);
                self.filter.clear();
                self.row = self.first_item();
            }
            // Space picks the default or none at once; enter does so only when nothing is
            // checked, so enter never drops checked items.
            Key::Char(' ') if target == Some(Row::Inherit) => return Some(Some(Picked::Inherit)),
            Key::Char(' ') if target == Some(Row::Nothing) => {
                return Some(Some(Picked::List(Vec::new())));
            }
            Key::Char(' ') => {
                if let Some(Row::Item(i)) = target {
                    let value = self.items[i].value.clone();
                    if !self.checked.remove(&value) {
                        self.checked.insert(value);
                    }
                }
            }
            Key::Enter if self.checked.is_empty() => {
                return Some(Some(match target {
                    Some(Row::Inherit) => Picked::Inherit,
                    Some(Row::Item(i)) => Picked::List(vec![self.items[i].value.clone()]),
                    _ => Picked::List(Vec::new()),
                }));
            }
            Key::Enter => {
                let picked = self
                    .items
                    .iter()
                    .filter(|i| self.checked.contains(&i.value))
                    .map(|i| i.value.clone());
                return Some(Some(Picked::List(picked.collect())));
            }
            Key::Backspace => {
                self.filter.pop();
                self.row = self.first_item();
            }
            Key::Char(c) => {
                self.filter.push(*c);
                self.row = self.first_item();
            }
            Key::Text(text) => {
                self.filter.push_str(text);
                self.row = self.first_item();
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(filter: &str) -> List<'static> {
        List {
            title: "Tools",
            inherit: Some("pi defaults"),
            nothing: Some("no tools"),
            items: vec![
                Item::new("read", "Read files"),
                Item::new("bash", "Run commands"),
            ],
            checked: BTreeSet::new(),
            filter: filter.into(),
            row: 0,
        }
    }

    #[test]
    fn rows_offer_the_default_none_and_typed_values() {
        assert_eq!(
            list("").rows(),
            [Row::Inherit, Row::Nothing, Row::Item(0), Row::Item(1)]
        );
        assert_eq!(list("read").rows(), [Row::Item(0)]);
        assert_eq!(
            list("files").rows(),
            [Row::Item(0), Row::Typed("files".into())],
            "notes match too, and the text stays a value of its own"
        );
        assert_eq!(list("mcp__*").rows(), [Row::Typed("mcp__*".into())]);
    }

    fn keys(list: &mut List<'_>, keys: &[Key]) -> Option<Option<Picked>> {
        keys.iter().find_map(|key| list.key(key))
    }

    fn text(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
    }

    #[test]
    fn enter_keeps_checked_items_on_every_row() {
        let mut checked = list("");
        checked.checked.insert("read".into());
        let done = keys(&mut checked, &[Key::Enter]);
        assert_eq!(
            done,
            Some(Some(Picked::List(vec!["read".into()]))),
            "on the default row"
        );

        let mut typed = list("");
        let mut flow = text("mcp__*");
        flow.extend([Key::Enter, Key::Enter]);
        let done = keys(&mut typed, &flow);
        assert_eq!(
            done,
            Some(Some(Picked::List(vec!["mcp__*".into()]))),
            "a typed value"
        );

        let mut filtered = list("");
        let mut flow = text("ba");
        flow.extend([Key::Char(' '), Key::Backspace, Key::Backspace, Key::Enter]);
        let done = keys(&mut filtered, &flow);
        assert_eq!(
            done,
            Some(Some(Picked::List(vec!["bash".into()]))),
            "after the filter"
        );
    }

    #[test]
    fn the_default_and_none_rows_apply_when_chosen() {
        let mut nothing_checked = list("");
        assert_eq!(
            keys(&mut nothing_checked, &[Key::Enter]),
            Some(Some(Picked::Inherit))
        );
        let mut none = list("");
        assert_eq!(
            keys(&mut none, &[Key::Down, Key::Enter]),
            Some(Some(Picked::List(Vec::new())))
        );
        let mut space = list("");
        space.checked.insert("read".into());
        assert_eq!(
            keys(&mut space, &[Key::Char(' ')]),
            Some(Some(Picked::Inherit))
        );
        let mut cancel = list("x");
        assert_eq!(
            keys(&mut cancel, &[Key::Escape]),
            None,
            "esc clears the filter first"
        );
        assert_eq!(keys(&mut cancel, &[Key::Escape]), Some(None));
    }
}
