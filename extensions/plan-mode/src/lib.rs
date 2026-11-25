//! Codex-like `/plan` mode: read-only exploration, structured questions, and an approved plan
//! before pi edits files. Rust port of `@narumitw/pi-plan-mode` 0.62.0.

mod anchor;
mod app;
mod command;
mod complete;
mod context;
mod contracts;
mod draft;
mod events;
mod export;
mod fresh;
mod ids;
mod policy;
mod question;
mod render;
mod settings;
mod settings_ui;
mod state;
mod ui;

use std::path::Path;
use std::process::ExitCode;

use pi_ext_sdk::prelude::*;

use crate::app::App;
use crate::question::QUESTION_TOOL;

pub use crate::state::STATE_ENTRY;

/// Whether `<agentDir>/keybindings.json` binds `key` to a pi action.
fn keybindings_use(agent_dir: &Path, key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(agent_dir.join("keybindings.json")) else {
        return false;
    };
    let Ok(Value::Object(bindings)) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    bindings.values().any(|value| match value {
        Value::String(k) => k.eq_ignore_ascii_case(key),
        Value::Array(keys) => keys
            .iter()
            .any(|k| k.as_str().is_some_and(|k| k.eq_ignore_ascii_case(key))),
        _ => false,
    })
}

pub fn main() -> ExitCode {
    extension(&pi_ext_sdk::paths::agent_dir()).run()
}

/// The extension for `agent_dir`. Shortcuts come from the settings and keybindings at load, as
/// pi binds extension shortcuts once.
pub fn extension(agent_dir: &Path) -> Extension {
    let toggle = settings::read(agent_dir).settings().toggle_shortcut;
    let f2 = toggle.as_deref() != Some("f2") && !keybindings_use(agent_dir, "f2");
    let app = App::new(agent_dir.to_path_buf(), f2, toggle.clone());

    let (a, b, c) = (app.clone(), app.clone(), app.clone());
    let mut ext = Extension::new("plan-mode")
        .tool(question::tool_spec(), move |args: Value, ctx: ToolCtx| {
            events::question_tool(a.clone(), args, ctx)
        })
        .tool(complete::tool_spec(), move |args: Value, ctx: ToolCtx| {
            events::complete_tool(b.clone(), args, ctx)
        })
        .command(
            "plan",
            "Enter or manage Codex-like Plan mode",
            move |args: String, ctx: CommandCtx| command::run(c.clone(), args, ctx),
        )
        .completions("plan", |prefix: String, _host: Host| async move {
            Ok(command::complete(&prefix))
        })
        .render_tool_call(
            QUESTION_TOOL,
            |req, _host| async move { Ok(render::call(&req)) },
        )
        .render_tool_result(QUESTION_TOOL, |req, _host| async move {
            Ok(render::result(&req))
        });

    let start = app.clone();
    ext = ext.on("session_start", move |ev: EventCtx| {
        let app = start.clone();
        async move {
            let host = ev.host().clone();
            app.session_start(&host).await?;
            if ev.get("reason").and_then(Value::as_str) == Some("new") {
                tokio::spawn(async move {
                    if let Err(err) = fresh::receive(&app, &host).await {
                        pi_ext_sdk::warn!("plan-mode: fresh handoff failed: {}", err.message());
                    }
                });
            }
            Ok(())
        }
    });
    let tree = app.clone();
    ext = ext.on("session_tree", move |ev: EventCtx| {
        let app = tree.clone();
        async move { app.session_tree(ev.host()).await }
    });
    ext = ext.on("session_before_tree", events::session_before_tree);
    let shutdown = app.clone();
    ext = ext.on("session_shutdown", move |ev: EventCtx| {
        let app = shutdown.clone();
        async move { app.shutdown(ev.host()).await }
    });
    let thinking = app.clone();
    ext = ext.on("thinking_level_select", move |ev: EventCtx| {
        let app = thinking.clone();
        async move {
            let level = ev
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            app.thinking_selected(ev.host(), &level).await
        }
    });
    let calls = app.clone();
    ext = ext.on("tool_call", move |ev: EventCtx| {
        events::tool_call(calls.clone(), ev)
    });
    let ends = app.clone();
    ext = ext.on("tool_execution_end", move |ev: EventCtx| {
        events::tool_execution_end(ends.clone(), ev)
    });
    let starts = app.clone();
    ext = ext.on("message_start", move |ev: EventCtx| {
        events::message_start(&starts, &ev);
        async { Ok(()) }
    });
    let contexts = app.clone();
    ext = ext.on("context", move |ev: EventCtx| {
        events::context(contexts.clone(), ev)
    });
    let before = app.clone();
    ext = ext.on("before_agent_start", move |ev: EventCtx| {
        events::before_agent_start(before.clone(), ev)
    });
    let ended = app.clone();
    ext = ext.on("agent_end", move |ev: EventCtx| {
        events::agent_end(ended.clone(), ev)
    });
    let settled = app.clone();
    ext = ext.on("agent_settled", move |ev: EventCtx| {
        events::agent_settled(settled.clone(), ev)
    });

    if f2 {
        let toggles = app.clone();
        ext = ext.shortcut(
            "f2",
            "Toggle Plan questionnaires and free discussion",
            move |ctx: CommandCtx| {
                let app = toggles.clone();
                async move { app.toggle_interaction(ctx.host(), false).await.map(drop) }
            },
        );
    }
    if let Some(key) = toggle {
        let toggles = app.clone();
        ext = ext.shortcut(&key, "Toggle Plan mode", move |ctx: CommandCtx| {
            let app = toggles.clone();
            async move { app.toggle_plan_mode(ctx.host()).await }
        });
    }
    ext
}
