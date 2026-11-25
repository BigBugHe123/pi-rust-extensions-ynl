//! Call and result rows of `plan_mode_question`: a compact answer summary instead of the JSON
//! payload (upstream `question-renderer.ts`).

use pi_ext_sdk::render::{ToolCallRender, ToolResultRender};
use pi_ext_sdk::style::{self, Color};
use serde_json::Value;

use crate::ui::safe_text;

fn fit(line: String, width: usize) -> String {
    style::truncate(&line, width)
}

pub fn call(req: &ToolCallRender) -> Option<Vec<String>> {
    let questions = req.args.get("questions").and_then(Value::as_array);
    let count = questions.map_or(0, Vec::len);
    let title = if count > 1 {
        "Plan questions"
    } else {
        "Plan question"
    };
    let headers: Vec<String> = questions
        .into_iter()
        .flatten()
        .filter_map(|q| q.get("header").and_then(Value::as_str))
        .map(safe_text)
        .filter(|h| !h.is_empty())
        .collect();
    let mut line = style::bold(title);
    if !headers.is_empty() {
        line.push_str(&style::dim(&format!("  {}", headers.join(" · "))));
    }
    Some(vec![fit(line, req.width)])
}

/// The payload: the JSON text merged with `details`.
fn payload(req: &ToolResultRender) -> Value {
    let mut payload: Value = serde_json::from_str(&req.text()).unwrap_or(Value::Null);
    if let Some(details) = req.result.get("details")
        && details.get("cancelled").is_some_and(Value::is_boolean)
        && let (Some(target), Some(source)) = (payload.as_object_mut(), details.as_object())
    {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    payload
}

pub fn result(req: &ToolResultRender) -> Option<Vec<String>> {
    let width = req.width;
    if req.options.is_partial {
        return Some(vec![fit(style::dim("Waiting for answers…"), width)]);
    }
    let payload = payload(req);
    let error = req.context.is_error || req.result.get("isError") == Some(&Value::Bool(true));
    if error || payload.get("cancelled") == Some(&Value::Bool(true)) {
        let reason = payload.get("reason").and_then(Value::as_str);
        let status = match reason {
            _ if error => "Question failed",
            Some("ui_unavailable") => "Interactive UI unavailable",
            Some("plan_mode_inactive") => "Plan mode inactive",
            Some("invalid_input") => "Invalid question",
            Some("discussion_requested") => "Switched to free discussion",
            Some("questions_paused") => "Questionnaires paused",
            _ => "Question cancelled",
        };
        let color = if error || reason == Some("invalid_input") {
            Color::Red
        } else {
            Color::Yellow
        };
        let mut lines = vec![fit(style::fg(color, status), width)];
        if req.options.expanded {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| req.text(), str::to_owned);
            lines.extend(
                message
                    .lines()
                    .map(|l| fit(style::dim(&format!("  {}", safe_text(l))), width)),
            );
        }
        return Some(lines);
    }
    let answers = payload.get("answers").and_then(Value::as_array)?;
    let mut lines = Vec::new();
    for answer in answers {
        let field = |key: &str| answer.get(key).and_then(Value::as_str).map(safe_text);
        let header = field("header").unwrap_or_default();
        let text = field("answer").unwrap_or_default();
        let custom = answer.get("wasCustom") == Some(&Value::Bool(true));
        let note = field("note");
        if !req.options.expanded {
            let mut line = format!(
                "{}{}{}",
                style::fg(Color::Green, "✓ "),
                style::dim(&format!("{header}: ")),
                style::bold(&text)
            );
            if custom {
                line.push_str(&style::dim(" (custom)"));
            }
            if note.is_some() {
                line.push_str(&style::dim(" · note"));
            }
            lines.push(fit(line, width));
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(fit(style::bold(&header), width));
        if let Some(question) = field("question") {
            lines.push(fit(style::dim(&format!("  {question}")), width));
        }
        if custom {
            lines.push(fit(style::dim("  Custom answer"), width));
        }
        lines.push(fit(
            format!("  {}{text}", style::fg(Color::Green, "✓ ")),
            width,
        ));
        if let Some(note) = note {
            lines.push(fit(style::dim("  Note"), width));
            lines.push(fit(style::dim(&format!("    {note}")), width));
        }
    }
    Some(lines)
}
