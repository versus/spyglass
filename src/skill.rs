//! The agent skill ships inside the binary (never fetched from the network) and is
//! generated for this machine: only commands whose dependencies are installed are listed.
//! Template lines prefixed with `<!--cap:NAME-->` are kept only when NAME is available.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::browser::chrome;
use crate::secrets::{self, Secret};
use crate::tools;

const TEMPLATE: &str = include_str!("../skill/SKILL.md");
const MARKER: &str = "<!-- spyglass-capabilities: ";

/// Optional capabilities (web, rss and github always work) and what enables each.
const OPTIONAL: &[(&str, &str)] = &[
    ("browser", "Reddit and `--render`: install Google Chrome or Chromium"),
    ("search", "web search: install Google Chrome or Chromium (or set a Brave/Exa key with `spyglass secrets set`)"),
    ("youtube", "YouTube: install `yt-dlp` and `deno`"),
];

pub type Caps = BTreeSet<&'static str>;

/// What works on this machine right now (read-only checks).
pub fn detect() -> Caps {
    let browser = chrome::find_chrome().is_some();
    let key = secrets::get(Secret::BraveApiKey).is_some() || secrets::get(Secret::ExaApiKey).is_some();
    let youtube = tools::find("yt-dlp").is_some() && tools::find("deno").is_some();
    let mut caps: Caps = ["web", "rss", "github"].into();
    for (name, on) in [("browser", browser), ("search", browser || key), ("youtube", youtube)] {
        if on {
            caps.insert(name);
        }
    }
    caps
}

/// The skill text for a set of capabilities.
pub fn render(caps: &Caps) -> String {
    let mut out = String::new();
    for line in TEMPLATE.lines() {
        let kept = match line.strip_prefix("<!--cap:").and_then(|rest| rest.split_once("-->")) {
            Some((cap, text)) => caps.contains(cap).then_some(text),
            None => Some(line),
        };
        if let Some(text) = kept {
            out.push_str(text);
            out.push('\n');
        }
    }
    let missing: Vec<&str> = OPTIONAL.iter().filter(|(cap, _)| !caps.contains(cap)).map(|(_, hint)| *hint).collect();
    if !missing.is_empty() {
        out.push_str("\n## Not available on this machine\n");
        out.push_str("If the user needs one of these, tell them what to install. After they install it, run `spyglass skill --install` to refresh this skill.\n");
        for hint in missing {
            out.push_str(&format!("- {hint}\n"));
        }
    }
    let list: Vec<&str> = caps.iter().copied().collect();
    out.push_str(&format!("\n{MARKER}{} -->\n", list.join(",")));
    out
}

/// Capabilities recorded in an installed skill, if it carries our marker.
pub fn installed_caps(skill_md: &str) -> Option<BTreeSet<String>> {
    let line = skill_md.lines().find_map(|l| l.strip_prefix(MARKER))?;
    let list = line.strip_suffix(" -->")?;
    Some(list.split(',').filter(|c| !c.is_empty()).map(str::to_string).collect())
}

/// Doctor line: is the installed skill present and in sync with this machine?
pub fn status(installed: Option<&str>, current: &Caps) -> (bool, String) {
    const FIX: &str = "run `spyglass skill --install`";
    let Some(md) = installed else { return (false, format!("not installed: {FIX}")) };
    let Some(have) = installed_caps(md) else { return (false, format!("from an older spyglass: {FIX}")) };
    let now: BTreeSet<String> = current.iter().map(|c| c.to_string()).collect();
    if have == now {
        return (true, "up to date".into());
    }
    let join = |s: Vec<&String>| if s.is_empty() { "-".to_string() } else { s.into_iter().cloned().collect::<Vec<_>>().join(", ") };
    let added = join(now.difference(&have).collect());
    let gone = join(have.difference(&now).collect());
    (false, format!("outdated (newly available: {added}; no longer available: {gone}): {FIX}"))
}

pub fn install_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude/skills/spyglass"))
}

pub fn run(install: bool) -> Result<String> {
    let text = render(&detect());
    if !install {
        return Ok(text);
    }
    let dir = install_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let path = install_to(&dir, &text)?;
    Ok(format!("skill installed: {}", path.display()))
}

/// Write SKILL.md into `dir`, refusing to write through a symlink.
pub fn install_to(dir: &Path, text: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    for p in [dir, path.as_path()] {
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!("refusing to write through a symlink: {}", p.display());
        }
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(names: &[&'static str]) -> Caps {
        names.iter().copied().collect()
    }

    fn all() -> Caps {
        caps(&["web", "rss", "github", "browser", "search", "youtube"])
    }

    #[test]
    fn full_skill_is_small_and_complete() {
        let md = render(&all());
        // ~4 chars per token: stay within ~800 tokens for small models.
        assert!(md.len() <= 3400, "SKILL.md is {} bytes", md.len());
        for cmd in [
            "spyglass web read",
            "--render",
            "spyglass search",
            "spyglass rss",
            "spyglass github",
            "spyglass youtube",
            "spyglass reddit",
            "spyglass doctor",
        ] {
            assert!(md.contains(cmd), "must mention `{cmd}`");
        }
        assert!(md.contains("<untrusted"));
        assert!(!md.to_uppercase().contains("MUST USE"));
        assert!(!md.contains("spyglass x "), "X stays out of the skill until verified");
        assert!(!md.contains("<!--cap:"), "template markers must not leak");
        assert!(!md.contains("Not available"));
    }

    #[test]
    fn minimal_machine_lists_only_what_works_and_how_to_get_more() {
        let md = render(&caps(&["web", "rss", "github"]));
        for gone in ["spyglass youtube", "spyglass reddit", "spyglass web read --render", "spyglass search"] {
            assert!(!md.contains(gone), "`{gone}` must be omitted");
        }
        assert!(md.contains("spyglass web read") && md.contains("spyglass github repo"));
        assert!(md.contains("## Not available on this machine"));
        assert!(md.contains("install `yt-dlp` and `deno`") && md.contains("Chrome"));
        assert!(md.contains("spyglass skill --install"), "tells how to refresh");
        assert!(!md.contains("<!--cap:"));
    }

    #[test]
    fn marker_round_trips() {
        let c = caps(&["web", "rss", "github", "youtube"]);
        let found = installed_caps(&render(&c)).unwrap();
        assert_eq!(found, c.iter().map(|s| s.to_string()).collect());
        assert_eq!(installed_caps("# an old skill without marker"), None);
    }

    #[test]
    fn doctor_status() {
        let now = caps(&["web", "rss", "github", "youtube"]);
        let (ok, msg) = status(None, &now);
        assert!(!ok && msg.contains("spyglass skill --install"), "{msg}");
        let (ok, _) = status(Some(&render(&now)), &now);
        assert!(ok);
        let (ok, msg) = status(Some(&render(&caps(&["web", "rss", "github"]))), &now);
        assert!(!ok && msg.contains("youtube") && msg.contains("spyglass skill --install"), "{msg}");
        let (ok, msg) = status(Some("# old skill"), &now);
        assert!(!ok && msg.contains("spyglass skill --install"), "{msg}");
    }

    #[test]
    fn installs_and_refuses_symlinks() {
        let base = std::env::temp_dir().join(format!("spyglass-skill-test-{}", std::process::id()));
        let dir = base.join("spyglass");
        let path = install_to(&dir, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/tmp/somewhere-else", &path).unwrap();
        assert!(install_to(&dir, "x").is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
}
