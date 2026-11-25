//! Plan tool policy: tool classification and the fail-closed Bash and PowerShell inspection
//! policies (upstream `tool-policy.ts`).

use std::collections::BTreeMap;

use serde_json::Value;

/// `safeSubcommands`: command prefixes mapped to the subcommand prefixes the user trusts.
pub type SafeSubcommands = BTreeMap<String, Vec<String>>;

pub const SAFE_BUILTIN_TOOLS: [&str; 6] = ["read", "bash", "powershell", "grep", "find", "ls"];
const BLOCKED_BUILTIN_TOOLS: [&str; 2] = ["edit", "write"];

const MUTATING_COMMANDS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "mkdir", "touch", "chmod", "chown", "chgrp", "ln", "tee",
    "truncate", "dd", "sudo", "su", "kill", "pkill", "killall", "reboot", "shutdown", "vim", "vi",
    "nano", "emacs", "code", "subl",
];
const READ_ONLY_POWERSHELL_COMMANDS: &[&str] = &[
    "format-list",
    "format-table",
    "get-childitem",
    "get-content",
    "get-item",
    "get-location",
    "measure-object",
    "out-string",
    "resolve-path",
    "select-string",
    "sort-object",
    "test-path",
    "write-output",
];
const READ_ONLY_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "grep", "find", "ls", "pwd", "echo", "printf", "wc", "sort", "uniq",
    "diff", "file", "stat", "du", "df", "tree", "which", "whereis", "type", "printenv", "uname",
    "whoami", "id", "date", "uptime", "ps", "jq", "rg", "fd", "bat", "eza",
];
/// `-i` edits in place for sed; for these inspection commands it is a case or inode flag.
const INSPECTION_CASE_FLAG_COMMANDS: &[&str] = &[
    "rg", "grep", "git", "diff", "fd", "ls", "sort", "find", "bat", "eza",
];
pub const BUILTIN_SAFE_GIT_SUBCOMMANDS: &[&str] = &[
    "status", "log", "diff", "show", "branch", "remote", "ls-files", "grep",
];
pub const CONFIGURABLE_SAFE_GIT_SUBCOMMANDS: &[&str] = &[
    "rev-parse",
    "blame",
    "describe",
    "merge-base",
    "ls-tree",
    "cat-file",
];
const SAFE_GH_SUBCOMMAND_PATHS: &[&str] = &["pr view", "pr list", "issue view", "issue list"];

/// A `tools.getAll` entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// `direct`, `model-only`, `codemode`, `deferred`, `hidden`; `None` on older hosts.
    pub exposure: Option<String>,
    pub source: Option<String>,
    pub path: Option<String>,
}

impl ToolInfo {
    pub fn from_value(value: &Value) -> Option<Self> {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
        let source = value.get("sourceInfo");
        Some(Self {
            name: text(value.get("name"))?,
            description: text(value.get("description")).unwrap_or_default(),
            exposure: text(value.get("exposure")),
            source: text(source.and_then(|s| s.get("source"))).filter(|s| !s.is_empty()),
            path: text(source.and_then(|s| s.get("path"))).filter(|s| !s.is_empty()),
        })
    }

    /// A core tool that owns `builtin:<name>`.
    pub fn has_native_source(&self) -> bool {
        self.source.as_deref() == Some("builtin")
            && self.path.as_deref().is_some_and(|path| {
                path == format!("builtin:{}", self.name)
                    || path == format!("<builtin:{}>", self.name)
            })
    }

    pub fn is_builtin(&self) -> bool {
        self.source.as_deref() == Some("builtin")
            && (self.path.is_none()
                || (self.has_native_source() && self.exposure.as_deref() != Some("model-only")))
    }

    pub fn classify(&self) -> Policy {
        if self.source.is_none() {
            return Policy::Blocked;
        }
        if !self.is_builtin() {
            return Policy::UserOptIn;
        }
        if BLOCKED_BUILTIN_TOOLS.contains(&self.name.as_str()) {
            return Policy::Blocked;
        }
        if self.name == "bash" || self.name == "powershell" {
            return Policy::Limited;
        }
        if SAFE_BUILTIN_TOOLS.contains(&self.name.as_str()) {
            Policy::ReadOnly
        } else {
            Policy::Blocked
        }
    }

