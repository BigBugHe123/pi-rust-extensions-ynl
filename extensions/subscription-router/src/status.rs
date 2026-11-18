//! The `subscription-router` footer status: account, quota, and reset countdown, as the
//! `indicator` config sets them.

use pi_ext_sdk::Host;

use crate::config::{ModelRef, ROUTER_PROVIDER};
use crate::family::parse_provider;
use crate::fsutil::now_ms;
use crate::slots::{SlotInfo, compact_text, indicator_text};
use crate::state::{State, current_model, lock};

pub const STATUS_KEY: &str = "subscription-router";

fn slot_text(state: &State, model: &ModelRef) -> Option<String> {
    let (family, mut slot) = parse_provider(&model.provider)?;
    let accounts = crate::pick::enabled(state);
    if slot == 1 && accounts {
        slot = crate::pick::status_slot(state, family);
    }
    let config = state.config();
    let indicator = &config.indicator;
    let now = now_ms();
    let slots = state.slots(family);
    let current = slots
        .iter()
        .find(|info| info.slot == slot)
        .cloned()
        .unwrap_or_else(|| SlotInfo {
            family,
            slot,
            snapshot: state.usage().peek(&family.provider(slot)),
        });
    let mut text = indicator_text(&current, now, indicator);
    if indicator.strategy && accounts {
        let pinned = lock(&state.picks).pinned.contains_key(&family);
        let how = match pinned {
            true => "pinned",
            false => config.policy(family).strategy.as_str(),
        };
        text.push_str(&format!(" · {how}"));
    }
    if indicator.all_accounts {
        for info in slots.iter().filter(|info| info.slot != slot) {
            let other = match state.models.lacks(&info.provider(), &model.id, now) {
                true => format!("#{} no access", info.slot),
                false => compact_text(info, now, indicator),
            };
            text.push_str(&format!(" | {other}"));
        }
    }
    Some(text)
}

/// The footer text for `current`; `None` hides it.
pub fn text(state: &State, current: Option<&ModelRef>) -> Option<String> {
    let current = current?;
    if !state.config().indicator.show {
        return None;
    }
    if current.provider != ROUTER_PROVIDER {
        let text = slot_text(state, current);
        let from = lock(&state.picks).fallback_from.clone();
        return match from.filter(|from| &from.to == current) {
            Some(from) => {
                let reason = format!("{} full, fallback", from.family.short());
                Some(text.map_or(reason.clone(), |t| format!("{t} · {reason}")))
            }
            None => text,
        };
    }
    let chosen = lock(&state.persisted).get(&current.id).cloned();
    let Some(chosen) = chosen.as_deref().and_then(ModelRef::parse) else {
        return Some(format!("router/{}", current.id));
    };
    let detail = slot_text(state, &chosen).map_or(String::new(), |t| format!(" · {t}"));
    Some(format!("router/{} → {}{detail}", current.id, chosen.key()))
}

pub async fn update(host: &Host, state: &State) {
    if !host.has_ui() {
        return;
    }
    let text = text(state, current_model(host).as_ref());
    host.set_status(STATUS_KEY, text.as_deref()).await.ok();
}
