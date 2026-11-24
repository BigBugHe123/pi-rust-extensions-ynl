//! `/plan export`: writes the stored plan to a new Markdown file (upstream `plan-export.ts`).

use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Resolves `requested` (or `default`) against `cwd`. A leading `@` is dropped.
pub fn resolve(requested: Option<&str>, cwd: &Path, default: &str) -> Result<PathBuf, String> {
    let raw = requested
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(default);
    let path = raw.strip_prefix('@').unwrap_or(raw);
    if path.trim().is_empty() {
        return Err("Plan export path must not be empty.".into());
    }
    if path.contains('\0') {
        return Err("Plan export path must not contain NUL bytes.".into());
    }
    let joined = cwd.join(path);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    Ok(normalized)
}

/// Writes `plan` plus one trailing newline to a new file; existing paths are never replaced.
pub fn write(plan: &str, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "Plan export target already exists: {}. Choose another path or remove it first.",
                    path.display()
                )
            } else {
                e.to_string()
            }
        })?;
    let text = if plan.ends_with('\n') {
        plan.to_owned()
    } else {
        format!("{plan}\n")
    };
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("plan-mode-export-{}", crate::ids::uuid()));
        let path = resolve(Some("@docs/../out/PLAN.md"), &dir, "PLAN.md").unwrap();
        assert_eq!(path, dir.join("out/PLAN.md"));
        assert_eq!(resolve(None, &dir, "PLAN.md").unwrap(), dir.join("PLAN.md"));
        write("# Plan", &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n");
        assert!(
            write("# Other", &path)
                .unwrap_err()
                .contains("already exists")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
