//! `spyglass doctor` — what works right now. Read-only: no installs, no config writes, no logins.

use std::time::Duration;

use crate::browser::{chrome, client};
use crate::secrets::{self, Secret};
use crate::tools;

pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

pub async fn run() -> String {
    let mut checks = vec![Check { name: "web / rss / github", ok: true, detail: "built in".into() }];
    let ytdlp = tools::run("yt-dlp", &["--version"], Duration::from_secs(10), 1024).await;
    checks.push(Check {
        name: "youtube (yt-dlp)",
        ok: ytdlp.is_ok(),
        detail: ytdlp.map(|v| format!("yt-dlp {}", v.trim())).unwrap_or_else(|_| "install yt-dlp from your package manager".into()),
    });
    let deno = tools::find("deno");
    checks.push(Check {
        name: "youtube JS runtime (deno)",
        ok: deno.is_some(),
        detail: deno.map(|p| p.display().to_string()).unwrap_or_else(|| "install deno: yt-dlp needs it for YouTube".into()),
    });
    let chrome = chrome::find_chrome();
    let chrome_found = chrome.is_some();
    checks.push(Check {
        name: "agent browser (Chrome)",
        ok: chrome.is_some(),
        detail: chrome.map(|p| p.display().to_string()).unwrap_or_else(|| "install Google Chrome or Chromium".into()),
    });
    let running = client::is_running().await;
    checks.push(Check {
        name: "agent browser running",
        ok: running,
        detail: if running { "yes".into() } else { "starts automatically on first reddit/x command".into() },
    });
    let key = secrets::get(Secret::BraveApiKey).is_some() || secrets::get(Secret::ExaApiKey).is_some();
    checks.push(Check {
        name: "web search",
        ok: key || chrome_found,
        detail: if key { "Brave/Exa API key configured".into() } else { "DuckDuckGo via the agent browser (no key needed)".into() },
    });
    let gh = secrets::get(Secret::GithubToken).is_some();
    checks.push(Check {
        name: "github token (optional)",
        ok: gh,
        detail: if gh { "configured".into() } else { "optional: higher rate limits and code search".into() },
    });
    render(&checks)
}

pub fn render(checks: &[Check]) -> String {
    let mut out: String = checks.iter().map(|c| format!("{} {} — {}\n", if c.ok { "✅" } else { "⬜" }, c.name, c.detail)).collect();
    out.push_str(&format!("\n{}/{} ready", checks.iter().filter(|c| c.ok).count(), checks.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_status_lines() {
        let checks = [
            Check { name: "web / rss / github", ok: true, detail: "built in".into() },
            Check { name: "web search key", ok: false, detail: "spyglass secrets set brave-api-key".into() },
        ];
        let out = render(&checks);
        assert!(out.contains("✅ web / rss / github — built in"));
        assert!(out.contains("⬜ web search key — spyglass secrets set brave-api-key"));
        assert!(out.ends_with("1/2 ready"));
    }
}
