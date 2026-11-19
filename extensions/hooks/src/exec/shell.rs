//! The program that runs a shell-form `command` hook.
//!
//! Claude Code and Codex hooks are POSIX shell. On Unix they run with `sh -c` (Codex: `$SHELL -lc`).
//! On Windows they run with Git Bash, as Claude Code does, so the same hook works on every platform.
//! `shell: "powershell"` runs with PowerShell on Windows. When no bash is found, the hook falls back
//! to `cmd /d /s /c`, with a warning.
//!
//! The Windows lookup is a pure function of [`Lookup`], so it is tested on every platform.

use base64::Engine as _;

/// The interpreter a hook asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellKind {
    /// `sh -c` on Unix, `bash -c` on Windows.
    #[default]
    Posix,
    /// Codex: `$SHELL -lc` (else `/bin/sh -lc`) on Unix, `bash -lc` on Windows.
    Login,
    /// `shell: "powershell"`: Windows only.
    PowerShell,
}

/// A resolved program; the hook line is the last of `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Program {
        program: String,
        args: Vec<String>,
        /// Extra environment for the program.
        env: Vec<(String, String)>,
    },
    /// `cmd /d /s /c "<line>"`; needs `raw_arg`, so the caller builds it.
    Cmd { reason: String },
}

/// Filesystem and environment reads, injectable for tests.
pub trait Lookup {
    fn exists(&self, path: &str) -> bool;
    fn var(&self, name: &str) -> Option<String>;
}

pub struct System;

impl Lookup for System {
    fn exists(&self, path: &str) -> bool {
        let path = std::path::Path::new(path);
        // Windows Store aliases are reparse points that `metadata()` cannot follow.
        path.is_file() || std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
    }

    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }
}

/// `-NoProfile -NonInteractive -ExecutionPolicy Bypass`, then `-EncodedCommand`, so quotes in the
/// hook line survive both PowerShell 5.1 and 7 argument parsing. Stdin stays free for the event JSON.
const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-EncodedCommand",
];

/// The Windows program for `line`. `shell_path` is pi's `shellPath` setting.
pub fn windows_launch(
    kind: ShellKind,
    line: &str,
    shell_path: Option<&str>,
    lookup: &dyn Lookup,
) -> Launch {
    if kind == ShellKind::PowerShell {
        return match find_powershell(lookup) {
            Some(program) => {
                let mut args: Vec<String> =
                    POWERSHELL_ARGS.iter().map(|a| (*a).to_owned()).collect();
                args.push(encode_powershell(line));
                Launch::Program {
                    program,
                    args,
                    env: Vec::new(),
                }
            }
            None => Launch::Cmd {
                reason: "no PowerShell found (pwsh.exe or powershell.exe)".into(),
            },
        };
    }
    match find_bash(shell_path, lookup) {
        Some(program) => {
            let flag = if kind == ShellKind::Login {
                "-lc"
            } else {
                "-c"
            };
            Launch::Program {
                program,
                args: vec![flag.into(), line.into()],
                // A login shell of MSYS2 changes to `$HOME` unless this is set; keep the session cwd.
                env: vec![("CHERE_INVOKING".into(), "1".into())],
            }
        }
        None => Launch::Cmd {
            reason: "no bash found; install Git for Windows, add bash.exe to PATH, \
                     or set shellPath in pi settings or CLAUDE_CODE_GIT_BASH_PATH"
                .into(),
        },
    }
}

/// pi's `shellPath`, `CLAUDE_CODE_GIT_BASH_PATH`, Git for Windows (machine and per-user installs),
/// `bash.exe` next to `git.exe` on `PATH`, then `bash.exe` on `PATH`. The WSL launcher
/// (`System32\bash.exe` and the `WindowsApps` alias) runs Linux, not the Windows session, so it is skipped.
pub fn find_bash(shell_path: Option<&str>, lookup: &dyn Lookup) -> Option<String> {
    let configured = [
        shell_path.map(|p| expand_home(p, lookup)),
        lookup.var("CLAUDE_CODE_GIT_BASH_PATH"),
    ];
    if let Some(found) = configured
        .into_iter()
        .flatten()
        .map(|p| p.trim().trim_matches('"').to_owned())
        .find(|p| !p.is_empty() && lookup.exists(p))
    {
        return Some(found);
    }
    let mut candidates = Vec::new();
    for (var, tail) in [
        ("ProgramFiles", r"Git\bin\bash.exe"),
        ("ProgramW6432", r"Git\bin\bash.exe"),
        ("ProgramFiles(x86)", r"Git\bin\bash.exe"),
        ("LOCALAPPDATA", r"Programs\Git\bin\bash.exe"),
    ] {
        if let Some(base) = lookup.var(var) {
            candidates.push(join(&base, tail));
        }
    }
    // `<git>\cmd\git.exe` or `<git>\bin\git.exe` -> `<git>\bin\bash.exe` (Scoop, custom prefixes).
    if let Some(git) = on_path("git.exe", lookup)
        && let Some(root) = parent(&git).and_then(parent)
    {
        candidates.push(join(root, r"bin\bash.exe"));
    }
    if let Some(found) = candidates.into_iter().find(|p| lookup.exists(p)) {
        return Some(found);
    }
    path_entries(lookup)
        .map(|dir| join(&dir, "bash.exe"))
        .filter(|p| !is_wsl_launcher(p, lookup))
        .find(|p| lookup.exists(p))
}

