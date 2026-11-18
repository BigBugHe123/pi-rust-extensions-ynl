//! The models each subscription account can use: its model list, read as pi reads its own, and
//! models a request on the account failed for. An unknown or empty list allows every model.

mod read;

pub use read::{ANTHROPIC_MODELS_ENV, CODEX_MODELS_ENV, models_url, refresh};

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::family::Family;
use crate::fsutil::write_json;
use crate::state::{State, lock};

const LIST_TTL_MS: i64 = 60 * 60_000;
/// A model that failed on an account stays off that account this long.
pub const DENIED_MS: i64 = 6 * 60 * 60_000;

static MISSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)not_found_error|model_not_found|not supported when using codex")
        .expect("valid regex")
});
static NO_ACCESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\bmodel\b\W{0,3}\s+(?:is\s+)?(?:not|no longer)\s+(?:available|supported|allowed|enabled|included)",
        r"|\bmodel\b.{0,40}\bdoes not exist",
        r"|(?:no|not have|n't have) access to (?:the |this )?model",
        r"|permission to (?:use|access) (?:the |this )?model",
    ))
    .expect("valid regex")
});
static HTTP_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:\bhttp\s*|^)(40[034])(?:[^0-9]|$)").expect("valid regex"));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelList {
    pub models: Vec<String>,
    pub read_at: i64,
}

#[derive(Default)]
struct Inner {
    lists: BTreeMap<String, ModelList>,
    /// `(account provider, model id)` that failed, until when.
    denied: HashMap<(String, String), i64>,
    /// After a failed or rejected read, the time of the next try.
    next_read: HashMap<String, i64>,
}

