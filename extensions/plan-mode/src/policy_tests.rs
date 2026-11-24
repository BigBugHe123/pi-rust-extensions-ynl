use serde_json::json;

use super::*;

const CWD: &str = "/work/repo";

fn bash(command: &str) -> Option<String> {
    blocked_bash_segment(command, &SafeSubcommands::new(), Some(CWD), false)
}

fn bash_with(command: &str, safe: &[(&str, &[&str])]) -> Option<String> {
    let safe: SafeSubcommands = safe
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
        .collect();
    blocked_bash_segment(command, &safe, Some(CWD), false)
}

fn ps(command: &str) -> Option<String> {
    blocked_powershell_segment(command, &SafeSubcommands::new(), Some(CWD))
}

#[test]
fn accepts_inspection_commands_and_chains() {
    for command in [
        "ls -la",
        "cat README.md | head -n 20",
        "rg -i todo src && git status --short",
        "grep -i foo a.txt; wc -l a.txt",
        "git --no-optional-locks status --short",
        "git -C . log --oneline -5",
        "git -C /work/repo diff",
        "git branch -a",
        "git remote -v",
        "git remote show -n origin",
        "sed -n 1,20p file.txt",
        "npm test",
        "npm run typecheck",
        "cargo test -p foo",
        "tsc --noEmit",
        "node --version",
        "hostname",
        "echo 'a > b'",
        "find . -name '*.rs'",
    ] {
        assert_eq!(bash(command), None, "{command}");
    }
}

#[test]
fn blocks_mutation_redirects_and_expansion() {
    for (command, blocked) in [
        ("rm -rf build", "rm -rf build"),
        ("ls && rm a", "rm a"),
        ("echo hi > out.txt", "echo hi > out.txt"),
        ("cat $(ls)", "cat $(ls)"),
        ("cat `ls`", "cat `ls`"),
        ("ls *", "ls *"),
        ("FOO=1 ls", "FOO=1 ls"),
        ("sed -i s/a/b/ f", "sed -i s/a/b/ f"),
        ("sed -ni 1p f", "sed -ni 1p f"),
        ("find . -delete", "find . -delete"),
        ("find . -exec rm {} ;", "find . -exec rm {} ;"),
        ("sort -o out a", "sort -o out a"),
        ("npm install", "npm install"),
        ("npm audit fix", "npm audit fix"),
        ("git commit -m x", "git commit -m x"),
        ("git branch -D main", "git branch -D main"),
        ("git branch feature", "git branch feature"),
        ("git -C ../other status", "git -C ../other status"),
        ("git -C /tmp status", "git -C /tmp status"),
        ("git log --output=x", "git log --output=x"),
        ("git rev-parse HEAD", "git rev-parse HEAD"),
        ("sleep 1 &", "sleep 1 &"),
        ("ls\nrm a", "ls\nrm a"),
        ("hostname newname", "hostname newname"),
        ("rg --pre cat x", "rg --pre cat x"),
        ("uniq a b", "uniq a b"),
        ("", "(empty command)"),
        ("ls ;", "ls ;"),
    ] {
        assert_eq!(bash(command).as_deref(), Some(blocked), "{command}");
    }
}

#[test]
fn blocks_code_execution_and_output_flags_that_upstream_allows() {
    for command in [
        "python3 -c \"import os\" --version",
        "node x.js --version",
        "fd -Hx rm",
        "git grep -lOcat pattern",
        "sed -n '-ew out' 1p f",
        "sort --o=out a",
        "sort -uo out a",
        "sort -uT /tmp a",
        "tree -ao out",
    ] {
        assert!(bash(command).is_some(), "{command}");
    }
    for command in [
        "node --version",
        "sed -nE 1,5p f",
        "sed --quiet 2p f",
        "sort -u a",
        "fd -H rs",
    ] {
        assert_eq!(bash(command), None, "{command}");
    }
}