    pub fn selectable(&self) -> bool {
        self.classify() != Policy::Blocked
    }

    /// Whether the tool can run on `route` given the active (declared) tool names.
    pub fn availability(&self, active: &[String], route: Route) -> Availability {
        let is_active = active.contains(&self.name);
        let declared = if is_active {
            Availability::Available
        } else {
            Availability::Inactive
        };
        match self.exposure.as_deref() {
            Some("hidden") => Availability::Hidden,
            Some("model-only") if route == Route::Nested => Availability::ModelOnly,
            Some("model-only") => declared,
            Some("codemode" | "deferred") if route != Route::Model => Availability::Available,
            Some("codemode" | "deferred") => declared,
            None | Some("direct") => declared,
            Some(_) => Availability::Unsupported,
        }
    }

    pub fn policy_label(&self) -> String {
        match self.classify() {
            Policy::ReadOnly => "built-in read-only".into(),
            Policy::Limited => "built-in limited".into(),
            Policy::Blocked if self.source.is_some() => "built-in blocked".into(),
            Policy::Blocked => "policy metadata unavailable".into(),
            Policy::UserOptIn => {
                let source = self.source.as_deref().unwrap_or("unknown");
                match &self.path {
                    Some(path) => format!("user opt-in: {source} {path}"),
                    None => format!("user opt-in: {source}"),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    ReadOnly,
    Limited,
    UserOptIn,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Selection,
    Model,
    Nested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    Inactive,
    Hidden,
    ModelOnly,
    Unsupported,
}

/// The automatic allowlist: the safe core tools among `tools`.
pub fn default_tool_names(tools: &[ToolInfo], configured: Option<&[String]>) -> Vec<String> {
    match configured {
        Some(names) => unique(names),
        None => tools
            .iter()
            .filter(|tool| tool.is_builtin() && SAFE_BUILTIN_TOOLS.contains(&tool.name.as_str()))
            .map(|tool| tool.name.clone())
            .collect(),
    }
}

pub fn unique(names: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for name in names {
        if !seen.contains(name) {
            seen.push(name.clone());
        }
    }
    seen
}

/// The command text of a `bash` or `powershell` call.
pub fn read_command(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
}

fn trusted_by_config(segment: &str, safe: &SafeSubcommands) -> bool {
    let candidate = segment.trim_start();
    safe.iter().any(|(command, subcommands)| {
        subcommands.iter().any(|subcommand| {
            let (command, subcommand) = (command.trim(), subcommand.trim());
            if command.is_empty() || subcommand.is_empty() {
                return false;
            }
            let prefix = format!("{command} {subcommand}");
            let Some(rest) = candidate.strip_prefix(&prefix) else {
                return false;
            };
            rest.chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || ";&|<>()".contains(c))
        })
    })
}

/// The first segment of a Bash command that the policy blocks, or the whole command when it
/// cannot be parsed. `None` means every segment is allowed.
pub fn blocked_bash_segment(
    command: &str,
    safe: &SafeSubcommands,
    cwd: Option<&str>,
    windows: bool,
) -> Option<String> {
    match split_shell_segments(command) {
        Some(segments) if !segments.is_empty() => segments
            .into_iter()
            .find(|segment| !is_safe_segment(segment, safe, cwd, windows)),
        _ => Some(empty_or(command)),
    }
}

/// Like [`blocked_bash_segment`] for the limited PowerShell policy.
pub fn blocked_powershell_segment(
    command: &str,
    safe: &SafeSubcommands,
    cwd: Option<&str>,
) -> Option<String> {
    match split_powershell_segments(command) {
        Some(segments) if !segments.is_empty() => segments
            .into_iter()
            .find(|segment| !is_safe_powershell_segment(segment, safe, cwd)),
        _ => Some(empty_or(command)),
    }
}

fn empty_or(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        "(empty command)".into()
    } else {
        trimmed.to_owned()
    }
}

fn push_segment(segments: &mut Vec<String>, text: &str) -> Option<()> {
    let segment = text.trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment.to_owned());
    Some(())
}

fn split_shell_segments(command: &str) -> Option<Vec<String>> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.contains(['\n', '\r', '`']) {
        return None;
    }
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut segments = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        i += 1;
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        if matches!(c, '>' | '<' | '(' | ')') {
            return None;
        }
        let next = chars.get(i).map(|(_, n)| *n);
        if c == '&' && next != Some('&') {
            return None;
        }
        let length = match c {
            ';' | '|' if next == Some(c) => 2,
            ';' | '|' => 1,
            '&' => 2,
            _ => 0,
        };
        if length == 0 {
            continue;
        }
        push_segment(&mut segments, &trimmed[start..at])?;
        i += length - 1;
        start = chars.get(i).map_or(trimmed.len(), |(at, _)| *at);
    }
    if quote.is_some() || escaped {
        return None;
    }
    push_segment(&mut segments, &trimmed[start..])?;
    Some(segments)
}

