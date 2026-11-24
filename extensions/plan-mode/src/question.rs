//! `plan_mode_question`: one to three structured questions, shown with the ask-user-question
//! panel in `tui` mode and its dialog walker in `rpc` mode (upstream `question-tool.ts`).

use pi_ext_ask_user_question::config::Config;
use pi_ext_ask_user_question::fallback;
use pi_ext_ask_user_question::render::render;
use pi_ext_ask_user_question::result::{AnswerKind, QuestionnaireResult};
use pi_ext_ask_user_question::schema::{Question, QuestionOption};
use pi_ext_ask_user_question::state::{EditTarget, Effect, Questionnaire};
use pi_ext_sdk::{CancellationToken, Host, PanelEvent, Result, ToolResult, ToolSpec};
use serde::Serialize;
use serde_json::{Value, json};

pub const QUESTION_TOOL: &str = "plan_mode_question";
const PANEL_WIDTH: usize = 80;
const F2_INPUTS: [&str; 6] = [
    "\x1bOQ",
    "\x1b[12~",
    "\x1b[[B",
    "\x1b[1;1Q",
    "\x1b[12;1~",
    "\x1b[-22;1u",
];

pub fn tool_spec() -> ToolSpec {
    ToolSpec::new(
        QUESTION_TOOL,
        "Ask one to three structured questions only when the latest effective Plan contract explicitly says /plan mode is active. Tool visibility alone does not activate Plan mode. Never call for ordinary planning requests, the writing-plans skill, roadmaps, checklists, or plan-file work.",
    )
    .label("Plan question")
    .parameters(json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["questions"],
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 3,
                "description": "Questions to show the user. Prefer 1 and do not exceed 3.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "header", "question", "options"],
                    "properties": {
                        "id": { "type": "string", "description": "Stable identifier for mapping answers (snake_case)." },
                        "header": { "type": "string", "description": "Short header label shown in the UI (12 or fewer chars)." },
                        "question": { "type": "string", "description": "Single-sentence prompt shown to the user." },
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "description": "Provide 2-4 mutually exclusive choices. Put the recommended option first when there is a clear default.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["label", "description"],
                                "properties": {
                                    "label": { "type": "string", "description": "Plain-text label (1-5 words) with no list numbers or bullets. The questionnaire adds the numbers." },
                                    "description": { "type": "string", "description": "One short sentence explaining impact/tradeoff if selected." }
                                }
                            }
                        }
                    }
                }
            }
        }
    }))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<PlanOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanAnswer {
    pub id: String,
    pub header: String,
    pub question: String,
    pub answer: String,
    pub was_custom: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Validates the arguments like upstream `normalizePlanModeQuestionParams`.
pub fn normalize(params: &Value) -> std::result::Result<Vec<PlanQuestion>, String> {
    let items = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or("questions must be an array")?;
    if !(1..=3).contains(&items.len()) {
        return Err("questions must contain 1-3 items".into());
    }
    let mut questions = Vec::new();
    for (qi, raw) in items.iter().enumerate() {
        let n = qi + 1;
        if !raw.is_object() {
            return Err(format!("question {n} must be an object"));
        }
        let (Some(id), Some(header), Some(question)) = (
            field(raw, "id"),
            field(raw, "header"),
            field(raw, "question"),
        ) else {
            return Err(format!(
                "question {n} requires non-empty id, header, and question"
            ));
        };
        let options = raw
            .get("options")
            .and_then(Value::as_array)
            .ok_or(format!("question {n} options must be an array"))?;
        if !(2..=4).contains(&options.len()) {
            return Err(format!("question {n} options must contain 2-4 items"));
        }
        let mut parsed = Vec::new();
        for (oi, option) in options.iter().enumerate() {
            let m = oi + 1;
            if !option.is_object() {
                return Err(format!("question {n} option {m} must be an object"));
            }
            let label = field(option, "label")
                .ok_or(format!("question {n} option {m} requires a label"))?;
            let description = field(option, "description")
                .ok_or(format!("question {n} option {m} requires a description"))?;
            parsed.push(PlanOption { label, description });
        }
        questions.push(PlanQuestion {
            id,
            header,
            question,
            options: parsed,
        });
    }
    Ok(questions)
}

fn payload_text(payload: Value) -> String {
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

pub fn answered(questions: &[PlanQuestion], answers: &[PlanAnswer]) -> ToolResult {
    ToolResult::text(payload_text(
        json!({ "cancelled": false, "answers": answers }),
    ))
    .details(json!({ "cancelled": false, "questions": questions, "answers": answers }))
}

pub fn cancelled(questions: &[PlanQuestion], reason: &str, message: &str) -> ToolResult {
    ToolResult::text(payload_text(
        json!({ "cancelled": true, "reason": reason, "message": message }),
    ))
    .details(json!({ "cancelled": true, "reason": reason, "questions": questions }))
}

/// Labels without a `1.`/`1)` prefix when every option has one in sequence; the panel adds numbers.
fn display_labels(options: &[PlanOption]) -> Vec<String> {
    let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
    let mut marker = None;
    let mut stripped = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let text = label.trim_start();
        let number = (index + 1).to_string();
        let Some(rest) = text.strip_prefix(&number) else {
            return labels;
        };
        let mut chars = rest.chars();
        let Some(mark) = chars.next().filter(|c| *c == '.' || *c == ')') else {
            return labels;
        };
        if marker.is_some_and(|m| m != mark) {
            return labels;
        }
        marker = Some(mark);
        let after = chars.as_str();
        if !after.starts_with(char::is_whitespace) || after.trim().is_empty() {
            return labels;
        }
        stripped.push(after.trim_start().to_owned());
    }
    stripped
}

fn questionnaire_questions(questions: &[PlanQuestion]) -> Vec<Question> {
    questions
        .iter()
        .map(|q| Question {
            question: q.question.clone(),
            header: q.header.clone(),
            options: display_labels(&q.options)
                .into_iter()
                .zip(&q.options)
                .map(|(label, option)| QuestionOption {
                    label,
                    description: option.description.clone(),
                    preview: None,
                })
                .collect(),
            multi_select: None,
        })
        .collect()
}

/// Maps a questionnaire result to upstream answers; `None` for a cancelled or incomplete result.
fn map_result(
    questions: &[PlanQuestion],
    shown: &[Question],
    result: &QuestionnaireResult,
) -> Option<Vec<PlanAnswer>> {
    if result.cancelled {
        return None;
    }
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let answer = result.answers.iter().find(|a| a.question_index == index)?;
            let text = answer.answer.clone().unwrap_or_default();
            let (answer_text, was_custom, option_index) = match answer.kind {
                AnswerKind::Option => {
                    let position = shown[index].options.iter().position(|o| o.label == text)?;
                    (
                        question.options[position].label.clone(),
                        false,
                        Some(position + 1),
                    )
                }
                AnswerKind::Custom | AnswerKind::Multi => {
                    if text.trim().is_empty() {
                        return None;
                    }
                    (text, true, None)
                }
            };
            Some(PlanAnswer {
                id: question.id.clone(),
                header: question.header.clone(),
                question: question.question.clone(),
                answer: answer_text,
                was_custom,
                option_index,
                note: answer.notes.clone().filter(|n| !n.is_empty()),
            })
        })
        .collect()
}

