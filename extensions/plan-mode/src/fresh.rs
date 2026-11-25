//! "Start fresh and implement": a new linked session that receives only the approved plan
//! (upstream `fresh-implementation.ts`).
//!
//! pi stops this process when the session is replaced, so the handoff goes through a file: the
//! source writes `handoff/<pi pid>.json` and calls `ctx.newSession`, and the extension process of
//! the new session reads the file at `session_start` (reason `new`) and sends the kickoff.

use std::path::{Path, PathBuf};

use pi_ext_sdk::{Error, Host, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{App, model_auth};
use crate::contracts::{implementation_handoff, transferred_plan_prompt};
use crate::state::{ActiveImplementation, LEGACY_SOURCE, ModelRef};
use crate::ui;

/// A handoff older than this belongs to a session replacement that did not happen.
const MAX_AGE_MS: u64 = 120_000;

/// The model and thinking level the new session uses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Handoff {
    version: u8,
    created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session: Option<String>,
    plan: String,
    source: String,
    retention: String,
    runtime: Runtime,
}

/// A model pi can use now, from the `--models` scope when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailableModel {
    pub model: ModelRef,
    pub name: Option<String>,
    pub search: String,
}

pub async fn available_models(host: &Host) -> Vec<AvailableModel> {
    let available = host
        .pi_call("ctx.modelRegistry", "getAvailable", Vec::new())
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let scoped = host
        .snapshot()
        .map(|s| s.scoped_models.clone())
        .unwrap_or_default();
    let mut models: Vec<AvailableModel> = available
        .iter()
        .filter_map(|m| {
            let provider = m.get("provider")?.as_str()?.to_owned();
            let id = m.get("id")?.as_str()?.to_owned();
            let name = m.get("name").and_then(Value::as_str).map(str::to_owned);
            let search = format!("{provider} {id} {}", name.as_deref().unwrap_or(""));
            Some(AvailableModel {
                model: ModelRef {
                    provider,
                    model_id: id,
                },
                name,
                search,
            })
        })
        .filter(|m| {
            scoped.is_empty()
                || scoped
                    .iter()
                    .any(|s| s.provider == m.model.provider && s.id == m.model.model_id)
        })
        .collect();
    let current = host.snapshot().and_then(|s| s.model.clone());
    models.sort_by_key(|m| {
        !current
            .as_ref()
            .is_some_and(|c| c.provider == m.model.provider && c.id == m.model.model_id)
    });
    models
}

/// Whether pi runs this code inside a command, where `ctx.newSession` exists. Waits for idle.
pub async fn command_context(host: &Host) -> bool {
    match host.pi_call("ctx", "waitForIdle", Vec::new()).await {
        Ok(_) => true,
        Err(err) => err.code() != pi_ext_sdk::code::METHOD_NOT_FOUND,
    }
}

#[cfg(unix)]
fn pi_pid() -> u32 {
    std::os::unix::process::parent_id()
}

#[cfg(not(unix))]
fn pi_pid() -> u32 {
    0
}

fn handoff_path(agent_dir: &Path) -> PathBuf {
    pi_ext_sdk::paths::extension_dir(agent_dir, "plan-mode")
        .join("handoff")
        .join(format!("{}.json", pi_pid()))
}

fn current_model(host: &Host) -> Option<ModelRef> {
    let model = host.snapshot()?.model.clone()?;
    Some(ModelRef {
        provider: model.provider,
        model_id: model.id,
    })
}