fn has_shell_expansion(segment: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in segment.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '$' && q == '"' {
                return true;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        if matches!(c, '$' | '*' | '?' | '[' | '{') {
            return true;
        }
    }
    false
}

/// `(^|\s)[A-Za-z_][A-Za-z0-9_]*=`: an environment assignment anywhere in the segment.
fn has_assignment(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    (0..chars.len()).any(|i| {
        if i > 0 && !chars[i - 1].is_whitespace() {
            return false;
        }
        if !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            return false;
        }
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        chars.get(j) == Some(&'=')
    })
}

fn shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in segment.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

fn is_safe_segment(
    segment: &str,
    safe: &SafeSubcommands,
    cwd: Option<&str>,
    windows: bool,
) -> bool {
    if trusted_by_config(segment, safe) {
        return true;
    }
    if has_shell_expansion(segment) || has_assignment(segment) {
        return false;
    }
    let Some(tokens) = shell_words(segment) else {
        return false;
    };
    let Some(first) = tokens.first() else {
        return false;
    };
    let command = first.to_lowercase();
    if MUTATING_COMMANDS.contains(&command.as_str()) {
        return false;
    }
    let args = &tokens[1..];
    if !has_safe_arguments(&command, args) {
        return false;
    }
    match command.as_str() {
        "hostname" => args.is_empty(),
        "tasklist" => windows && is_safe_tasklist_arguments(args),
        _ if READ_ONLY_COMMANDS.contains(&command.as_str()) => true,
        _ => is_safe_structured_command(&command, args, safe, cwd),
    }
}

fn eq_or_prefixed(arg: &str, flag: &str) -> bool {
    arg == flag || arg.starts_with(&format!("{flag}="))
}

fn has_safe_arguments(command: &str, args: &[String]) -> bool {
    let mut forbidden = vec!["--in-place", "--fix", "--write", "-delete", "--delete"];
    if !INSPECTION_CASE_FLAG_COMMANDS.contains(&command) {
        forbidden.push("-i");
    }
    let any = |test: &dyn Fn(&str) -> bool| args.iter().any(|a| test(a));
    if any(&|a| forbidden.contains(&a)) {
        return false;
    }
    // A short option cluster such as `-uo` that contains one of `letters`.
    let cluster = |a: &str, letters: &[char]| {
        a.starts_with('-') && !a.starts_with("--") && a[1..].contains(letters)
    };
    match command {
        "sed" => args.iter().filter(|a| a.starts_with('-')).all(|a| {
            ["--quiet", "--silent", "--regexp-extended"].contains(&a.as_str())
                || (a.len() > 1 && a[1..].chars().all(|c| matches!(c, 'n' | 'E' | 'r')))
        }),
        "find" => !any(&|a| {
            [
                "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
            ]
            .contains(&a)
        }),
        "date" => !any(&|a| a == "-s" || a.starts_with("--set")),
        "sort" | "tree" => {
            let output = |a: &str| cluster(a, &['o']) || long_option(a, "--output", "--o");
            let temp = |a: &str| {
                cluster(a, &['T'])
                    || long_option(a, "--temporary-directory", "--t")
                    || long_option(a, "--compress-program", "--com")
            };
            !any(&|a| output(a) || (command == "sort" && temp(a)))
        }
        "diff" => !any(&|a| eq_or_prefixed(a, "--output")),
        "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        "fd" => !any(&|a| {
            cluster(a, &['x', 'X'])
                || ["--exec", "--exec-batch"]
                    .iter()
                    .any(|f| eq_or_prefixed(a, f))
        }),
        "rg" => !any(&|a| eq_or_prefixed(a, "--pre")),
        "bat" => !any(&|a| eq_or_prefixed(a, "--pager")),
        _ => true,
    }
}

