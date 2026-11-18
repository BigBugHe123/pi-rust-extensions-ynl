//! One read of an account's model list: `GET /v1/models` for Anthropic and `GET /codex/models`
//! for Codex, with the headers pi sends for its own login.

use std::time::Duration;

use serde_json::Value;

use super::{LIST_TTL_MS, ModelAccess};
use crate::family::{Family, parse_provider};
use crate::fsutil::now_ms;
use crate::state::State;
use crate::usage::{ANTHROPIC_USER_AGENT, Tokens, codex_account_id};

pub const ANTHROPIC_MODELS_ENV: &str = "PI_ROUTER_ANTHROPIC_MODELS_URL";
pub const CODEX_MODELS_ENV: &str = "PI_ROUTER_CODEX_MODELS_URL";
const RETRY_MS: i64 = 10 * 60_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PAGES: usize = 20;
/// The Codex backend lists the models of this client version; pi sends the same one.
const CODEX_CLIENT_VERSION: &str = "0.166.0";

/// `$PI_ROUTER_*_MODELS_URL`, else the list endpoint next to the usage endpoint.
pub fn models_url(state: &State, family: Family) -> String {
    let (env, usage, suffix, path) = match family {
        Family::Anthropic => (
            ANTHROPIC_MODELS_ENV,
            state.usage().anthropic_url(),
            "/api/oauth/usage",
            "/v1/models",
        ),
        Family::Codex => (
            CODEX_MODELS_ENV,
            state.usage().codex_url(),
            "/wham/usage",
            "/codex/models",
        ),
    };
    if let Some(url) = std::env::var(env).ok().filter(|u| !u.is_empty()) {
        return url;
    }
    let usage = usage.trim_end_matches('/');
    format!("{}{path}", usage.strip_suffix(suffix).unwrap_or(usage))
}

/// Reads the model list of account `provider` when it is due, or now with `force`. Concurrent
/// reads of one account share one request.
pub async fn refresh(state: &State, tokens: &dyn Tokens, provider: &str, force: bool) {
    let Some((family, _)) = parse_provider(provider) else {
        return;
    };
    let access: &ModelAccess = &state.models;
    let gate = access.gate(provider);
    let _held = gate.lock().await;
    let now = now_ms();
    if !force && !access.due(provider, now) {
        return;
    }
    let token = tokio::time::timeout(REQUEST_TIMEOUT, tokens.token(provider)).await;
    let Some(token) = token.ok().flatten().filter(|t| !t.is_empty()) else {
        return;
    };
    match read(state, family, &token).await {
        Ok(Some(models)) if !models.is_empty() => access.insert(provider, models, now),
        Ok(_) => access.wait(provider, now + LIST_TTL_MS),
        Err(err) => {
            pi_ext_sdk::warn!("cannot read the models of {provider}: {err}");
            access.wait(provider, now + RETRY_MS);
        }
    }
}

/// The ids of the account's models; `Ok(None)` when the endpoint rejects the token (401, 403,
/// 404) or answers in another shape.
async fn read(state: &State, family: Family, token: &str) -> Result<Option<Vec<String>>, String> {
    let url = reqwest::Url::parse(&models_url(state, family)).map_err(|err| err.to_string())?;
    match family {
        Family::Anthropic => read_anthropic(&state.http, url, token).await,
        Family::Codex => read_codex(&state.http, url, token).await,
    }
}

async fn get(request: reqwest::RequestBuilder) -> Result<Option<Value>, String> {
    let response = request
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    if matches!(status, 401 | 403 | 404) {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    Ok(response.json().await.ok())
}

async fn read_anthropic(
    http: &reqwest::Client,
    url: reqwest::Url,
    token: &str,
) -> Result<Option<Vec<String>>, String> {
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page = url.clone();
        page.query_pairs_mut().append_pair("limit", "1000");
        if let Some(after) = &after {
            page.query_pairs_mut().append_pair("after_id", after);
        }
        let request = http
            .get(page)
            .header("anthropic-version", "2023-06-01")
            .header("User-Agent", ANTHROPIC_USER_AGENT);
        let request = if token.contains("sk-ant-oat") {
            request
                .bearer_auth(token)
                .header("anthropic-beta", "oauth-2025-04-20")
        } else {
            request.header("x-api-key", token)
        };
        let Some(body) = get(request).await? else {
            return Ok(None);
        };
        let Some(data) = body.get("data").and_then(Value::as_array) else {
            return Ok(None);
        };
        let ids = data.iter().filter_map(|m| m.get("id")?.as_str());
        models.extend(ids.filter(|id| !id.is_empty()).map(str::to_owned));
        let more = body.get("has_more").and_then(Value::as_bool) == Some(true);
        match body.get("last_id").and_then(Value::as_str).filter(|_| more) {
            Some(last) => after = Some(last.to_owned()),
            None => return Ok(Some(models)),
        }
    }
    Ok(None)
}

async fn read_codex(
    http: &reqwest::Client,
    mut url: reqwest::Url,
    token: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(account) = codex_account_id(token) else {
        return Ok(None);
    };
    url.query_pairs_mut()
        .append_pair("client_version", CODEX_CLIENT_VERSION);
    let request = http
        .get(url)
        .bearer_auth(token)
        .header("ChatGPT-Account-Id", account)
        .header("originator", "pi");
    let Some(body) = get(request).await? else {
        return Ok(None);
    };
    let Some(entries) = body.get("models").and_then(Value::as_array) else {
        return Ok(None);
    };
    let slug = |entry: &Value| {
        let text = |key: &str| entry.get(key)?.as_str().filter(|t| !t.is_empty());
        text("slug").or_else(|| text("id")).map(str::to_owned)
    };
    Ok(Some(entries.iter().filter_map(slug).collect()))
}