/// Model lists by account provider (`anthropic`, `anthropic-2`), kept in a 0600 file.
pub struct ModelAccess {
    path: PathBuf,
    inner: Mutex<Inner>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ModelAccess {
    pub fn load(path: PathBuf) -> Self {
        let stored = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value["version"] == 1);
        let lists = stored
            .and_then(|value| serde_json::from_value(value["accounts"].clone()).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(Inner {
                lists,
                ..Inner::default()
            }),
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// Whether account `provider` has model `id`; `None` when the router does not know.
    pub fn has(&self, provider: &str, id: &str, now: i64) -> Option<bool> {
        let inner = lock(&self.inner);
        let key = (provider.to_owned(), id.to_owned());
        if inner.denied.get(&key).is_some_and(|until| *until > now) {
            return Some(false);
        }
        let list = inner.lists.get(provider)?;
        Some(list.models.iter().any(|listed| same_model(id, listed)))
    }

    pub fn lacks(&self, provider: &str, id: &str, now: i64) -> bool {
        self.has(provider, id, now) == Some(false)
    }

    pub fn list(&self, provider: &str) -> Option<ModelList> {
        lock(&self.inner).lists.get(provider).cloned()
    }

    /// Models that failed on account `provider` and stay off it, sorted.
    pub fn denied(&self, provider: &str, now: i64) -> Vec<String> {
        let inner = lock(&self.inner);
        let mut models: Vec<String> = inner
            .denied
            .iter()
            .filter(|((p, _), until)| p == provider && **until > now)
            .map(|((_, id), _)| id.clone())
            .collect();
        models.sort();
        models
    }

    /// Stores the list of account `provider` as read at `now`.
    pub fn insert(&self, provider: &str, models: Vec<String>, now: i64) {
        {
            let mut inner = lock(&self.inner);
            let list = ModelList {
                models,
                read_at: now,
            };
            inner.lists.insert(provider.to_owned(), list);
            inner.next_read.remove(provider);
        }
        self.persist();
    }

    pub fn deny(&self, provider: &str, id: &str, now: i64) {
        let key = (provider.to_owned(), id.to_owned());
        lock(&self.inner).denied.insert(key, now + DENIED_MS);
    }

    /// Drops a denial that no other account could take over.
    pub fn allow(&self, provider: &str, id: &str) {
        let key = (provider.to_owned(), id.to_owned());
        lock(&self.inner).denied.remove(&key);
    }

    pub fn clear_denied(&self) {
        lock(&self.inner).denied.clear();
    }

    /// Drops what the router knows of account `provider`, after a login or a removal.
    pub fn forget(&self, provider: &str) {
        {
            let mut inner = lock(&self.inner);
            inner.lists.remove(provider);
            inner.next_read.remove(provider);
            inner.denied.retain(|(p, _), _| p != provider);
        }
        self.persist();
    }

    fn due(&self, provider: &str, now: i64) -> bool {
        let inner = lock(&self.inner);
        if inner.next_read.get(provider).is_some_and(|at| *at > now) {
            return false;
        }
        inner
            .lists
            .get(provider)
            .is_none_or(|list| now - list.read_at >= LIST_TTL_MS)
    }

    fn wait(&self, provider: &str, until: i64) {
        lock(&self.inner)
            .next_read
            .insert(provider.to_owned(), until);
    }

    fn gate(&self, provider: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = lock(&self.gates);
        gates.entry(provider.to_owned()).or_default().clone()
    }

    fn persist(&self) {
        let accounts = serde_json::to_value(&lock(&self.inner).lists).unwrap_or_default();
        if let Err(err) = write_json(&self.path, &json!({ "version": 1, "accounts": accounts })) {
            pi_ext_sdk::warn!("cannot write {}: {err}", self.path.display());
        }
    }
}

/// `id` without a trailing `-YYYYMMDD` or `-YYYY-MM-DD` snapshot date.
fn undated(id: &str) -> &str {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if let Some((base, _)) = id
        .rsplit_once('-')
        .filter(|(_, date)| date.len() == 8 && digits(date))
    {
        return base;
    }
    let parts: Vec<&str> = id.rsplitn(4, '-').collect();
    match parts.as_slice() {
        [day, month, year, base]
            if (year.len(), month.len(), day.len()) == (4, 2, 2)
                && [day, month, year].iter().all(|part| digits(part)) =>
        {
            base
        }
        _ => id,
    }
}

/// The same model as pi matches them: equal ids, or an alias and its dated snapshot.
pub fn same_model(a: &str, b: &str) -> bool {
    a == b || undated(b) == a || undated(a) == b
}

/// The accounts of `family` that have model `id`; all of them when none is known to have it.
pub fn eligible_ids(state: &State, family: Family, id: &str, now: i64) -> Vec<u32> {
    let ids = crate::pick::account_ids(state, family);
    let usable: Vec<u32> = ids
        .iter()
        .copied()
        .filter(|slot| !state.models.lacks(&family.provider(*slot), id, now))
        .collect();
    if usable.is_empty() { ids } else { usable }
}

/// A request failed because the account cannot use the model: an error type that says so, a
/// 404 that names the model, or a 400 or 403 that says the model is not available.
pub fn model_missing(error: &str, status: Option<u64>, id: &str) -> bool {
    if MISSING.is_match(error) {
        return true;
    }
    let status = status.or_else(|| HTTP_STATUS.captures(error)?[1].parse().ok());
    let lower = error.to_lowercase();
    let names_model =
        lower.contains("model") || (!id.is_empty() && lower.contains(&id.to_lowercase()));
    match status {
        Some(404) => names_model,
        Some(400 | 403) => NO_ACCESS.is_match(error),
        _ => false,
    }
}

/// Account providers of the families with more than one account.
pub fn providers(state: &State) -> Vec<String> {
    Family::ALL
        .into_iter()
        .map(|family| (family, crate::pick::account_ids(state, family)))
        .filter(|(_, ids)| ids.len() > 1)
        .flat_map(|(family, ids)| ids.into_iter().map(move |slot| family.provider(slot)))
        .collect()
}
