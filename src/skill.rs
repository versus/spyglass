//! The agent skill ships inside the binary — never fetched from the network.

use std::path::Path;

use anyhow::Result;

pub const SKILL_MD: &str = include_str!("../skill/SKILL.md");

pub fn run(install: bool) -> Result<String> {
    if !install {
        return Ok(SKILL_MD.to_string());
    }
    let dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?.join(".claude/skills/spyglass");
    let path = install_to(&dir)?;
    Ok(format!("skill installed: {}", path.display()))
}

/// Write SKILL.md into `dir`, refusing to write through a symlink.
pub fn install_to(dir: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    for p in [dir, path.as_path()] {
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!("refusing to write through a symlink: {}", p.display());
        }
    }
    std::fs::write(&path, SKILL_MD)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_is_small_and_complete() {
        // ~4 chars per token: stay within ~800 tokens for small models.
        assert!(SKILL_MD.len() <= 3400, "SKILL.md is {} bytes", SKILL_MD.len());
        for cmd in [
            "spyglass web read",
            "spyglass search",
            "spyglass rss",
            "spyglass github",
            "spyglass youtube",
            "spyglass reddit",
            "spyglass doctor",
        ] {
            assert!(SKILL_MD.contains(cmd), "SKILL.md must mention `{cmd}`");
        }
        // X is not verified on live data yet: agents must not be sent there.
        assert!(!SKILL_MD.contains("spyglass x "), "X stays out of the skill until verified");
        assert!(SKILL_MD.contains("<untrusted"));
        assert!(!SKILL_MD.to_uppercase().contains("MUST USE"));
    }

    #[test]
    fn installs_and_refuses_symlinks() {
        let base = std::env::temp_dir().join(format!("spyglass-skill-test-{}", std::process::id()));
        let dir = base.join("spyglass");
        let path = install_to(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/tmp/somewhere-else", &path).unwrap();
        assert!(install_to(&dir).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
}