/// `pwsh.exe` on `PATH`, `%ProgramFiles%\PowerShell\7\pwsh.exe`, `powershell.exe` on `PATH`, then
/// `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`.
pub fn find_powershell(lookup: &dyn Lookup) -> Option<String> {
    let under = |var: &str, tail: &str| {
        lookup
            .var(var)
            .map(|base| join(&base, tail))
            .filter(|p| lookup.exists(p))
    };
    on_path("pwsh.exe", lookup)
        .or_else(|| under("ProgramFiles", r"PowerShell\7\pwsh.exe"))
        .or_else(|| on_path("powershell.exe", lookup))
        .or_else(|| {
            under(
                "SystemRoot",
                r"System32\WindowsPowerShell\v1.0\powershell.exe",
            )
        })
}

/// Base64 of the UTF-16LE text, as `-EncodedCommand` expects.
pub fn encode_powershell(line: &str) -> String {
    let bytes: Vec<u8> = line.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn is_wsl_launcher(path: &str, lookup: &dyn Lookup) -> bool {
    let lower = path.replace('/', "\\").to_lowercase();
    if lower.contains(r"\windowsapps\") {
        return true;
    }
    let root = lookup
        .var("SystemRoot")
        .unwrap_or_else(|| r"C:\Windows".into())
        .replace('/', "\\")
        .to_lowercase();
    let root = root.trim_end_matches('\\');
    ["system32", "sysnative"]
        .iter()
        .any(|dir| lower == format!(r"{root}\{dir}\bash.exe"))
}

fn on_path(exe: &str, lookup: &dyn Lookup) -> Option<String> {
    path_entries(lookup)
        .map(|dir| join(&dir, exe))
        .find(|p| lookup.exists(p))
}

fn path_entries(lookup: &dyn Lookup) -> impl Iterator<Item = String> {
    let path = lookup
        .var("PATH")
        .or_else(|| lookup.var("Path"))
        .unwrap_or_default();
    path.split(';')
        .map(|dir| dir.trim().trim_matches('"').to_owned())
        .filter(|dir| !dir.is_empty())
        .collect::<Vec<_>>()
        .into_iter()
}

fn join(base: &str, tail: &str) -> String {
    format!("{}\\{tail}", base.trim_end_matches(['\\', '/']))
}

fn parent(path: &str) -> Option<&str> {
    let path = path.trim_end_matches(['\\', '/']);
    path.rfind(['\\', '/']).map(|i| &path[..i])
}

fn expand_home(path: &str, lookup: &dyn Lookup) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            match lookup.var("USERPROFILE").or_else(|| lookup.var("HOME")) {
                Some(home) => format!("{}{rest}", home.trim_end_matches(['\\', '/'])),
                None => path.to_owned(),
            }
        }
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Default)]
    struct Fake {
        files: HashSet<String>,
        vars: HashMap<String, String>,
    }

    impl Fake {
        fn file(mut self, path: &str) -> Self {
            self.files.insert(path.to_owned());
            self
        }
        fn var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_owned(), value.to_owned());
            self
        }
        fn windows() -> Self {
            Self::default()
                .var("ProgramFiles", r"C:\Program Files")
                .var("ProgramFiles(x86)", r"C:\Program Files (x86)")
                .var("LOCALAPPDATA", r"C:\Users\me\AppData\Local")
                .var("SystemRoot", r"C:\Windows")
                .var("USERPROFILE", r"C:\Users\me")
        }
    }

    impl Lookup for Fake {
        fn exists(&self, path: &str) -> bool {
            self.files.contains(path)
        }
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
    }

    const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";
    const PASEO: &str =
        r#"if [ -n "$PASEO_TERMINAL_ID" ]; then "${PASEO_HOOK_CLI:-paseo}" hooks claude Stop; fi"#;

    #[test]
    fn posix_hooks_run_with_git_bash() {
        let fake = Fake::windows().file(GIT_BASH);
        assert_eq!(
            windows_launch(ShellKind::Posix, PASEO, None, &fake),
            Launch::Program {
                program: GIT_BASH.into(),
                args: vec!["-c".into(), PASEO.into()],
                env: vec![("CHERE_INVOKING".into(), "1".into())],
            }
        );
    }

    #[test]
    fn codex_hooks_get_a_login_bash() {
        let fake = Fake::windows().file(GIT_BASH);
        let Launch::Program { args, .. } = windows_launch(ShellKind::Login, "true", None, &fake)
        else {
            panic!("expected bash");
        };
        assert_eq!(args[0], "-lc");
    }

    #[test]
    fn shell_path_setting_wins() {
        let fake = Fake::windows()
            .file(GIT_BASH)
            .file(r"C:\Users\me\msys64\usr\bin\bash.exe")
            .file(r"D:\tools\bash.exe")
            .var("CLAUDE_CODE_GIT_BASH_PATH", r"D:\tools\bash.exe");
        assert_eq!(
            find_bash(Some(r"~\msys64\usr\bin\bash.exe"), &fake).as_deref(),
            Some(r"C:\Users\me\msys64\usr\bin\bash.exe")
        );
        assert_eq!(
            find_bash(None, &fake).as_deref(),
            Some(r"D:\tools\bash.exe")
        );
    }

    #[test]
    fn missing_configured_paths_fall_through() {
        let fake = Fake::windows()
            .file(GIT_BASH)
            .var("CLAUDE_CODE_GIT_BASH_PATH", r"D:\gone\bash.exe");
        assert_eq!(
            find_bash(Some(r"E:\gone.exe"), &fake).as_deref(),
            Some(GIT_BASH)
        );
    }

    #[test]
    fn per_user_git_install_is_found() {
        let user = r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe";
        let fake = Fake::windows().file(user);
        assert_eq!(find_bash(None, &fake).as_deref(), Some(user));
    }

    #[test]
    fn bash_next_to_git_on_path() {
        let fake = Fake::windows()
            .var("PATH", r"C:\Windows\System32;D:\scoop\apps\git\current\cmd")
            .file(r"D:\scoop\apps\git\current\cmd\git.exe")
            .file(r"D:\scoop\apps\git\current\bin\bash.exe");
        assert_eq!(
            find_bash(None, &fake).as_deref(),
            Some(r"D:\scoop\apps\git\current\bin\bash.exe")
        );
    }

    #[test]
    fn wsl_launchers_are_skipped() {
        let fake = Fake::windows()
            .var(
                "PATH",
                r"C:\Windows\system32;C:\Users\me\AppData\Local\Microsoft\WindowsApps;C:\msys64\usr\bin",
            )
            .file(r"C:\Windows\system32\bash.exe")
            .file(r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\bash.exe")
            .file(r"C:\msys64\usr\bin\bash.exe");
        assert_eq!(
            find_bash(None, &fake).as_deref(),
            Some(r"C:\msys64\usr\bin\bash.exe")
        );
        let only_wsl = Fake::windows()
            .var("PATH", r"C:\Windows\System32")
            .file(r"C:\Windows\System32\bash.exe");
        assert_eq!(find_bash(None, &only_wsl), None);
    }

    #[test]
    fn no_bash_falls_back_to_cmd() {
        assert!(matches!(
            windows_launch(ShellKind::Posix, "echo hi", None, &Fake::windows()),
            Launch::Cmd { .. }
        ));
    }

    #[test]
    fn powershell_prefers_pwsh_and_encodes_the_line() {
        let ps51 = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let fake = Fake::windows().file(ps51);
        let Launch::Program { program, args, .. } =
            windows_launch(ShellKind::PowerShell, "Write-Output 'a \"b\"'", None, &fake)
        else {
            panic!("expected PowerShell");
        };
        assert_eq!(program, ps51);
        assert_eq!(args[..5], POWERSHELL_ARGS);
        assert_eq!(args[5], encode_powershell("Write-Output 'a \"b\"'"));
        let fake = fake.file(pwsh);
        let Launch::Program { program, .. } =
            windows_launch(ShellKind::PowerShell, "1", None, &fake)
        else {
            panic!("expected PowerShell");
        };
        assert_eq!(program, pwsh);
    }

    #[test]
    fn encoded_command_is_utf16le_base64() {
        // `[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('dir'))`
        assert_eq!(encode_powershell("dir"), "ZABpAHIA");
    }
}
