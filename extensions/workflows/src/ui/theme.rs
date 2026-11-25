//! Colors from pi's theme for the widget and the run report, the basic ANSI colors without a
//! theme (the panel uses those, like the other extension panels), and the status glyphs.

use pi_ext_sdk::{Host, Theme};

use crate::runs::{RunStatus, StepStatus, now_ms};

#[derive(Clone, Debug, Default)]
pub struct Paint {
    theme: Option<Theme>,
}

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

impl Paint {
    pub async fn load(host: &Host) -> Self {
        Self {
            theme: host.theme().await.ok().filter(|t| !t.fg.is_empty()),
        }
    }

    pub fn fg(&self, token: &str, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        match self.theme.as_ref().filter(|t| t.fg.contains_key(token)) {
            Some(theme) => theme.fg(token, text),
            None => {
                let code = match token {
                    "accent" | "mdHeading" => "36",
                    "success" => "32",
                    "error" => "31",
                    "warning" => "33",
                    "text" => "39",
                    _ => "90",
                };
                format!("\x1b[{code}m{text}\x1b[39m")
            }
        }
    }

    pub fn accent(&self, text: &str) -> String {
        self.fg("accent", text)
    }

    pub fn muted(&self, text: &str) -> String {
        self.fg("muted", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.fg("dim", text)
    }

    pub fn step_glyph(&self, status: StepStatus) -> String {
        match status {
            StepStatus::Pending => self.dim("○"),
            StepStatus::Running => self.accent(spinner()),
            StepStatus::Done => self.fg("success", "✓"),
            StepStatus::Failed => self.fg("error", "✗"),
        }
    }

    pub fn run_glyph(&self, status: RunStatus) -> String {
        match status {
            RunStatus::Running => self.accent(spinner()),
            RunStatus::Waiting => self.fg("warning", "◷"),
            RunStatus::Done => self.fg("success", "✓"),
            RunStatus::Failed => self.fg("error", "✗"),
            RunStatus::Cancelled => self.muted("■"),
            RunStatus::Interrupted => self.fg("warning", "◌"),
        }
    }
}

fn spinner() -> &'static str {
    SPIN[(now_ms() / 250 % SPIN.len() as u64) as usize]
}
