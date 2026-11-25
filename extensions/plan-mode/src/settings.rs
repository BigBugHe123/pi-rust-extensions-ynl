//! Settings in `<agentDir>/ext/plan-mode.json`, with upstream `<agentDir>/pi-plan-mode.json` as
//! a read-only fallback. Only the global file exists: a project file could widen the Plan policy.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::policy::SafeSubcommands;
use crate::state::{ModelRef, RETENTIONS, THINKING_LEVELS};

pub const FILE: &str = "ext/plan-mode.json";
pub const UPSTREAM_FILE: &str = "pi-plan-mode.json";
pub const DEFAULT_EXPORT_PATH: &str = "PLAN.md";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_EXPORT_PATH: usize = 4096;
const MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "super"];
const NAMED_KEYS: &[&str] = &[
    "escape",
    "esc",
    "enter",
    "return",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "clear",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
];
const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `inherit` or a fixed level.
    pub thinking_level: String,
    pub default_plan_tools: Option<Vec<String>>,
    pub retention: Option<String>,
    pub implementation_model: Option<ModelRef>,
    pub implementation_thinking: Option<String>,
    pub export_path: Option<String>,
    pub safe_subcommands: SafeSubcommands,
    pub toggle_shortcut: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thinking_level: "inherit".into(),
            default_plan_tools: None,
            retention: None,
            implementation_model: None,
            implementation_thinking: None,
            export_path: None,
            safe_subcommands: SafeSubcommands::new(),
            toggle_shortcut: None,
        }
    }
}

impl Settings {
    pub fn plan_thinking(&self) -> Option<&str> {
        (self.thinking_level != "inherit").then_some(self.thinking_level.as_str())
    }

    pub fn retention(&self) -> &str {
        self.retention.as_deref().unwrap_or("clear-on-start")
    }

    pub fn export_path(&self) -> &str {
        self.export_path.as_deref().unwrap_or(DEFAULT_EXPORT_PATH)
    }

    /// Validates a settings document like upstream `normalizePlanModeSettings`.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "settings must be a JSON object".to_owned())?;
        let mut settings = Settings::default();
        if let Some(level) = object.get("thinkingLevel") {
            settings.thinking_level = level
                .as_str()
                .filter(|l| *l == "inherit" || THINKING_LEVELS.contains(l))
                .ok_or("invalid thinkingLevel")?
                .to_owned();
        }
        if let Some(tools) = object.get("defaultPlanTools") {
            let names = tools
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .filter(|s| !s.trim().is_empty())
                                .map(str::to_owned)
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or("invalid defaultPlanTools")?;
            settings.default_plan_tools = Some(crate::policy::unique(&names));
        }
        if let Some(retention) = object.get("implementationPlanRetention") {
            let retention = retention
                .as_str()
                .filter(|r| RETENTIONS.contains(r))
                .ok_or("invalid implementationPlanRetention")?;
            settings.retention = Some(retention.to_owned());
        }
        if let Some(model) = object.get("defaultImplementationModel") {
            settings.implementation_model =
                Some(model_ref(model).ok_or("invalid defaultImplementationModel")?);
        }
        if let Some(level) = object.get("defaultImplementationThinkingLevel") {
            let level = level
                .as_str()
                .filter(|l| THINKING_LEVELS.contains(l))
                .ok_or("invalid defaultImplementationThinkingLevel")?;
            settings.implementation_thinking = Some(level.to_owned());
        }
        if let Some(path) = object.get("defaultPlanExportPath") {
            settings.export_path =
                Some(export_path(path.as_str()).ok_or("invalid defaultPlanExportPath")?);
        }
        if let Some(key) = object.get("toggleShortcut") {
            settings.toggle_shortcut =
                Some(key_id(key.as_str().unwrap_or("")).ok_or("invalid toggleShortcut")?);
        }
        if let Some(pane) = object.get("planPane") {
            pane.as_str()
                .filter(|p| ["right", "left", "off"].contains(p))
                .ok_or("invalid planPane")?;
        }
        if let Some(safe) = object.get("safeSubcommands") {
            settings.safe_subcommands = safe_subcommands(safe).ok_or("invalid safeSubcommands")?;
        }
        Ok(settings)
    }
}

fn model_ref(value: &Value) -> Option<ModelRef> {
    let object = value.as_object()?;
    if object.keys().any(|k| k != "provider" && k != "modelId") {
        return None;
    }
    let model = ModelRef {
        provider: object.get("provider")?.as_str()?.trim().to_owned(),
        model_id: object.get("modelId")?.as_str()?.trim().to_owned(),
    };
    model.valid().then_some(model)
}

