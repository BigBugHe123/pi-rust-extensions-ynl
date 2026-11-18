//! `/router indicator`: what the footer status shows, saved to the config at each change.

use std::sync::Arc;

use pi_ext_sdk::prelude::*;

use super::index_of;
use crate::config::{Indicator, IndicatorPercent, IndicatorWindow};
use crate::menu::profiles::persist;
use crate::state::{State, current_model};

const WINDOW_HELP: [(IndicatorWindow, &str); 3] = [
    (
        IndicatorWindow::Auto,
        "auto: weekly percent, reset of the fuller window",
    ),
    (IndicatorWindow::FiveHour, "5h: the five-hour window"),
    (IndicatorWindow::Week, "7d: the weekly window"),
];

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn options(indicator: &Indicator) -> [String; 6] {
    let accounts = match indicator.all_accounts {
        true => "all",
        false => "the one in use",
    };
    [
        format!("Show: {}", on_off(indicator.show)),
        format!("Percent: {}", indicator.percent.as_str()),
        format!("Window: {}", indicator.window.as_str()),
        format!("Reset countdown: {}", on_off(indicator.reset)),
        format!("Accounts: {accounts}"),
        format!("Account policy: {}", on_off(indicator.strategy)),
    ]
}

/// The footer as it shows now, for the menu title.
fn preview(host: &Host, state: &State, indicator: &Indicator) -> String {
    match crate::status::text(state, current_model(host).as_ref()) {
        Some(text) => text,
        None if !indicator.show => "hidden".into(),
        None => "shows with an Anthropic or Codex model".into(),
    }
}

pub async fn menu(host: &Host, state: &Arc<State>) -> Result<()> {
    loop {
        let indicator = state.config().indicator;
        let options = options(&indicator);
        let title = format!("Footer: {}", preview(host, state, &indicator));
        let Some(index) = index_of(&options, host.select(&title, &options).await?) else {
            return Ok(());
        };
        let mut next = indicator.clone();
        match index {
            0 => next.show = !next.show,
            1 => {
                next.percent = match next.percent {
                    IndicatorPercent::Left => IndicatorPercent::Used,
                    IndicatorPercent::Used => IndicatorPercent::Left,
                }
            }
            2 => {
                let windows: Vec<&str> = WINDOW_HELP.iter().map(|(_, help)| *help).collect();
                let Some(i) = index_of(&windows, host.select("Window", &windows).await?) else {
                    continue;
                };
                next.window = WINDOW_HELP[i].0;
            }
            3 => next.reset = !next.reset,
            4 => next.all_accounts = !next.all_accounts,
            _ => next.strategy = !next.strategy,
        }
        if next != indicator {
            let mut config = state.config();
            config.indicator = next;
            persist(host, state, config).await?;
            crate::status::update(host, state).await;
        }
    }
}
