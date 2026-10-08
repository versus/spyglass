//! Append-only JSONL audit log: what was called, which host, outcome. Never arguments' secrets or bodies.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Entry<'a> {
    pub ts: u64,
    pub command: &'a str,
    pub host: Option<String>,
    pub ok: bool,
    pub output_chars: usize,
}

pub fn default_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(base.join("spyglass").join("audit.jsonl"))
}

/// Best effort: auditing must never break the actual command.
pub fn record(path: &Path, entry: &Entry) {
    let _ = try_record(path, entry);
}

fn try_record(path: &Path, entry: &Entry) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn appends_private_jsonl_lines() {
        let dir = std::env::temp_dir().join(format!("spyglass-audit-test-{}", std::process::id()));
        let path = dir.join("sub").join("audit.jsonl");
        let e = Entry { ts: 1, command: "web read", host: Some("example.com".into()), ok: true, output_chars: 10 };
        record(&path, &e);
        record(&path, &Entry { ok: false, ..e });
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"host\":\"example.com\""));
        assert!(lines[1].contains("\"ok\":false"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