/// How a questionnaire ended.
pub enum Asked {
    Answered(Vec<PlanAnswer>),
    Cancelled,
    /// The user pressed F2 to switch to free discussion.
    Discussion,
}

fn is_f2(panel: &pi_ext_sdk::Panel) -> bool {
    panel.last_input().is_some_and(|input| {
        input.id.as_deref() == Some("f2") || F2_INPUTS.contains(&input.data.as_str())
    })
}

/// Shows the questions. `f2` lets the F2 key end the panel with [`Asked::Discussion`].
pub async fn ask(
    host: &Host,
    cancel: &CancellationToken,
    questions: &[PlanQuestion],
    rpc: bool,
    f2: bool,
) -> Result<Asked> {
    let shown = questionnaire_questions(questions);
    let result = if rpc {
        fallback::walk(host, &shown).await?
    } else {
        match panel(host, cancel, &shown, f2).await? {
            PanelOutcome::Done(result) => result,
            PanelOutcome::Discussion => return Ok(Asked::Discussion),
            PanelOutcome::NoPanel => fallback::walk(host, &shown).await?,
        }
    };
    Ok(match map_result(questions, &shown, &result) {
        Some(answers) => Asked::Answered(answers),
        None => Asked::Cancelled,
    })
}

enum PanelOutcome {
    Done(QuestionnaireResult),
    Discussion,
    NoPanel,
}