fn is_safe_structured_command(
    command: &str,
    args: &[String],
    safe: &SafeSubcommands,
    cwd: Option<&str>,
) -> bool {
    match command {
        "git" => return is_safe_git_command(args, safe, cwd),
        "gh" => return is_safe_gh_command(args, safe),
        _ => {}
    }
    let subcommand_index = args.iter().position(|a| !a.starts_with('-'));
    let subcommand = subcommand_index.map(|i| args[i].to_lowercase());
    let subcommand = subcommand.as_deref().unwrap_or("");
    let subcommand_args = subcommand_index.map_or(&[][..], |i| &args[i + 1..]);
    match command {
        "sed" => {
            let script = args.iter().find(|a| !a.starts_with('-'));
            let quiet = args.iter().any(|a| {
                ["--quiet", "--silent"].contains(&a.as_str())
                    || (a.starts_with('-') && !a[1..].contains('-') && a[1..].contains('n'))
            });
            script.is_some_and(|s| is_print_range(s)) && quiet
        }
        "node" | "python" | "python3" | "tsc" | "biome" | "ruff" | "ty" => {
            if args.len() == 1 && args[0] == "--version" {
                return true;
            }
            command == "tsc"
                && args.iter().any(|a| a == "--noEmit")
                && !args.iter().any(|a| {
                    ["--incremental", "--tsBuildInfoFile", "--generateTrace"]
                        .iter()
                        .any(|f| eq_or_prefixed(a, f))
                })
        }
        "npm" => {
            if subcommand == "audit" && subcommand_args.iter().any(|a| a == "fix") {
                return false;
            }
            if [
                "list", "ls", "view", "info", "search", "outdated", "audit", "test",
            ]
            .contains(&subcommand)
            {
                return true;
            }
            subcommand == "run"
                && args
                    .get(1)
                    .is_some_and(|a| ["test", "check", "typecheck", "lint"].contains(&a.as_str()))
        }
        "cargo" | "go" | "pytest" | "vitest" | "jest" => {
            ["test", "check"].contains(&subcommand)
                || ["pytest", "vitest", "jest"].contains(&command)
        }
        _ => false,
    }
}

/// `^\d+(,\d+)?p$`
fn is_print_range(script: &str) -> bool {
    let Some(range) = script.strip_suffix('p') else {
        return false;
    };
    let mut parts = range.splitn(2, ',');
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits)
}

fn is_safe_tasklist_arguments(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_lowercase();
        match arg.as_str() {
            "/v" | "/nh" | "/svc" => {}
            "/fo" => {
                let format = args.get(i + 1).map(|f| f.to_lowercase());
                if !format.is_some_and(|f| ["table", "list", "csv"].contains(&f.as_str())) {
                    return false;
                }
                i += 1;
            }
            "/fi" => {
                if args.get(i + 1).is_none_or(|f| f.starts_with('/')) {
                    return false;
                }
                i += 1;
            }
            "/m" => {
                if args.get(i + 1).is_some_and(|m| !m.starts_with('/')) {
                    i += 1;
                }
            }
            _ => return false,
        }
        i += 1;
    }
    true
}

fn is_safe_powershell_query(args: &[String], switches: &[&str], values: &[&str]) -> bool {
    let mut positional = 0;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let lower = arg.to_lowercase();
        if arg.is_empty() {
            return false;
        }
        if !arg.starts_with('-') {
            positional += 1;
            if positional > 1 {
                return false;
            }
        } else if !switches.contains(&lower.as_str()) {
            if !values.contains(&lower.as_str()) {
                return false;
            }
            if args
                .get(i + 1)
                .is_none_or(|v| v.is_empty() || v.starts_with('-'))
            {
                return false;
            }
            i += 1;
        }
        i += 1;
    }
    true
}

