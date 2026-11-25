//! The managed Markdown draft: one private file per workflow that native `write` and `edit` may
//! change in Plan mode (upstream `plan-draft.ts` and `draft-contract.ts`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::anchor;
use crate::contracts::{context_message, latest_mode_contract};
use crate::state::{Boundary, DraftIdentity, MAX_PLAN_CHARS, validate_plan};

pub const CONTRACT_ENTRY: &str = "plan-mode-draft-contract";
pub const TOOL_NAMES: [&str; 3] = ["read", "write", "edit"];
pub const READ_LIMIT: &str = "Keep the draft within 50 KiB, 2,000 lines, and 50,000 characters.";
const MAX_BYTES: u64 = 50 * 1024;
const MAX_LINES: usize = 2000;

/// The draft directory of a workflow under the agent directory.
fn directories(root: &Path, workflow_id: &str) -> [PathBuf; 3] {
    let package = pi_ext_sdk::paths::extension_dir(root, "plan-mode");
    let drafts = package.join("drafts");
    let workflow = drafts.join(workflow_id);
    [package, drafts, workflow]
}

#[derive(Debug, Clone)]
struct DraftFile {
    root: PathBuf,
    workflow_id: String,
    path: PathBuf,
    /// The workflow directory this process created, for rollback.
    created: Option<(u64, u64)>,
}

