use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// Age after which a temp file counts as left by a writer killed before its rename. A live write
/// takes far less.
const STALE_TMP: Duration = Duration::from_secs(60);

/// `create_dir_all`, with mode 0700 for the directories it creates on unix.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Writes `bytes` to a new sibling temp file with mode 0600, then renames it over `path`.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(parent)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    remove_stale_tmp(path, name);
    let suffix = crate::oauth::random_hex(8);
    let tmp = path.with_file_name(format!(".{name}.{}.{suffix}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// Removes the temp files of `path` that writers killed before the rename left behind.
fn remove_stale_tmp(path: &Path, name: &str) {
    let Some(Ok(entries)) = path.parent().map(std::fs::read_dir) else {
        return;
    };
    let prefix = format!(".{name}.");
    for entry in entries.flatten() {
        let file = entry.file_name();
        let ours = file
            .to_str()
            .is_some_and(|file| file.starts_with(&prefix) && file.ends_with(".tmp"));
        let stale = || {
            let modified = entry.metadata().and_then(|meta| meta.modified());
            modified.is_ok_and(|time| time.elapsed().is_ok_and(|age| age > STALE_TMP))
        };
        if ours && stale() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_private(path, text.as_bytes())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// ISO time without milliseconds, as upstream `isoSeconds`.
pub fn iso_seconds(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| ms.to_string())
}

pub fn iso_minute(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%MZ").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// `2d 4h`, `3h 5m`, `12m`, or `now`, as multi-codex `formatDuration`.
pub fn duration(ms: i64) -> String {
    if ms <= 0 {
        return "now".into();
    }
    let minutes = (ms + 59_999) / 60_000;
    let (days, hours, mins) = (minutes / 1440, (minutes % 1440) / 60, minutes % 60);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 || (days == 0 && hours == 0) {
        parts.push(format!("{mins}m"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_and_times() {
        assert_eq!(duration(0), "now");
        assert_eq!(duration(61_000), "2m");
        assert_eq!(duration(26 * 3_600_000 + 60_000), "1d 2h 1m");
        assert_eq!(iso_seconds(1_500), "1970-01-01T00:00:01Z");
        assert_eq!(iso_minute(61_000), "1970-01-01T00:01Z");
    }

    #[test]
    fn writes_private_files() {
        let dir = std::env::temp_dir().join(format!("sr-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("a/b.json");
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || write_json(&path, &serde_json::json!({ "a": n })))
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        write_json(&path, &serde_json::json!({ "a": 1 })).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"a\": 1"));
        let leftovers = std::fs::read_dir(dir.join("a")).unwrap().count();
        assert_eq!(leftovers, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&dir.join("a")), 0o700);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removes_temp_files_that_killed_writers_left() {
        let dir = std::env::temp_dir().join(format!("sr-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_private_dir(&dir).unwrap();
        let left = dir.join(".usage.json.123.abcd.tmp");
        let live = dir.join(".usage.json.456.ef01.tmp");
        let other = dir.join(".accounts.json.123.abcd.tmp");
        for file in [&left, &live, &other] {
            std::fs::write(file, "{}").unwrap();
        }
        let old = std::time::SystemTime::now() - STALE_TMP * 2;
        for file in [&left, &other] {
            let handle = std::fs::File::options().write(true).open(file).unwrap();
            handle.set_modified(old).unwrap();
        }
        write_json(&dir.join("usage.json"), &serde_json::json!({})).unwrap();
        assert!(!left.exists(), "a killed writer's temp file is removed");
        assert!(live.exists(), "a write in progress keeps its temp file");
        assert!(other.exists(), "temp files of other files stay");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