fn is_safe_git_command(args: &[String], safe: &SafeSubcommands, cwd: Option<&str>) -> bool {
    let Some(index) = git_subcommand_index(args, cwd) else {
        return false;
    };
    let Some(subcommand) = args.get(index).map(|s| s.to_lowercase()) else {
        return false;
    };
    if subcommand.starts_with('-') {
        return false;
    }
    let rest = &args[index + 1..];
    let configured = safe
        .get("git")
        .is_some_and(|subs| subs.contains(&subcommand));
    let validator: fn(&[String]) -> bool = match subcommand.as_str() {
        "branch" => is_safe_git_branch,
        "remote" => is_safe_git_remote,
        "grep" => is_safe_git_grep,
        s if BUILTIN_SAFE_GIT_SUBCOMMANDS.contains(&s) => |_| true,
        "cat-file" if configured => is_safe_git_cat_file,
        s if configured && CONFIGURABLE_SAFE_GIT_SUBCOMMANDS.contains(&s) => |_| true,
        _ => return false,
    };
    has_safe_git_arguments(&subcommand, rest) && validator(rest)
}

fn git_subcommand_index(args: &[String], cwd: Option<&str>) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-pager" | "--no-optional-locks" => i += 1,
            "-C" => {
                let directory = args.get(i + 1)?;
                if !is_current_directory(directory, cwd?) {
                    return None;
                }
                i += 2;
            }
            _ => break,
        }
    }
    Some(i)
}

/// Collapses repeated separators and `.` segments, keeping a leading and a trailing separator.
fn normalize_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let trailing = path.len() > 1 && path.ends_with('/');
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    out.push_str(&parts.join("/"));
    if out.is_empty() {
        out.push('.');
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn is_current_directory(directory: &str, cwd: &str) -> bool {
    if directory.split(['/', '\\']).any(|part| part == "..") {
        return false;
    }
    if directory == "." {
        return true;
    }
    std::path::Path::new(directory).is_absolute()
        && normalize_path(directory) == normalize_path(cwd)
}

fn has_safe_git_arguments(subcommand: &str, args: &[String]) -> bool {
    !args.iter().any(|a| {
        a == "--help"
            || eq_or_prefixed(a, "--show-signature")
            || a.contains("%G")
            || eq_or_prefixed(a, "--output")
            || eq_or_prefixed(a, "--ext-diff")
            || eq_or_prefixed(a, "--textconv")
            || a == "--paginate"
            || eq_or_prefixed(a, "--open-files-in-pager")
            || (subcommand == "grep"
                && a.starts_with('-')
                && !a.starts_with("--")
                && a.contains('O'))
    })
}

/// A possibly abbreviated long option: `--fi` and longer prefixes of `--filters`.
fn long_option(arg: &str, option: &str, shortest: &str) -> bool {
    let name = arg.split('=').next().unwrap_or("");
    name.len() >= shortest.len() && option.starts_with(name)
}

fn is_safe_git_cat_file(args: &[String]) -> bool {
    !args
        .iter()
        .any(|a| long_option(a, "--filters", "--fi") || long_option(a, "--textconv", "--t"))
}

fn is_safe_git_grep(args: &[String]) -> bool {
    !args.iter().any(|a| {
        long_option(a, "--textconv", "--textc")
            || long_option(a, "--open-files-in-pager", "--op")
            || long_option(a, "--ext-grep", "--ext")
    })
}

fn is_safe_git_branch(args: &[String]) -> bool {
    if args.iter().any(|a| !a.starts_with('-')) {
        return false;
    }
    !args.iter().any(|a| {
        let cluster = a[1..].split('-').next().unwrap_or("");
        cluster.contains(['d', 'D', 'm', 'M', 'c', 'C', 'u'])
            || long_option(a, "--delete", "--del")
            || long_option(a, "--move", "--mov")
            || long_option(a, "--copy", "--cop")
            || long_option(a, "--edit-description", "--e")
            || long_option(a, "--unset-upstream", "--u")
            || long_option(a, "--set-upstream-to", "--set-u")
            || long_option(a, "--create-reflog", "--creat")
    })
}

fn is_safe_git_remote(args: &[String]) -> bool {
    let Some(index) = args.iter().position(|a| !a.starts_with('-')) else {
        return true;
    };
    match args[index].as_str() {
        "get-url" => true,
        "show" => {
            let show = &args[index + 1..];
            if show.iter().any(|a| a == "--") {
                return false;
            }
            let remotes = show.iter().filter(|a| !a.starts_with('-')).count();
            remotes == 0 || (remotes == 1 && show.iter().any(|a| a == "-n"))
        }
        _ => false,
    }
}

fn is_safe_gh_command(args: &[String], safe: &SafeSubcommands) -> bool {
    let (Some(group), Some(action)) = (args.first(), args.get(1)) else {
        return false;
    };
    let (group, action) = (group.to_lowercase(), action.to_lowercase());
    if group.starts_with('-') || action.starts_with('-') {
        return false;
    }
    let path = format!("{group} {action}");
    let configured = safe.get("gh").is_some_and(|paths| paths.contains(&path));
    configured && SAFE_GH_SUBCOMMAND_PATHS.contains(&path.as_str()) && is_safe_gh_read(&args[2..])
}

fn is_safe_gh_read(args: &[String]) -> bool {
    let unsafe_arg = |a: &String| {
        a.starts_with("-w")
            || eq_or_prefixed(a, "--web")
            || eq_or_prefixed(a, "--browser")
            || a == "--paginate"
            || eq_or_prefixed(a, "--pager")
            || eq_or_prefixed(a, "--output")
    };
    if args.iter().any(unsafe_arg) {
        return false;
    }
    let mut has_json = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--json" {
            if args
                .get(i + 1)
                .is_none_or(|v| v.is_empty() || v.starts_with('-'))
            {
                return false;
            }
            has_json = true;
            i += 1;
        } else if let Some(value) = arg.strip_prefix("--json=") {
            if value.is_empty() {
                return false;
            }
            has_json = true;
        }
        i += 1;
    }
    has_json
}

