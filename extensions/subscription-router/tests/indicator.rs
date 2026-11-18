//! The footer indicator settings and their config, and `/router`, which opens the menu when pi
//! has a UI and shows the status without one.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{TestHost, now_ms, temp_dir};
use pi_ext_subscription_router::accounts::Account;
use pi_ext_subscription_router::config::{ModelRef, config_json, parse_config};
use pi_ext_subscription_router::family::Family;
use pi_ext_subscription_router::state::{State, lock};
use pi_ext_subscription_router::status::text;
use pi_ext_subscription_router::usage::{Scope, Status, Urls, UsageSnapshot, UsageWindow};
use serde_json::{Value, json};

const HOUR: i64 = 3_600_000;

fn usage(slot: u32, five_hour: f64, week: f64) -> UsageSnapshot {
    let window = |name: &str, used, reset_in, duration| UsageWindow {
        name: name.into(),
        used_percent: used,
        reset_at: Some(now_ms() + reset_in),
        duration_ms: Some(duration),
        span_source: None,
        scope: Scope::Provider,
        families: None,
    };
    UsageSnapshot {
        provider: Family::Anthropic.provider(slot),
        status: Status::Available,
        windows: vec![
            window("5h", five_hour, 2 * HOUR, 5 * HOUR),
            window("7d", week, 72 * HOUR, 168 * HOUR),
        ],
        credits: None,
        tier: None,
        retrieved_at: now_ms(),
        expires_at: now_ms() + 300_000,
        message: None,
    }
}

fn urls() -> Urls {
    Urls {
        codex: "http://127.0.0.1:9/wham/usage".into(),
        anthropic: "http://127.0.0.1:9/api/oauth/usage".into(),
    }
}

async fn state(name: &str, indicator: Value) -> Arc<State> {
    let state = Arc::new(State::with_urls(&temp_dir(name), urls()));
    for slot in [2, 3] {
        let account = Account::new(Family::Anthropic, slot, None);
        state.accounts.upsert(account).await.unwrap();
    }
    *lock(&state.config) = parse_config(&json!({ "indicator": indicator })).unwrap();
    state.accounts_mode.store(true, Ordering::Relaxed);
    state.usage().insert(usage(1, 20.0, 37.0));
    state.usage().insert(usage(2, 100.0, 50.0));
    state
}

fn footer(state: &State) -> Option<String> {
    text(state, Some(&ModelRef::new("anthropic", "claude-sonnet-5")))
}

#[tokio::test]
async fn settings_change_what_the_footer_shows() {
    let cases = [
        (json!({}), "Claude #1 · 63% · 7d 3d"),
        (json!({ "percent": "used" }), "Claude #1 · 37% used · 7d 3d"),
        (json!({ "window": "5h" }), "Claude #1 · 5h 80% · 2h"),
        (
            json!({ "window": "7d", "reset": false }),
            "Claude #1 · 7d 63%",
        ),
        (
            json!({ "strategy": true }),
            "Claude #1 · 63% · 7d 3d · balance",
        ),
        (
            json!({ "accounts": "all", "reset": false }),
            "Claude #1 · 63% | #2 5h limit | #3 no access",
        ),
    ];
    for (index, (indicator, expected)) in cases.into_iter().enumerate() {
        let state = state(&format!("indicator-{index}"), indicator).await;
        let model = vec!["claude-opus-5-5".to_owned()];
        state.models.insert("anthropic-3", model, now_ms());
        assert_eq!(footer(&state).as_deref(), Some(expected));
    }
    let pinned = state("indicator-pinned", json!({ "strategy": true })).await;
    lock(&pinned.picks).pinned.insert(Family::Anthropic, 1);
    assert_eq!(
        footer(&pinned).as_deref(),
        Some("Claude #1 · 63% · 7d 3d · pinned")
    );
    let hidden = state("indicator-hidden", json!({ "show": false })).await;
    assert_eq!(footer(&hidden), None);
}

#[test]
fn the_indicator_config_round_trips_and_rejects_bad_values() {
    let indicator = json!({ "show": false, "percent": "used", "window": "5h", "reset": false,
        "accounts": "all", "strategy": true });
    let config = parse_config(&json!({ "indicator": indicator })).unwrap();
    let written = config_json(&config);
    assert_eq!(written["indicator"], indicator);
    assert_eq!(parse_config(&written).unwrap(), config);
    let default = parse_config(&json!({ "indicator": {} })).unwrap();
    assert!(config_json(&default).get("indicator").is_none());
    for (bad, error) in [
        (json!({ "window": "1d" }), "indicator.window"),
        (json!({ "show": "no" }), "indicator.show"),
        (json!({ "accounts": 2 }), "indicator.accounts"),
    ] {
        let message = parse_config(&json!({ "indicator": bad })).unwrap_err();
        assert!(message.contains(error), "{message}");
    }
}

/// `/router` with no arguments; returns the dialog titles and notices it sent.
async fn bare_router(has_ui: bool) -> (Vec<String>, Vec<String>) {
    let dir = temp_dir(&format!("router-menu-{has_ui}"));
    let state = Arc::new(State::with_urls(&dir, urls()));
    let mut host = TestHost::start(pi_ext_subscription_router::extension(state));
    host.initialize(dir.to_str().unwrap()).await;
    let ctx = json!({ "cwd": "/work", "hasUI": has_ui, "flags": {}, "model": null });
    let run = json!({ "name": "router", "args": "", "ctx": ctx });
    host.request(1, "command.run", run).await;
    let (_, seen) = host.serve_until(1, |_, _| Value::Null).await;
    host.finish().await.unwrap();
    let of = |method: &str, key: &str| -> Vec<String> {
        let found = seen.iter().filter(|(m, _)| m == method);
        found
            .map(|(_, p)| p[key].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    (of("ui.select", "title"), of("ui.notify", "message"))
}

#[tokio::test]
async fn bare_router_opens_the_menu_with_a_ui_and_shows_status_without_one() {
    let (dialogs, notices) = bare_router(true).await;
    assert_eq!(dialogs, ["Subscription router"]);
    assert!(notices.is_empty(), "{notices:?}");
    let (dialogs, notices) = bare_router(false).await;
    assert!(dialogs.is_empty(), "{dialogs:?}");
    assert_eq!(
        notices,
        ["subscription-router: no subscription providers or profiles."]
    );
}