#[cfg(unix)]
fn private_and_identity(meta: &std::fs::Metadata) -> (bool, (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    (meta.mode() & 0o077 == 0, (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn private_and_identity(_meta: &std::fs::Metadata) -> (bool, (u64, u64)) {
    (true, (0, 0))
}

#[cfg(unix)]
fn single_link(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() == 1
}

#[cfg(not(unix))]
fn single_link(_meta: &std::fs::Metadata) -> bool {
    true
}

fn check_directory(path: &Path) -> Result<(u64, u64), String> {
    let error = || "Draft directory is not a private, canonical directory.".to_owned();
    let meta = std::fs::symlink_metadata(path).map_err(|_| error())?;
    let (private, identity) = private_and_identity(&meta);
    let canonical = std::fs::canonicalize(path).map_err(|_| error())?;
    if !meta.is_dir() || meta.file_type().is_symlink() || canonical != path || !private {
        return Err(error());
    }
    Ok(identity)
}

#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

impl DraftFile {
    /// Checks the directories and, when the file exists, that it is a regular, single-link file.
    fn check(&self) -> Result<Option<u64>, String> {
        if std::fs::canonicalize(&self.root).ok().as_deref() != Some(self.root.as_path()) {
            return Err("Draft root has changed.".into());
        }
        for directory in directories(&self.root, &self.workflow_id) {
            check_directory(&directory)?;
        }
        match std::fs::symlink_metadata(&self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.to_string()),
            Ok(meta) => {
                let canonical = std::fs::canonicalize(&self.path).ok();
                if !meta.is_file()
                    || meta.file_type().is_symlink()
                    || !single_link(&meta)
                    || canonical.as_deref() != Some(self.path.as_path())
                {
                    return Err("Draft is not a regular, unlinked file.".into());
                }
                Ok(Some(meta.len()))
            }
        }
    }

    fn read(&self) -> Result<Option<String>, String> {
        let Some(size) = self.check()? else {
            return Ok(None);
        };
        if size > MAX_BYTES {
            return Err(READ_LIMIT.into());
        }
        let bytes = std::fs::read(&self.path).map_err(|e| e.to_string())?;
        let text = String::from_utf8(bytes)
            .map_err(|_| "Draft is not readable as UTF-8 Markdown. Write it again as text.")?;
        if text.lines().count() > MAX_LINES || text.encode_utf16().count() > MAX_PLAN_CHARS {
            return Err(READ_LIMIT.into());
        }
        Ok(Some(text))
    }
}

fn bind_file(agent_dir: &Path, identity: &DraftIdentity) -> Result<DraftFile, String> {
    let root = std::fs::canonicalize(agent_dir).map_err(|e| e.to_string())?;
    let dirs = directories(&root, &identity.workflow_id);
    let path = dirs[2].join("plan.md");
    if !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control) {
        return Err("Pi cannot safely resolve the draft path.".into());
    }
    let mut file = DraftFile {
        root,
        workflow_id: identity.workflow_id.clone(),
        path,
        created: None,
    };
    if identity.status == "pending" {
        if let Some(parent) = dirs[0].parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        for (index, directory) in dirs.iter().enumerate() {
            if let Err(err) = create_private_dir(directory)
                && (index == 2 || err.kind() != std::io::ErrorKind::AlreadyExists)
            {
                return Err(err.to_string());
            }
            let identity = check_directory(directory)?;
            if index == 2 {
                file.created = Some(identity);
            }
        }
    }
    file.check()?;
    Ok(file)
}

/// The draft of the current workflow and its in-flight `write` and `edit` calls.
#[derive(Debug, Default)]
pub struct Draft {
    file: Option<DraftFile>,
    mutations: HashSet<String>,
    revision: u64,
}

impl Draft {
    pub fn reset(&mut self) {
        self.file = None;
        self.mutations.clear();
        self.revision += 1;
    }

    /// Binds the workflow's file, creating its private directory when the draft is pending.
    pub fn bind(&mut self, agent_dir: &Path, identity: &DraftIdentity) -> bool {
        self.reset();
        self.file = bind_file(agent_dir, identity).ok();
        self.file.is_some()
    }

    /// Removes the workflow directory this process created and forgets the file.
    pub fn rollback(&mut self) {
        let previous = self.file.take();
        self.reset();
        let Some(file) = previous else { return };
        let Some(created) = file.created else { return };
        let dirs = directories(&file.root, &file.workflow_id);
        if dirs.iter().take(2).all(|d| check_directory(d).is_ok())
            && check_directory(&dirs[2]).ok() == Some(created)
        {
            let _ = std::fs::remove_dir(&dirs[2]);
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|f| f.path.as_path())
    }

    /// Admits a `write` or `edit` call on exactly the draft path.
    pub fn admit(&mut self, call_id: &str, path: Option<&str>) -> Result<(), String> {
        let file = self
            .file
            .as_ref()
            .filter(|f| path.map(Path::new) == Some(f.path.as_path()))
            .ok_or("Use the exact absolute path to this workflow's Markdown draft.")?;
        file.check()
            .map_err(|_| "The draft path is unavailable or unsafe.")?;
        self.revision += 1;
        self.mutations.insert(call_id.to_owned());
        Ok(())
    }

    pub fn finish(&mut self, call_id: &str) {
        self.mutations.remove(call_id);
    }

    pub fn settle(&mut self) {
        self.mutations.clear();
    }

    /// The validated draft text for `plan_mode_complete({})`.
    pub fn snapshot(&self) -> Result<String, String> {
        let file = self
            .file
            .as_ref()
            .ok_or("The draft directory is unavailable. Restore it and reload Plan mode.")?;
        if !self.mutations.is_empty() {
            return Err("Wait for draft writes to finish before completing the plan.".into());
        }
        let text = file
            .read()?
            .ok_or("Write the decision-ready Markdown draft before completing the plan.")?;
        validate_plan(&text).map_err(|e| format!("Draft {e}. Update the draft and try again."))
    }
}

/// What the draft contract tells the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Authorization {
    Managed { path: String },
    Unavailable,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractEntry {
    pub version: u8,
    pub authorization: Authorization,
    pub boundary: Boundary,
}

fn contract_text(authorization: &Authorization) -> String {
    let marker = "[PI PLAN MODE DRAFT CONTRACT v1]";
    let policy = "Only use this authorization while Plan mode is active. This contract overrides generic Plan, interaction, and finalization wording about file edits and completion parameters, including after questionnaire toggles. Questionnaire and discussion rules still apply. Do not edit project files or implement the plan.";
    match authorization {
        Authorization::ReadOnly => format!(
            "{marker}\nNo draft is writable in this session. Ignore earlier draft authorizations. Use the read-only Plan mode contract and pass the complete Markdown as plan_mode_complete({{ plan }})."
        ),
        Authorization::Unavailable => format!(
            "{marker}\n{policy}\nThe managed draft directory is unavailable. Do not write elsewhere or supply plan text to plan_mode_complete. Ask the user to restore the directory and reload."
        ),
        Authorization::Managed { path } => format!(
            "{marker}\n{policy}\nUse Pi's native write, edit, and read to maintain the single Plan Markdown draft. The only file you may change is the draft at this exact absolute path (as a JSON string): {}.\nCall write or edit directly, not through another tool. Write to create the draft, edit it for targeted revisions, and read it to check the result. {READ_LIMIT}\nOnce the file contains the complete decision-ready plan, call plan_mode_complete({{}}) alone as the final action. It reads the file and stores a snapshot; do not resend the Markdown as a tool argument or present it as an assistant message.",
            Value::String(path.clone())
        ),
    }
}

fn contract_message(authorization: &Authorization) -> Value {
    context_message(CONTRACT_ENTRY, &contract_text(authorization), None)
}

fn is_contract_message(message: &Value, content: &str) -> bool {
    message.get("role").and_then(Value::as_str) == Some("custom")
        && message.get("customType").and_then(Value::as_str) == Some(CONTRACT_ENTRY)
        && message.get("display") == Some(&Value::Bool(false))
        && message.get("content").and_then(Value::as_str) == Some(content)
}

pub fn parse_entry(entry: &Value) -> Option<ContractEntry> {
    if entry.get("type").and_then(Value::as_str) != Some("custom")
        || entry.get("customType").and_then(Value::as_str) != Some(CONTRACT_ENTRY)
    {
        return None;
    }
    let parsed: ContractEntry = serde_json::from_value(entry.get("data")?.clone()).ok()?;
    (parsed.version == 1 && anchor::is_hash(&parsed.boundary.hash)).then_some(parsed)
}

/// Whether an earlier draft contract of this branch granted a writable draft.
pub fn has_historical_grant(branch: &[Value]) -> bool {
    branch
        .iter()
        .filter_map(parse_entry)
        .any(|e| e.authorization != Authorization::ReadOnly)
}

/// Puts recorded draft contracts back at their boundaries and appends the expected one when the
/// latest after the latest mode contract differs. Returns the entry to persist for a new contract.
pub fn reconcile(
    messages: Vec<Value>,
    branch: &[Value],
    expected: Option<&Authorization>,
) -> (Vec<Value>, Option<ContractEntry>) {
    let mut result = messages;
    let mut last: Option<(usize, String)> = None;
    for recorded in branch.iter().filter_map(parse_entry) {
        let index = recorded.boundary.length;
        if !anchor::matches(&result, &recorded.boundary) {
            continue;
        }
        let content = contract_text(&recorded.authorization);
        if !result
            .get(index)
            .is_some_and(|m| is_contract_message(m, &content))
        {
            result.insert(index, contract_message(&recorded.authorization));
        }
        if last.as_ref().is_none_or(|(at, _)| index >= *at) {
            last = Some((index, content));
        }
    }
    let Some(expected) = expected else {
        return (result, None);
    };
    let mode_index = latest_mode_contract(&result).map(|(i, _)| i);
    if let Some((index, content)) = &last
        && mode_index.is_none_or(|m| *index > m)
        && *content == contract_text(expected)
    {
        return (result, None);
    }
    let entry = ContractEntry {
        version: 1,
        authorization: expected.clone(),
        boundary: anchor::boundary(&result),
    };
    result.push(contract_message(expected));
    (result, Some(entry))
}

/// Whether the latest assistant message of `branch` has exactly one tool call, `id` of `name`.
pub fn standalone_call(branch: &[Value], id: &str, name: &str) -> bool {
    let Some(message) = branch.iter().rev().find_map(|entry| {
        let message = entry.get("message")?;
        (entry.get("type").and_then(Value::as_str) == Some("message")
            && message.get("role").and_then(Value::as_str) == Some("assistant"))
        .then_some(message)
    }) else {
        return false;
    };
    let calls: Vec<&Value> = message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
                .collect()
        })
        .unwrap_or_default();
    calls.len() == 1
        && calls[0].get("id").and_then(Value::as_str) == Some(id)
        && calls[0].get("name").and_then(Value::as_str) == Some(name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::contracts::{Mode, mode_contract_message};

    #[test]
    fn reconcile_appends_once_and_replays_at_the_boundary() {
        let messages = vec![
            mode_contract_message(Mode::Plan),
            json!({ "role": "user", "content": "plan it" }),
        ];
        let expected = Authorization::Managed {
            path: "/a/plan.md".into(),
        };
        let (first, entry) = reconcile(messages.clone(), &[], Some(&expected));
        let entry = entry.expect("first context records the contract");
        assert_eq!(first.len(), 3);
        let branch = vec![json!({ "type": "custom", "customType": CONTRACT_ENTRY,
            "data": serde_json::to_value(&entry).unwrap() })];
        let mut later = messages;
        later.push(json!({ "role": "assistant", "content": [] }));
        let (second, again) = reconcile(later, &branch, Some(&expected));
        assert!(again.is_none());
        assert_eq!(second.len(), 4);
        assert_eq!(second[2]["customType"], CONTRACT_ENTRY);
    }

    #[test]
    fn draft_lifecycle() {
        let agent = std::env::temp_dir().join(format!("plan-mode-draft-{}", crate::ids::uuid()));
        std::fs::create_dir_all(&agent).unwrap();
        let identity = DraftIdentity {
            version: 1,
            workflow_id: crate::ids::uuid(),
            session_id: "s".into(),
            status: "pending".into(),
        };
        let mut draft = Draft::default();
        assert!(draft.bind(&agent, &identity));
        let path = draft.path().unwrap().to_owned();
        assert!(
            draft
                .snapshot()
                .unwrap_err()
                .contains("Write the decision-ready")
        );
        assert!(draft.admit("c1", Some("/elsewhere.md")).is_err());
        draft.admit("c1", path.to_str()).unwrap();
        std::fs::write(&path, "# Plan\n").unwrap();
        assert!(
            draft
                .snapshot()
                .unwrap_err()
                .contains("Wait for draft writes")
        );
        draft.finish("c1");
        assert_eq!(draft.snapshot().unwrap(), "# Plan\n");
        let _ = std::fs::remove_dir_all(agent);
    }
}
