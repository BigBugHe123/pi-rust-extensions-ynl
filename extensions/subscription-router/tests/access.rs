//! Models per account: the policies skip an account without the model, the model list reads,
//! and a request that fails because the account lacks the model moves to another account.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::http::{MockHttp, Reply};
use common::{TestHost, codex_jwt, ctx, now_ms, temp_dir};
use pi_ext_subscription_router::access::{model_missing, refresh, same_model};
use pi_ext_subscription_router::accounts::Account;
use pi_ext_subscription_router::config::parse_config;
use pi_ext_subscription_router::family::Family;
use pi_ext_subscription_router::pick::{all_full, choose};
use pi_ext_subscription_router::state::{State, lock};
use pi_ext_subscription_router::usage::{
    BoxFuture, Scope, Status, Tokens, Urls, UsageSnapshot, UsageWindow,
};
use serde_json::{Value, json};

const HOUR: i64 = 3_600_000;
const MYTHOS: &str = "claude-mythos-5-2";
const SONNET: &str = "claude-sonnet-5";

fn usage(slot: u32, five_hour: f64) -> UsageSnapshot {
    let window = |name: &str, used, duration| UsageWindow {
        name: name.into(),
        used_percent: used,
        reset_at: Some(now_ms() + 2 * HOUR),
        duration_ms: Some(duration),
        span_source: None,
        scope: Scope::Provider,
        families: None,
    };
    UsageSnapshot {
        provider: Family::Anthropic.provider(slot),
        status: Status::Available,
        windows: vec![
            window("5h", five_hour, 5 * HOUR),
            window("7d", 20.0, 168 * HOUR),
        ],
        credits: None,
        tier: None,
        retrieved_at: now_ms(),
        expires_at: now_ms() + 300_000,
        message: None,
    }
}

fn state_at(dir: &std::path::Path, url: &str) -> Arc<State> {
    let urls = Urls {
        codex: format!("{url}/wham/usage"),
        anthropic: format!("{url}/api/oauth/usage"),
    };
    Arc::new(State::with_urls(dir, urls))
}

async fn state(name: &str, config: Value) -> Arc<State> {
    let state = state_at(&temp_dir(name), "http://127.0.0.1:9");
    let account = Account::new(Family::Anthropic, 2, None);
    state.accounts.upsert(account).await.unwrap();
    *lock(&state.config) = parse_config(&config).unwrap();
    state.accounts_mode.store(true, Ordering::Relaxed);
    state.usage().insert(usage(1, 60.0));
    state.usage().insert(usage(2, 10.0));
    state
}

#[tokio::test]
async fn policies_skip_an_account_without_the_model() {
    let balance = state("access-balance", json!({ "accounts": {} })).await;
    balance
        .models
        .insert("anthropic-2", vec![SONNET.into()], now_ms());
    let now = now_ms();
    assert_eq!(choose(&balance, Family::Anthropic, SONNET, now), 2);
    assert_eq!(choose(&balance, Family::Anthropic, MYTHOS, now), 1);
    lock(&balance.picks).pinned.insert(Family::Anthropic, 2);
    assert_eq!(choose(&balance, Family::Anthropic, MYTHOS, now), 1);
    assert_eq!(choose(&balance, Family::Anthropic, SONNET, now), 2);
    balance.usage().insert(usage(1, 100.0));
    assert!(all_full(&balance, Family::Anthropic, MYTHOS, now));
    assert!(!all_full(&balance, Family::Anthropic, SONNET, now));

    let config = json!({ "accounts": { "anthropic": { "strategy": "manual", "account": 2 } } });
    let manual = state("access-manual", config).await;
    manual
        .models
        .insert("anthropic-2", vec![SONNET.into()], now);
    assert_eq!(choose(&manual, Family::Anthropic, MYTHOS, now), 1);

    let open = state("access-open", json!({ "accounts": {} })).await;
    open.models.insert("anthropic", vec![SONNET.into()], now);
    open.models.insert("anthropic-2", vec![SONNET.into()], now);
    assert_eq!(
        choose(&open, Family::Anthropic, MYTHOS, now),
        2,
        "no account lists the model: every account stays a choice"
    );
}

