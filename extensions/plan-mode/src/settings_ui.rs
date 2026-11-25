//! `/plan settings`: edits `ext/plan-mode.json` row by row (upstream `settings-menu.ts`).

use pi_ext_sdk::{Host, Result};

use crate::app::App;
use crate::command::{Picked, pick_model, pick_thinking, pick_tools};
use crate::settings::{self, FILE, Loaded, Patch, Settings};
use crate::{export, fresh, ui};

fn retention_label(retention: &str) -> &'static str {
    match retention {
        "clear-after-first-run" => "Through first implementation run",
        "keep" => "Until manually cleared",
        _ => "Off — conversation history only",
    }
}

fn tools_label(settings: &Settings) -> String {
    match &settings.default_plan_tools {
        None => "automatic safe built-ins".into(),
        Some(names) if names.is_empty() => "No optional tools".into(),
        Some(names) => ui::safe_text(&names.join(", ")),
    }
}

/// Shows the Settings menu until the user closes it.
pub async fn show(app: &App, host: &Host) -> Result<()> {
    loop {
        app.refresh_settings();
        let (snapshot, loaded_shortcut) = {
            let core = app.lock();
            (core.settings.clone(), core.toggle_shortcut.clone())
        };
        let path = app.agent_dir.join(FILE);
        if let Loaded::Invalid(reason) = &snapshot.loaded {
            let file = snapshot.path.as_deref().unwrap_or(&path);
            let title = format!(
                "Plan Mode Settings · Read only\nThe settings file is invalid: {}\nFix or remove {}, then reopen Settings.",
                ui::safe_text(reason),
                file.display()
            );
            host.select(&title, &["Close"]).await?;
            return Ok(());
        }
        let current = snapshot.settings();
        let mut lines = vec![
            "Plan Mode Settings".to_owned(),
            format!(
                "Saves to {}. Changes apply to the next Plan workflow.",
                path.display()
            ),
        ];
        if let Some(notice) = &snapshot.notice {
            lines.push(notice.clone());
        }
        let export_target = export::resolve(None, &host.cwd(), current.export_path())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let model = current
            .implementation_model
            .as_ref()
            .map_or("same as plan".to_owned(), |m| {
                ui::safe_text(&format!("{} [{}]", m.model_id, m.provider))
            });
        let shortcut = match (&current.toggle_shortcut, &loaded_shortcut) {
            (configured, loaded) if configured == loaded => {
                configured.clone().unwrap_or_else(|| "off".into())
            }
            (configured, loaded) => format!(
                "{} (loaded: {}; applies after /reload)",
                configured.as_deref().unwrap_or("off"),
                loaded.as_deref().unwrap_or("off")
            ),
        };
        let rows = [
            format!("Plan thinking: {}", current.thinking_level),
            format!("Plan policy tools: {}", tools_label(&current)),
            format!("Plan reinjection: {}", retention_label(current.retention())),
            format!("Fresh model: {model}"),
            format!(
                "Fresh thinking: {}",
                current
                    .implementation_thinking
                    .as_deref()
                    .unwrap_or("same as plan")
            ),
            format!(
                "Export destination: {} → {}",
                ui::safe_text(current.export_path()),
                ui::safe_text(&export_target)
            ),
            format!("Plan mode shortcut: {shortcut}"),
        ];
        let Some(choice) = host.select(&lines.join("\n"), &rows).await? else {
            return Ok(());
        };
        let Some(index) = rows.iter().position(|r| *r == choice) else {
            continue;
        };
        let Some(patch) = edit(app, host, index, &current).await? else {
            continue;
        };
        match settings::save(&app.agent_dir, &patch) {
            Ok(_) => app.refresh_settings(),
            Err(error) => {
                let _ = host
                    .notify_error(format!(
                        "Unable to save Plan settings: {}",
                        ui::safe_text(&error)
                    ))
                    .await;
            }
        }
    }
}

async fn edit(app: &App, host: &Host, row: usize, current: &Settings) -> Result<Option<Patch>> {
    let mut patch = Patch::default();
    match row {
        0 => {
            let mut options = vec!["inherit".to_owned()];
            options.extend(crate::state::THINKING_LEVELS.map(String::from));
            let title = "Plan thinking\nThe thinking level while Plan mode is active. inherit keeps the current level.";
            let Some(level) = host.select(title, &options).await? else {
                return Ok(None);
            };
            patch.thinking_level = Some(level);
        }
        1 => {
            let (tools, active) = app.tools(host).await;
            let initial = match &current.default_plan_tools {
                Some(names) => names.clone(),
                None => crate::policy::default_tool_names(&tools, None),
            };
            let title = "Default Plan policy allowlist\nApplies when a session has no /plan tools selection. Plan mode never activates tools.";
            let automatic = Some("Use automatic safe built-ins");
            match pick_tools(host, title, &tools, &active, initial, automatic).await? {
                None => return Ok(None),
                Some(Picked::Automatic) => patch.default_plan_tools = Some(None),
                Some(Picked::Names(names)) => patch.default_plan_tools = Some(Some(names)),
            }
        }
        2 => {
            let options = [
                ("clear-on-start", retention_label("clear-on-start")),
                (
                    "clear-after-first-run",
                    retention_label("clear-after-first-run"),
                ),
                ("keep", retention_label("keep")),
            ];
            let labels: Vec<&str> = options.iter().map(|(_, l)| *l).collect();
            let title = "Plan reinjection\nWhether the context keeps the exact approved plan after implementation starts.";
            let Some(choice) = host.select(title, &labels).await? else {
                return Ok(None);
            };
            let value = options.iter().find(|(_, l)| *l == choice).map(|(v, _)| *v);
            patch.retention = value.map(str::to_owned);
        }
        3 => {
            let models = fresh::available_models(host).await;
            let Some(choice) = pick_model(host, &models, None).await? else {
                return Ok(None);
            };
            patch.implementation_model = Some(choice);
        }
        4 => {
            let Some(choice) =
                pick_thinking(host, "Fresh implementation thinking level", None).await?
            else {
                return Ok(None);
            };
            patch.implementation_thinking = Some(choice);
        }
        5 => {
            let title = format!(
                "Export destination\nUsed when /plan export has no path. Relative paths resolve from the working directory. Leave empty for {}.",
                settings::DEFAULT_EXPORT_PATH
            );
            let Some(answer) = host.input(&title, Some(current.export_path())).await? else {
                return Ok(None);
            };
            let answer = answer.trim();
            if answer.is_empty() {
                patch.export_path = Some(None);
            } else if let Some(path) = settings::export_path(Some(answer)) {
                patch.export_path = Some(Some(path));
            } else {
                let _ = host
                    .notify_warning(
                        "Enter a path without control characters, up to 4,096 characters.",
                    )
                    .await;
                return Ok(None);
            }
        }
        _ => {
            let title = "Plan mode shortcut\nA pi key id such as ctrl+alt+p. Leave empty to turn it off. Takes effect after /reload.";
            let prefill = current.toggle_shortcut.as_deref().unwrap_or("ctrl+alt+p");
            let Some(answer) = host.input(title, Some(prefill)).await? else {
                return Ok(None);
            };
            let answer = answer.trim();
            if answer.is_empty() {
                patch.toggle_shortcut = Some(None);
            } else if let Some(key) = settings::key_id(answer) {
                patch.toggle_shortcut = Some(Some(key));
            } else {
                let _ = host
                    .notify_warning(format!(
                        "{} is not a pi key id. Use a key such as ctrl+alt+p.",
                        ui::safe_text(answer)
                    ))
                    .await;
                return Ok(None);
            }
        }
    }
    Ok(Some(patch))
}