#[test]
fn configured_subcommands_trust_only_their_segment() {
    let safe: &[(&str, &[&str])] = &[("kubectl", &["apply"]), ("git", &["rev-parse"])];
    assert_eq!(
        bash_with("git status && kubectl apply -f d.yaml", safe),
        None
    );
    assert_eq!(
        bash_with("kubectl apply -f d.yaml && rm -rf build", safe).as_deref(),
        Some("rm -rf build")
    );
    assert!(bash_with("kubectl applies", safe).is_some());
    assert_eq!(bash_with("git rev-parse HEAD", safe), None);
    assert!(bash_with("kubectl apply > out", safe).is_some());
}

#[test]
fn gh_needs_configuration_and_json_output() {
    let safe: &[(&str, &[&str])] = &[("gh", &["pr view"])];
    assert!(bash("gh pr view 1 --json title").is_some());
    assert_eq!(
        bash_with("gh pr view 1 --web", safe),
        None,
        "the prefix trusts the segment"
    );
    // Without the literal prefix (case differs), the reviewed gh validator applies.
    assert_eq!(bash_with("gh PR view 1 --json title", safe), None);
    assert!(bash_with("gh PR view 1", safe).is_some());
    assert!(bash_with("gh PR view 1 --json title --web", safe).is_some());
    assert!(bash_with("gh pr list --json title", safe).is_some());
}

#[test]
fn tasklist_needs_windows() {
    let safe = SafeSubcommands::new();
    assert!(blocked_bash_segment("tasklist /v", &safe, Some(CWD), false).is_some());
    assert_eq!(
        blocked_bash_segment("tasklist /v /fo csv", &safe, Some(CWD), true),
        None
    );
    assert!(blocked_bash_segment("tasklist /s host", &safe, Some(CWD), true).is_some());
}

#[test]
fn powershell_policy() {
    for command in [
        "Get-ChildItem -Recurse",
        "Get-Content README.md | Select-String foo",
        "Get-Process -Name pwsh",
        "git status; Get-Location",
        "Write-Output 'it''s'",
    ] {
        assert_eq!(ps(command), None, "{command}");
    }
    for command in [
        "Remove-Item x",
        "Get-Content $env:HOME",
        "gci",
        "Get-Process -ComputerName x",
        "Get-ChildItem && ls",
        "Get-Content x > y",
        "Get-Content \"$x\"",
        "Get-ChildItem --% /s",
    ] {
        assert!(ps(command).is_some(), "{command}");
    }
}

fn tool(name: &str, source: &str, path: &str, exposure: &str) -> ToolInfo {
    ToolInfo::from_value(&json!({
        "name": name, "description": "d", "exposure": exposure,
        "sourceInfo": { "source": source, "path": path }
    }))
    .unwrap()
}

#[test]
fn classifies_tools() {
    let read = tool("read", "builtin", "builtin:read", "direct");
    assert_eq!(read.classify(), Policy::ReadOnly);
    assert_eq!(
        tool("bash", "builtin", "builtin:bash", "direct").classify(),
        Policy::Limited
    );
    assert_eq!(
        tool("edit", "builtin", "builtin:edit", "direct").classify(),
        Policy::Blocked
    );
    assert_eq!(
        tool("mcp__docs__read", "builtin", "builtin:mcp", "direct").classify(),
        Policy::UserOptIn
    );
    assert_eq!(
        tool("codemode", "builtin", "builtin:codemode", "model-only").classify(),
        Policy::UserOptIn
    );
    assert_eq!(
        tool("read", "local", "/x/ext.ts", "direct").classify(),
        Policy::UserOptIn
    );
    let deferred = tool("docs", "local", "/x", "deferred");
    let active: Vec<String> = Vec::new();
    assert_eq!(
        deferred.availability(&active, Route::Nested),
        Availability::Available
    );
    assert_eq!(
        deferred.availability(&active, Route::Model),
        Availability::Inactive
    );
    assert_eq!(
        read.availability(&["read".into()], Route::Model),
        Availability::Available
    );
}