#[test]
fn matches_aliases_and_detects_missing_models() {
    assert!(same_model(MYTHOS, "claude-mythos-5-2-20261101"));
    assert!(same_model("claude-mythos-5-2-2026-11-01", MYTHOS));
    assert!(!same_model(MYTHOS, "claude-mythos-5"));
    for (error, status) in [
        ("Provider HTTP 404: model: claude-mythos-5-2", None),
        ("not_found_error", Some(404)),
        (
            "The 'gpt-5.5-pro' model is not supported when using Codex with a ChatGPT account.",
            Some(400),
        ),
        ("You do not have permission to use this model", Some(403)),
    ] {
        assert!(model_missing(error, status, MYTHOS), "{error}");
    }
    for (error, status) in [
        (
            "Provider HTTP 503: the model is not available right now",
            None,
        ),
        ("temperature is not supported for this model", Some(400)),
        ("Provider HTTP 404", None),
        ("invalid x-api-key", Some(401)),
    ] {
        assert!(!model_missing(error, status, MYTHOS), "{error}");
    }
}

struct Fixed(String);

impl Tokens for Fixed {
    fn token<'a>(&'a self, _provider: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { Some(self.0.clone()) })
    }
}

#[tokio::test]
async fn reads_model_lists_as_pi_does_and_keeps_them() {
    let mock = MockHttp::start(vec![
        Reply::json(
            200,
            json!({ "data": [{ "id": SONNET }], "has_more": true, "last_id": SONNET }),
        ),
        Reply::json(
            200,
            json!({ "data": [{ "id": "claude-mythos-5-2-20261101" }], "has_more": false }),
        ),
        Reply::json(
            200,
            json!({ "models": [{ "slug": "gpt-5.5" }, { "slug": "gpt-5.5-mini", "visibility": "hide" }] }),
        ),
        Reply::json(401, json!({})),
    ])
    .await;
    let dir = temp_dir("access-read");
    let state = state_at(&dir, &mock.url);
    let token = Fixed("sk-ant-oat01-two".into());
    refresh(&state, &token, "anthropic-2", false).await;
    refresh(&state, &token, "anthropic-2", false).await;
    assert_eq!(
        state.models.has("anthropic-2", MYTHOS, now_ms()),
        Some(true)
    );
    assert_eq!(
        state.models.has("anthropic-2", "claude-opus-5-5", now_ms()),
        Some(false)
    );
    let codex = Fixed(codex_jwt("acct-2"));
    refresh(&state, &codex, "openai-codex-2", false).await;
    let list = state.models.list("openai-codex-2").unwrap();
    assert_eq!(list.models, ["gpt-5.5", "gpt-5.5-mini"]);
    refresh(&state, &token, "anthropic-3", false).await;
    assert_eq!(state.models.has("anthropic-3", MYTHOS, now_ms()), None);

    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        4,
        "a list within its hour is not read again"
    );
    assert_eq!(requests[0].path, "/v1/models?limit=1000");
    assert_eq!(
        requests[1].path,
        format!("/v1/models?limit=1000&after_id={SONNET}")
    );
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer sk-ant-oat01-two"
    );
    assert_eq!(requests[0].headers["anthropic-beta"], "oauth-2025-04-20");
    assert_eq!(requests[0].headers["anthropic-version"], "2023-06-01");
    assert_eq!(requests[2].path, "/codex/models?client_version=0.166.0");
    assert_eq!(requests[2].headers["chatgpt-account-id"], "acct-2");
    assert_eq!(requests[2].headers["originator"], "pi");

    let reloaded = state_at(&dir, &mock.url);
    assert_eq!(
        reloaded.models.has("anthropic-2", MYTHOS, now_ms()),
        Some(true)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = dir.join("ext/subscription-router/account-models.json");
        let mode = std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

fn pi(method: &str, params: &Value) -> Value {
    match (method, params["method"].as_str()) {
        ("pi.call", Some("find")) => {
            json!({ "provider": params["args"][0], "id": params["args"][1], "api": "anthropic-messages" })
        }
        ("pi.call", _) => json!(true),
        ("usage.get", _) => json!([]),
        _ => Value::Null,
    }
}

#[tokio::test]
async fn a_request_on_an_account_without_the_model_retries_on_one_with_it() {
    let mock = MockHttp::start(vec![]).await;
    let dir = temp_dir("access-retry");
    std::fs::create_dir_all(dir.join("ext/subscription-router")).unwrap();
    let accounts = json!({ "version": 1, "accounts": { "anthropic-2": { "type": "pi" } } });
    std::fs::write(
        dir.join("ext/subscription-router/accounts.json"),
        accounts.to_string(),
    )
    .unwrap();
    let state = state_at(&dir, &mock.url);
    let mut host = TestHost::start(pi_ext_subscription_router::extension(state.clone()));
    host.initialize_with(dir.to_str().unwrap(), &["providerAuth"])
        .await;
    state.usage().insert(usage(1, 60.0));
    state.usage().insert(usage(2, 10.0));

    let model = ctx(Some(("anthropic", MYTHOS)));
    let auth = |id: &str| json!({ "name": "before_provider_auth", "event": { "type": "before_provider_auth", "provider": "anthropic", "model": { "provider": "anthropic", "id": id, "api": "anthropic-messages" } }, "ctx": model });
    host.request(1, "event", auth(MYTHOS)).await;
    let (response, _) = host.serve_until(1, pi).await;
    assert_eq!(
        response["result"]["result"],
        json!({ "authProvider": "anthropic-2" })
    );

    let message = json!({ "role": "assistant", "content": [], "provider": "anthropic", "model": MYTHOS, "stopReason": "error", "errorMessage": format!("Provider HTTP 404: model: {MYTHOS}") });
    host.request(2, "event", json!({ "name": "message_end", "event": { "type": "message_end", "message": message }, "ctx": model })).await;
    let (response, seen) = host.serve_until(2, pi).await;
    assert_eq!(
        response["result"]["result"]["message"]["errorMessage"],
        format!("HTTP 429: rate limited; Claude #2 cannot use {MYTHOS}; retrying on Claude #1")
    );
    let notice = seen.iter().find(|(m, _)| m == "ui.notify").expect("notice");
    assert_eq!(
        notice.1["message"],
        format!("Claude #2 cannot use {MYTHOS}. Retrying on Claude #1.")
    );

    host.request(3, "event", auth(MYTHOS)).await;
    let (response, _) = host.serve_until(3, pi).await;
    assert_eq!(response["result"]["result"], Value::Null, "account 1");
    host.request(4, "event", auth(SONNET)).await;
    let (response, _) = host.serve_until(4, pi).await;
    assert_eq!(
        response["result"]["result"],
        json!({ "authProvider": "anthropic-2" }),
        "other models still use account 2"
    );

    // A late failure after a request of another model names no account: nothing changes.
    let late = json!({ "name": "message_end", "event": { "type": "message_end", "message": message }, "ctx": model });
    host.request(5, "event", late.clone()).await;
    let (response, _) = host.serve_until(5, pi).await;
    assert_eq!(response["result"]["result"], Value::Null);
    assert!(state.models.denied("anthropic", now_ms()).is_empty());

    // A failure on account 1, the last account with the model, keeps it there.
    host.request(6, "event", auth(MYTHOS)).await;
    host.serve_until(6, pi).await;
    host.request(7, "event", late).await;
    let (response, _) = host.serve_until(7, pi).await;
    assert_eq!(response["result"]["result"], Value::Null);
    assert!(state.models.denied("anthropic", now_ms()).is_empty());
    host.request(8, "event", auth(MYTHOS)).await;
    let (response, _) = host.serve_until(8, pi).await;
    assert_eq!(response["result"]["result"], Value::Null, "still account 1");
    host.finish().await.unwrap();
}
