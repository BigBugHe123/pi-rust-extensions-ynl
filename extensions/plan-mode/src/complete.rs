//! `plan_mode_complete` and the legacy `<proposed_plan>` block (upstream `completion-tool.ts`,
//! `message-transform.ts`).

use pi_ext_sdk::{ToolResult, ToolSpec};
use serde_json::{Value, json};

use crate::state::{COMPLETE_TOOL, validate_plan};

pub fn tool_spec() -> ToolSpec {
    ToolSpec::new(
        COMPLETE_TOOL,
        "Submit a decision-ready plan only when the latest effective Plan contract explicitly says /plan mode is active, and call it alone as the final action. Tool visibility alone does not activate Plan mode. Never call for ordinary planning requests, the writing-plans skill, roadmaps, checklists, or plan-file work.",
    )
    .label("Complete plan")
    .parameters(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "plan": {
                "type": "string",
                "description": "The complete decision-ready Markdown for read-only workflows. Omit in managed-draft workflows."
            }
        }
    }))
}

/// The `plan` argument of a read-only workflow, trimmed and validated.
pub fn plan_argument(params: &Value) -> Result<String, String> {
    let plan = params
        .get("plan")
        .and_then(Value::as_str)
        .ok_or_else(|| "plan must be a string".to_owned())?;
    validate_plan(plan.trim())
}

pub fn proposed_plan_markdown(plan: &str) -> String {
    format!("**Proposed Plan**\n\n{plan}")
}

pub fn completed(plan: &str) -> ToolResult {
    ToolResult::text(proposed_plan_markdown(plan))
        .details(json!({ "version": 1, "source": COMPLETE_TOOL, "plan": plan }))
        .terminate()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Absent,
    Valid(String),
    Empty,
    Multiple,
    Malformed,
    Unclosed,
}

const OPEN: &str = "<proposed_plan>";
const CLOSE: &str = "</proposed_plan>";

fn count_ci(text: &str, needle: &str) -> usize {
    text.to_ascii_lowercase().matches(needle).count()
}

/// A line that is exactly `tag` plus trailing spaces or tabs (and an optional `\r`).
fn is_tag_line(line: &str, tag: &str) -> bool {
    line.trim_end_matches('\r').trim_end_matches([' ', '\t']) == tag
}

/// The byte range of each line of `text`, without the `\n`.
fn lines(text: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if c == '\n' {
            result.push((start, at));
            start = at + 1;
        }
    }
    result.push((start, text.len()));
    result
}

/// One `<proposed_plan>` block whose tags are on their own lines.
pub fn parse_proposed_plan(text: &str) -> Parsed {
    let (opening, closing) = (count_ci(text, OPEN), count_ci(text, CLOSE));
    match (opening, closing) {
        (0, 0) => return Parsed::Absent,
        (o, c) if o > 1 || c > 1 => return Parsed::Multiple,
        (1, 0) => return Parsed::Unclosed,
        (1, 1) => {}
        _ => return Parsed::Malformed,
    }
    let spans = lines(text);
    let line = |i: usize| &text[spans[i].0..spans[i].1];
    let Some(open) = (0..spans.len()).find(|&i| is_tag_line(line(i), OPEN)) else {
        return Parsed::Malformed;
    };
    let Some(close) = (open + 2..spans.len()).find(|&i| is_tag_line(line(i), CLOSE)) else {
        return Parsed::Malformed;
    };
    let body = &text[spans[open + 1].0..spans[close - 1].1];
    let plan = body.trim_end_matches('\r').trim();
    if plan.is_empty() {
        Parsed::Empty
    } else {
        Parsed::Valid(plan.to_owned())
    }
}

/// Removes every well-formed `<proposed_plan>` block from `text`.
pub fn strip_proposed_plan_blocks(text: &str) -> String {
    let spans = lines(text);
    let line = |i: usize| &text[spans[i].0..spans[i].1];
    let mut result = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < spans.len() {
        if is_tag_line(line(i), OPEN)
            && let Some(close) = (i + 2..spans.len()).find(|&j| is_tag_line(line(j), CLOSE))
        {
            let end = spans[close].0 + line(close).trim_end_matches('\r').len();
            result.push_str(&text[copied..spans[i].0]);
            copied = end;
            i = close + 1;
            continue;
        }
        i += 1;
    }
    result.push_str(&text[copied..]);
    result
}

pub fn invalid_plan_message(parsed: &Parsed) -> Option<String> {
    let detail = match parsed {
        Parsed::Empty => "the block is empty",
        Parsed::Multiple => "more than one plan block was produced",
        Parsed::Malformed => "the tags must be on their own lines",
        Parsed::Unclosed => "the closing tag is missing",
        Parsed::Absent | Parsed::Valid(_) => return None,
    };
    Some(format!(
        "Proposed plan is not ready: {detail}. Continue Plan mode and produce one complete non-empty <proposed_plan> block."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_block_with_tags_on_their_own_lines() {
        let text = "Intro\n<proposed_plan>\n# Plan\n- step\n</proposed_plan>\nDone";
        assert_eq!(
            parse_proposed_plan(text),
            Parsed::Valid("# Plan\n- step".into())
        );
        assert_eq!(parse_proposed_plan("no plan"), Parsed::Absent);
        assert_eq!(parse_proposed_plan("<proposed_plan>\nx"), Parsed::Unclosed);
        assert_eq!(
            parse_proposed_plan("<proposed_plan>x</proposed_plan>"),
            Parsed::Malformed
        );
        assert_eq!(
            parse_proposed_plan("<proposed_plan>\n \n</proposed_plan>"),
            Parsed::Empty
        );
        assert_eq!(
            parse_proposed_plan(
                "<proposed_plan>\na\n</proposed_plan>\n<proposed_plan>\nb\n</proposed_plan>"
            ),
            Parsed::Multiple
        );
    }

    #[test]
    fn strips_blocks() {
        let text = "a\n<proposed_plan>\nplan\n</proposed_plan>\nb";
        assert_eq!(strip_proposed_plan_blocks(text), "a\n\nb");
        assert_eq!(strip_proposed_plan_blocks("plain"), "plain");
    }
}