pub fn export_path(value: Option<&str>) -> Option<String> {
    let path = value?.trim();
    let ok = !path.is_empty()
        && path.encode_utf16().count() <= MAX_EXPORT_PATH
        && path.chars().any(|c| c != '@' && !c.is_whitespace())
        && !path.chars().any(char::is_control);
    ok.then(|| path.to_owned())
}

/// A pi key id such as `ctrl+shift+p`, lowercased; `None` when invalid (upstream `normalizeKeyId`).
pub fn key_id(value: &str) -> Option<String> {
    let normalized = value.trim().to_lowercase();
    let mut keys: Vec<String> = ('a'..='z')
        .chain('0'..='9')
        .chain(SYMBOL_KEYS.chars())
        .map(String::from)
        .chain(NAMED_KEYS.iter().map(|k| (*k).to_owned()))
        .collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let base = keys
        .into_iter()
        .find(|k| normalized == *k || normalized.ends_with(&format!("+{k}")))?;
    let prefix = &normalized[..normalized.len() - base.len()];
    if prefix.is_empty() {
        return Some(base);
    }
    let function_key = base.len() > 1 && base.starts_with('f') && base[1..].parse::<u8>().is_ok();
    if function_key || !prefix.ends_with('+') {
        return None;
    }
    let mut seen: Vec<&str> = Vec::new();
    for modifier in prefix[..prefix.len() - 1].split('+') {
        if !MODIFIERS.contains(&modifier) || seen.contains(&modifier) {
            return None;
        }
        seen.push(modifier);
    }
    Some(normalized)
}

fn safe_subcommands(value: &Value) -> Option<SafeSubcommands> {
    let mut result = SafeSubcommands::new();
    for (command, subcommands) in value.as_object()? {
        let command = command.trim();
        if command.is_empty() {
            return None;
        }
        let items = subcommands.as_array()?;
        let entry = result.entry(command.to_owned()).or_default();
        for item in items {
            let sub = item.as_str()?.trim();
            if sub.is_empty() {
                return None;
            }
            if !entry.iter().any(|s| s == sub) {
                entry.push(sub.to_owned());
            }
        }
    }
    Some(result)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    Missing,
    Invalid(String),
    Valid(Settings),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub loaded: Loaded,
    /// The file the settings came from.
    pub path: Option<PathBuf>,
    /// A warning about the upstream fallback.
    pub notice: Option<String>,
}

impl Snapshot {
    pub fn settings(&self) -> Settings {
        match &self.loaded {
            Loaded::Valid(settings) => settings.clone(),
            _ => Settings::default(),
        }
    }
}

fn read_document(path: &Path) -> Result<Option<Value>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if !meta.is_file() {
        return Err("settings path is not a regular file".into());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!("settings file exceeds {MAX_BYTES} bytes"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("invalid JSON: {e}"))
}

pub fn read(agent_dir: &Path) -> Snapshot {
    let canonical = agent_dir.join(FILE);
    let upstream = agent_dir.join(UPSTREAM_FILE);
    let load = |path: &Path| match read_document(path) {
        Ok(None) => Loaded::Missing,
        Ok(Some(value)) => match Settings::from_value(&value) {
            Ok(settings) => Loaded::Valid(settings),
            Err(reason) => Loaded::Invalid(reason),
        },
        Err(reason) => Loaded::Invalid(reason),
    };
    let loaded = load(&canonical);
    if loaded != Loaded::Missing {
        let notice = upstream
            .exists()
            .then(|| format!("{UPSTREAM_FILE} ignored because {FILE} takes precedence."));
        return Snapshot {
            loaded,
            path: Some(canonical),
            notice,
        };
    }
    let loaded = load(&upstream);
    if loaded == Loaded::Missing {
        return Snapshot {
            loaded,
            path: None,
            notice: None,
        };
    }
    let notice = matches!(loaded, Loaded::Valid(_)).then(|| {
        format!("Using {UPSTREAM_FILE}; Settings saves write {FILE} and leave {UPSTREAM_FILE} unchanged.")
    });
    Snapshot {
        loaded,
        path: Some(upstream),
        notice,
    }
}

/// A change from the Settings menu. `Some(None)` removes the key.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub thinking_level: Option<String>,
    pub default_plan_tools: Option<Option<Vec<String>>>,
    pub retention: Option<String>,
    pub implementation_model: Option<Option<ModelRef>>,
    pub implementation_thinking: Option<Option<String>>,
    pub export_path: Option<Option<String>>,
    pub toggle_shortcut: Option<Option<String>>,
}

fn set(document: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            document.insert(key.to_owned(), value);
        }
        None => {
            document.remove(key);
        }
    }
}