/// Checks the model and auth, writes the handoff, and replaces the session.
pub async fn start(app: &App, host: &Host, runtime: Runtime) -> Result<()> {
    let mode = host.snapshot().and_then(|s| s.mode.clone());
    if matches!(mode.as_deref(), Some("print" | "json")) {
        return Err(Error::msg(
            "Fresh plan implementation is unavailable in print/JSON mode. Use TUI or RPC.",
        ));
    }
    if !command_context(host).await {
        app.warn(host, "Fresh implementation requires the interactive /plan command. Reopen /plan and try again.")
            .await;
        return Ok(());
    }
    let state = app.state();
    let stored = match (state.ready_plan(), &state.saved_plan) {
        (Some(plan), _) => Some((plan.to_owned(), state.latest_plan_source.clone())),
        (None, Some(saved)) if !state.enabled => {
            Some((saved.plan.clone(), Some(saved.source.clone())))
        }
        _ => None,
    };
    let Some((plan, source)) = stored else {
        app.warn(host, "No completed plan is available to implement.")
            .await;
        return Ok(());
    };
    let Some(model) = runtime.model.clone().or_else(|| current_model(host)) else {
        app.warn(host, "Unable to implement the plan: no model is selected.")
            .await;
        return Ok(());
    };
    if !model.valid() {
        app.warn(host, "Unable to implement the plan: implementation model provider and ID must each contain 1-512 characters. Choose another model and retry.").await;
        return Ok(());
    }
    if let Err(error) = model_auth(host, &model.provider, &model.model_id).await {
        let message = format!(
            "Unable to implement the plan with {}/{}: {error}. Choose another model or configure authentication, then retry.",
            ui::safe_text(&model.provider),
            ui::safe_text(&model.model_id)
        );
        app.warn(host, message).await;
        return Ok(());
    }
    let parent_session = host.snapshot().and_then(|s| s.session_file.clone());
    let handoff = Handoff {
        version: 1,
        created_at: crate::ids::now_ms(),
        parent_session: parent_session.clone(),
        plan,
        source: source.unwrap_or_else(|| LEGACY_SOURCE.into()),
        retention: app.settings().retention().to_owned(),
        runtime,
    };
    let path = handoff_path(&app.agent_dir);
    if let Err(err) = write_handoff(&path, &handoff) {
        let _ = host
            .notify_error(format!("Unable to start a fresh implementation session: {err}. The source plan remains available."))
            .await;
        return Ok(());
    }
    if mode.as_deref() == Some("rpc") {
        app.notify(host, "Starting fresh implementation session…")
            .await;
    }
    let options = match &parent_session {
        Some(parent) => json!({ "parentSession": parent }),
        None => json!({}),
    };
    match host.pi_call("ctx", "newSession", vec![options]).await {
        Ok(result) if result.get("cancelled") == Some(&Value::Bool(true)) => {
            let _ = std::fs::remove_file(&path);
            app.notify(
                host,
                "Fresh implementation cancelled. The plan remains available.",
            )
            .await;
        }
        Ok(_) => {}
        // pi ends this session's extensions while it replaces the session, which can fail the
        // call after `session_shutdown`; the new session still needs the handoff then.
        Err(_) if app.is_shutting_down() => {}
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            let message = format!(
                "Unable to start a fresh implementation session: {}. The source plan remains available; retry or resume the planning session.",
                ui::safe_text(err.message())
            );
            let _ = host.notify_error(message).await;
        }
    }
    Ok(())
}

/// Writes the handoff in private directories (the draft check needs the package directory
/// private) and a file only the user can read.
fn write_handoff(path: &Path, handoff: &Handoff) -> std::result::Result<(), String> {
    let handoff_dir = path.parent().ok_or("handoff path has no parent")?;
    let package = handoff_dir.parent().ok_or("handoff path has no parent")?;
    if let Some(data) = package.parent() {
        std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
    }
    for dir in [package, handoff_dir] {
        if let Err(err) = crate::draft::create_private_dir(dir)
            && err.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(err.to_string());
        }
    }
    let text = serde_json::to_string(handoff).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|e| e.to_string())
}

