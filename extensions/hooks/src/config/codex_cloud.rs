//! Codex cloud config fragments from the cache Codex writes after a bundle fetch (Codex 0.171.0
//! codex-rs/cloud-config/src/cache.rs). Its HMAC key is public, so its hooks load as user hooks.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::codex::{CodexLayer, parse_toml};
use super::options::read_json;

pub const CACHE_FILE: &str = "cloud-config-bundle-cache.json";
const CACHE_VERSION: u64 = 1;
pub(super) const HMAC_KEY: &[u8] =
    b"codex-cloud-config-bundle-cache-v1-6160ae70-bcfd-4ca8-a99b-40f73b3b072e";

/// Cloud config fragments in Codex precedence order, lowest first. Requirements fragments are
/// not read.
pub fn load(codex_home: &Path) -> Vec<CodexLayer> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    load_at(&codex_home.join(CACHE_FILE), now)
}

fn load_at(file: &Path, now: i64) -> Vec<CodexLayer> {
    let Some(doc) = read_json(file) else {
        return Vec::new();
    };
    let payload = match valid_payload(&doc, now) {
        Ok(payload) => payload,
        Err(reason) => {
            pi_ext_sdk::info!("hooks: skipping {}: {reason}", file.display());
            return Vec::new();
        }
    };
    let items = payload.pointer("/bundle/config_toml/enterprise_managed");
    let mut layers: Vec<CodexLayer> = items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|fragment| {
            let id = fragment.get("id")?.as_str()?;
            let contents = fragment.get("contents")?.as_str()?;
            let label = format!("{}#config_toml/{id}", file.display());
            let doc = parse_toml(contents, &label)?;
            Some(CodexLayer::from_config(PathBuf::from(label), doc))
        })
        .collect();
    // Bundle fragments arrive highest priority first.
    layers.reverse();
    layers
}

/// Codex's checks without the auth identity: signature, version, identity fields, expiry.
fn valid_payload(doc: &Value, now: i64) -> Result<&Value, &'static str> {
    let payload = doc.get("signed_payload").ok_or("no signed_payload")?;
    let signature = doc.get("signature").and_then(Value::as_str);
    let signature = signature
        .and_then(|s| BASE64.decode(s).ok())
        .ok_or("no signature")?;
    let bytes = serde_json::to_vec(payload).map_err(|_| "payload does not serialize")?;
    if hmac_sha256(HMAC_KEY, &bytes)[..] != signature[..] {
        return Err("signature does not match");
    }
    if payload.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
        return Err("unsupported cache version");
    }
    for field in ["chatgpt_user_id", "account_id"] {
        if payload.get(field).and_then(Value::as_str).is_none() {
            return Err("cache identity is incomplete");
        }
    }
    let expires = payload.get("expires_at").and_then(Value::as_str);
    match expires.and_then(unix_seconds) {
        Some(expires) if expires > now => Ok(payload),
        Some(_) => Err("cache expired"),
        None => Err("no expires_at"),
    }
}

pub(super) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// Seconds since the Unix epoch of an RFC 3339 time such as `2026-11-21T10:00:00.5Z`.
fn unix_seconds(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once(['T', 't'])?;
    let numbers = |text: &str, sep: char| -> Option<Vec<i64>> {
        let parts: Vec<i64> = text
            .split(sep)
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (parts.len() == 3).then_some(parts)
    };
    let [year, month, day] = numbers(date, '-')?[..] else {
        return None;
    };
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (time, zone) = rest.split_at(zone_at);
    let [hour, minute, second] = numbers(time.split('.').next()?, ':')?[..] else {
        return None;
    };
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone[1..].split_once(':')?;
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
        }
    };
    let clock = hour * 3600 + minute * 60 + second;
    Some(days_from_civil(year, month, day) * 86_400 + clock - offset)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let short = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&short),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&long),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn rfc_3339_times_convert() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            unix_seconds("2026-11-21T10:00:00.123456Z"),
            Some(1_795_255_200)
        );
        assert_eq!(
            unix_seconds("2026-11-21T12:00:00+02:00"),
            Some(1_795_255_200)
        );
        assert_eq!(unix_seconds("soon"), None);
    }

    fn cache(dir: &Path, expires_at: &str, sign: bool) -> PathBuf {
        let payload = json!({
            "version": 1,
            "cached_at": "2026-11-21T09:00:00Z",
            "expires_at": expires_at,
            "chatgpt_user_id": "user-1",
            "account_id": "acct-1",
            "bundle": {
                "config_toml": { "enterprise_managed": [
                    { "id": "high", "name": "High", "contents": "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"high\"\n" },
                    { "id": "low", "name": "Low", "contents": "[features]\nhooks = false\n" }
                ] },
                "requirements_toml": { "enterprise_managed": [
                    { "id": "req", "name": "Req", "contents": "allow_managed_hooks_only = true\n" }
                ] }
            }
        });
        let key: &[u8] = if sign { HMAC_KEY } else { b"other" };
        let signature = BASE64.encode(hmac_sha256(key, &serde_json::to_vec(&payload).unwrap()));
        let doc = json!({ "signed_payload": payload, "signature": signature });
        let file = dir.join(CACHE_FILE);
        std::fs::write(&file, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        file
    }

    #[test]
    fn a_signed_fresh_cache_loads_lowest_fragment_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = cache(dir.path(), "2026-11-21T10:00:00Z", true);
        let configs = load_at(&file, unix_seconds("2026-11-21T09:30:00Z").unwrap());
        assert_eq!(configs.len(), 2, "requirements fragments are not read");
        assert_eq!(configs[0].feature, Some(false));
        assert_eq!(configs[1].hooks.len(), 1);
        let label = configs[1].hooks[0].0.display().to_string();
        assert!(label.ends_with("#config_toml/high"), "{label}");
    }

    #[test]
    fn expired_or_edited_caches_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let now = unix_seconds("2026-11-21T11:00:00Z").unwrap();
        let file = cache(dir.path(), "2026-11-21T10:00:00Z", true);
        assert!(load_at(&file, now).is_empty());
        let file = cache(dir.path(), "2026-11-21T12:00:00Z", false);
        assert!(load_at(&file, now).is_empty());
        let file = cache(dir.path(), "2026-11-21T12:00:00Z", true);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, text.replace("user-1", "user-2")).unwrap();
        assert!(load_at(&file, now).is_empty());
    }
}