/// Applies `patch` to the latest valid document and writes `ext/plan-mode.json` through a temp file
/// and a rename. Unknown keys stay. Returns the saved settings.
pub fn save(agent_dir: &Path, patch: &Patch) -> Result<Settings, String> {
    let snapshot = read(agent_dir);
    if let Loaded::Invalid(reason) = &snapshot.loaded {
        return Err(format!("settings are invalid: {reason}"));
    }
    let mut document = match &snapshot.path {
        Some(path) => read_document(path)?
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        None => Map::new(),
    };
    if let Some(level) = &patch.thinking_level {
        document.insert("thinkingLevel".into(), Value::String(level.clone()));
    }
    if let Some(tools) = &patch.default_plan_tools {
        set(
            &mut document,
            "defaultPlanTools",
            tools.as_ref().map(|t| serde_json::json!(t)),
        );
    }
    if let Some(retention) = &patch.retention {
        document.insert(
            "implementationPlanRetention".into(),
            Value::String(retention.clone()),
        );
    }
    if let Some(model) = &patch.implementation_model {
        let value = model
            .as_ref()
            .map(|m| serde_json::json!({ "provider": m.provider, "modelId": m.model_id }));
        set(&mut document, "defaultImplementationModel", value);
    }
    if let Some(level) = &patch.implementation_thinking {
        set(
            &mut document,
            "defaultImplementationThinkingLevel",
            level.clone().map(Value::String),
        );
    }
    if let Some(path) = &patch.export_path {
        set(
            &mut document,
            "defaultPlanExportPath",
            path.clone().map(Value::String),
        );
    }
    if let Some(key) = &patch.toggle_shortcut {
        set(
            &mut document,
            "toggleShortcut",
            key.clone().map(Value::String),
        );
    }
    let value = Value::Object(document);
    let settings = Settings::from_value(&value)?;
    let path = agent_dir.join(FILE);
    let dir = path.parent().ok_or("settings path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let temp = dir.join(format!(".plan-mode.json.{}.tmp", crate::ids::uuid()));
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&temp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })?;
    Ok(settings)
}

/// The settings files' modification stamp, to reload after a manual edit.
pub fn stamp(agent_dir: &Path) -> Vec<Option<(SystemTime, u64)>> {
    [FILE, UPSTREAM_FILE]
        .iter()
        .map(|file| {
            let meta = std::fs::metadata(agent_dir.join(file)).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn validates_like_upstream() {
        let settings = Settings::from_value(&json!({
            "thinkingLevel": "high",
            "defaultPlanTools": ["read", "read", "bash"],
            "implementationPlanRetention": "keep",
            "defaultImplementationModel": { "provider": "anthropic", "modelId": "m" },
            "defaultPlanExportPath": " docs/PLAN.md ",
            "safeSubcommands": { "kubectl": ["get", " get "], " kubectl ": ["apply"] },
            "toggleShortcut": "Ctrl+Shift+P",
            "planPane": "left",
            "unknown": 1
        }))
        .unwrap();
        assert_eq!(settings.plan_thinking(), Some("high"));
        assert_eq!(
            settings.default_plan_tools,
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(settings.export_path(), "docs/PLAN.md");
        assert_eq!(settings.safe_subcommands["kubectl"], vec!["get", "apply"]);
        assert_eq!(settings.toggle_shortcut.as_deref(), Some("ctrl+shift+p"));
        for bad in [
            json!({ "thinkingLevel": "huge" }),
            json!({ "defaultPlanTools": [""] }),
            json!({ "safeSubcommands": { "git": "status" } }),
            json!({ "toggleShortcut": "hyper+x" }),
            json!({ "defaultPlanExportPath": "@" }),
            json!([]),
        ] {
            assert!(Settings::from_value(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_ids() {
        assert_eq!(key_id("f2").as_deref(), Some("f2"));
        assert_eq!(key_id("ctrl+alt+p").as_deref(), Some("ctrl+alt+p"));
        assert_eq!(key_id("ctrl++").as_deref(), Some("ctrl++"));
        assert_eq!(key_id("shift+f2"), None);
        assert_eq!(key_id("ctrl+ctrl+p"), None);
        assert_eq!(key_id("ctrlp"), None);
    }

    #[test]
    fn save_keeps_unknown_keys_and_prefers_the_canonical_file() {
        let dir = std::env::temp_dir().join(format!("plan-mode-settings-{}", crate::ids::uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(UPSTREAM_FILE),
            r#"{"thinkingLevel":"low","extra":true}"#,
        )
        .unwrap();
        let snapshot = read(&dir);
        assert_eq!(snapshot.settings().plan_thinking(), Some("low"));
        assert!(snapshot.notice.is_some());
        let patch = Patch {
            export_path: Some(Some("out.md".into())),
            ..Patch::default()
        };
        let saved = save(&dir, &patch).unwrap();
        assert_eq!(saved.export_path(), "out.md");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(written["extra"], true);
        assert_eq!(written["thinkingLevel"], "low");
        let snapshot = read(&dir);
        assert_eq!(snapshot.path, Some(dir.join(FILE)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