fn split_powershell_segments(command: &str) -> Option<Vec<String>> {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | '`' | '\u{2018}'..='\u{201e}'))
    {
        return None;
    }
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut segments = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|(_, n)| *n);
        i += 1;
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if next == Some('\'') {
                        i += 1;
                    } else {
                        quote = None;
                    }
                }
                continue;
            }
            Some(_) => {
                if c == '$' || c == '`' {
                    return None;
                }
                if c == '"' {
                    quote = None;
                }
                continue;
            }
            None => {}
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        if "$@#!?{}()[]><".contains(c) || c == '&' || (c == '|' && next == Some('|')) {
            return None;
        }
        if c != ';' && c != '|' {
            continue;
        }
        push_segment(&mut segments, &trimmed[start..at])?;
        start = at + 1;
    }
    if quote.is_some() {
        return None;
    }
    push_segment(&mut segments, &trimmed[start..])?;
    Some(segments)
}

fn powershell_words(segment: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = segment.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match quote {
            Some('\'') => {
                if c != '\'' {
                    word.push(c);
                } else if chars.get(i) == Some(&'\'') {
                    word.push('\'');
                    i += 1;
                } else {
                    quote = None;
                }
                continue;
            }
            Some(_) => {
                if c == '"' {
                    quote = None;
                } else {
                    word.push(c);
                }
                continue;
            }
            None => {}
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            has_word = true;
        } else if c.is_whitespace() {
            if has_word {
                words.push(std::mem::take(&mut word));
            }
            has_word = false;
        } else {
            word.push(c);
            has_word = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_word {
        words.push(word);
    }
    Some(words)
}

fn is_safe_powershell_segment(segment: &str, safe: &SafeSubcommands, cwd: Option<&str>) -> bool {
    if trusted_by_config(segment, safe) {
        return true;
    }
    let Some(tokens) = powershell_words(segment) else {
        return false;
    };
    if tokens.is_empty() || tokens.iter().any(|t| t == "--%") {
        return false;
    }
    let command = tokens[0].to_lowercase();
    if command.is_empty() {
        return false;
    }
    let args = &tokens[1..];
    match command.as_str() {
        c if READ_ONLY_POWERSHELL_COMMANDS.contains(&c) => true,
        "get-process" => is_safe_powershell_query(
            args,
            &["-module", "-fileversioninfo", "-includeusername"],
            &["-name", "-id"],
        ),
        "get-service" => is_safe_powershell_query(
            args,
            &["-dependentservices", "-requiredservices"],
            &["-name", "-displayname", "-include", "-exclude"],
        ),
        "git" | "gh" => is_safe_structured_command(&command, args, safe, cwd),
        _ => false,
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