async fn panel(
    host: &Host,
    cancel: &CancellationToken,
    questions: &[Question],
    f2: bool,
) -> Result<PanelOutcome> {
    let config = Config::load(&host.agent_dir(), &host.cwd());
    let mut state =
        Questionnaire::new(questions.to_vec()).with_keys(&config.collapse_key, &config.notes_key);
    let mut panel = match host.panel(render(&state, PANEL_WIDTH)).open().await {
        Ok(panel) => panel,
        Err(err) if err.is_no_ui() => return Ok(PanelOutcome::NoPanel),
        Err(err) => return Err(err),
    };
    let mut width = panel.width().unwrap_or(PANEL_WIDTH);
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => {
                drop(panel);
                return Err(pi_ext_sdk::Error::cancelled());
            }
            event = panel.next_event() => event,
        };
        let effect = match &event {
            PanelEvent::Resize(w) => {
                width = *w;
                None
            }
            PanelEvent::Key(_) if f2 && is_f2(&panel) => {
                panel.close(Value::Null).await?;
                return Ok(PanelOutcome::Discussion);
            }
            PanelEvent::Key(key) => state.handle_key(key),
            PanelEvent::Closed => Some(state.done(true)),
        };
        match effect {
            Some(Effect::Done(result)) => {
                if !matches!(event, PanelEvent::Closed) {
                    panel.close(Value::Null).await?;
                }
                return Ok(PanelOutcome::Done(result));
            }
            Some(Effect::Edit(target, draft)) => {
                let title = match target {
                    EditTarget::Input => "Edit answer",
                    EditTarget::Notes => "Edit note",
                };
                match host.editor(title, Some(&draft)).await {
                    Ok(text) => state.apply_edit(target, text),
                    Err(err) => {
                        let message = format!("External editor failed: {}", err.message());
                        let _ = host.notify_error(message).await;
                    }
                }
            }
            None => {}
        }
        panel.set_lines(render(&state, width))?;
    }
}

#[cfg(test)]
mod tests {
    use pi_ext_ask_user_question::result::QuestionAnswer;

    use super::*;

    fn params() -> Value {
        json!({ "questions": [{
            "id": "db", "header": "Database", "question": "Which database?",
            "options": [
                { "label": "1. Postgres", "description": "Relational" },
                { "label": "2. SQLite", "description": "Embedded" }
            ]
        }]})
    }

    #[test]
    fn validates_like_upstream() {
        assert!(normalize(&params()).is_ok());
        assert_eq!(
            normalize(&json!({ "questions": [] })).unwrap_err(),
            "questions must contain 1-3 items"
        );
        let missing = json!({ "questions": [{ "id": "a", "header": "h", "question": "q",
            "options": [{ "label": "x" }, { "label": "y", "description": "d" }] }] });
        assert_eq!(
            normalize(&missing).unwrap_err(),
            "question 1 option 1 requires a description"
        );
    }

    #[test]
    fn strips_numbers_for_display_and_maps_answers_back() {
        let questions = normalize(&params()).unwrap();
        let shown = questionnaire_questions(&questions);
        assert_eq!(shown[0].options[1].label, "SQLite");
        let mut answer = QuestionAnswer::new(
            0,
            "Which database?",
            AnswerKind::Option,
            Some("SQLite".into()),
        );
        answer.notes = Some("small app".into());
        let result = QuestionnaireResult {
            answers: vec![answer],
            ..QuestionnaireResult::default()
        };
        let answers = map_result(&questions, &shown, &result).unwrap();
        assert_eq!(answers[0].answer, "2. SQLite");
        assert_eq!(answers[0].option_index, Some(2));
        assert_eq!(answers[0].note.as_deref(), Some("small app"));
        assert!(map_result(&questions, &shown, &QuestionnaireResult::cancelled()).is_none());
    }
}