/// Reads and removes a fresh handoff for this new session, when one is waiting.
async fn claim(app: &App, host: &Host) -> Option<Handoff> {
    let path = handoff_path(&app.agent_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    let handoff: Handoff = serde_json::from_str(&text).ok()?;
    let fresh = crate::ids::now_ms().saturating_sub(handoff.created_at) <= MAX_AGE_MS;
    let header = host
        .pi_call("ctx.sessionManager", "getHeader", Vec::new())
        .await
        .unwrap_or(Value::Null);
    let parent = header.get("parentSession").and_then(Value::as_str);
    let linked = match (&handoff.parent_session, parent) {
        (Some(expected), Some(actual)) => expected == actual,
        (Some(_), None) => false,
        (None, _) => true,
    };
    if !fresh {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    if !linked || handoff.version != 1 {
        return None;
    }
    std::fs::remove_file(&path).ok()?;
    Some(handoff)
}

/// `session_start` with reason `new`: kicks off a pending fresh implementation.
pub async fn receive(app: &App, host: &Host) -> Result<()> {
    let Some(handoff) = claim(app, host).await else {
        return Ok(());
    };
    let history = handoff.retention == "clear-on-start";
    let kickoff = if history {
        transferred_plan_prompt(&handoff.plan, true)
    } else {
        implementation_handoff(&handoff.plan)
    };
    let setup = async {
        app.publish_normal(host).await?;
        if !history {
            app.lock().state.active_implementation = Some(ActiveImplementation {
                id: crate::ids::uuid(),
                plan: handoff.plan.clone(),
                source: handoff.source.clone(),
                started_at: crate::ids::now_ms(),
                retention: handoff.retention.clone(),
            });
            app.persist(host).await?;
        }
        Ok::<(), Error>(())
    };
    if let Err(err) = setup.await {
        let _ = host.set_editor_text(&kickoff).await;
        let message = format!(
            "Fresh session created, but the active plan could not be saved: {}. The implementation request is in the editor; submit it to continue or resume the parent planning session.",
            ui::safe_text(err.message())
        );
        let _ = host.notify_error(message).await;
        return Ok(());
    }
    apply_runtime(
        app,
        host,
        handoff.runtime.model.as_ref(),
        handoff.runtime.thinking.as_deref(),
    )
    .await;
    app.update_ui(host).await;
    if let Err(err) = host.send_user_message(kickoff.clone(), None).await {
        let detail = ui::safe_text(err.message());
        let message = if history {
            let _ = host.set_editor_text(&kickoff).await;
            format!(
                "Fresh session created, but implementation did not start: {detail}. The implementation request is in the editor; submit it or resume the parent planning session."
            )
        } else {
            format!(
                "Fresh session created, but implementation did not start: {detail}. Send a message to continue, use /plan exit to clear the active plan, or resume the parent planning session."
            )
        };
        let _ = host.notify_error(message).await;
        return Ok(());
    }
    app.notify(
        host,
        "Fresh implementation session started. Only the approved plan was transferred.",
    )
    .await;
    Ok(())
}

/// Applies a model and thinking level in the destination session; failures keep the defaults.
pub async fn apply_runtime(
    app: &App,
    host: &Host,
    model: Option<&ModelRef>,
    thinking: Option<&str>,
) {
    if let Some(model) = model
        && current_model(host).as_ref() != Some(model)
    {
        let reference = ui::safe_text(&format!("{}/{}", model.provider, model.model_id));
        match model_auth(host, &model.provider, &model.model_id).await {
            Err(error) => {
                let message = format!(
                    "Implementation model {reference} could not be used: {error}. The destination default model will continue."
                );
                app.warn(host, message).await;
            }
            Ok(()) => {
                let applied = host.set_model(&model.provider, &model.model_id).await;
                if !matches!(applied, Ok(true)) {
                    let message = format!(
                        "Implementation model {reference} could not be applied. The destination default model will continue."
                    );
                    app.warn(host, message).await;
                }
            }
        }
    }
    if let Some(level) = thinking {
        if host.set_thinking_level(level).await.is_err() {
            let message = format!(
                "Implementation thinking {level} could not be applied. The destination default will continue."
            );
            app.warn(host, message).await;
            return;
        }
        let effective = host.thinking_level().await.ok().flatten();
        if let Some(effective) = effective.filter(|e| e != level) {
            let message = format!(
                "Implementation thinking {level} is unsupported by the destination model; Pi is using {effective}."
            );
            app.warn(host, message).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_round_trips() {
        let dir = std::env::temp_dir().join(format!("plan-mode-handoff-{}", crate::ids::uuid()));
        let path = dir.join("plan-mode/handoff/h.json");
        let handoff = Handoff {
            version: 1,
            created_at: 1,
            parent_session: Some("/s.jsonl".into()),
            plan: "# P".into(),
            source: LEGACY_SOURCE.into(),
            retention: "keep".into(),
            runtime: Runtime {
                model: Some(ModelRef {
                    provider: "p".into(),
                    model_id: "m".into(),
                }),
                thinking: Some("high".into()),
            },
        };
        write_handoff(&path, &handoff).unwrap();
        let read: Handoff = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, handoff);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(
                mode(&dir.join("plan-mode")),
                0o700,
                "drafts need a private package dir"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
