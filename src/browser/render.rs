//! JavaScript-rendered pages in a cookie-less context. Tried headless first (no window);
//! sites that block headless browsers are retried in the visible browser.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use super::scenario::{Blocked, Ctx, Found, Site, arg_str, scrape, solve_by_user};
use super::tab::Allow;

pub const SITE: Site = Site { allow: Allow::AnyPublic, scratch: true, visible: false };

/// Sites known to block headless browsers: go straight to the visible browser.
const VISIBLE_DOMAINS: &[&str] =
    &["reddit.com", "redd.it", "x.com", "twitter.com", "duckduckgo.com", "instagram.com", "facebook.com", "linkedin.com"];

/// Should this host skip the headless attempt? Built-in list plus hosts learned this session.
pub fn needs_visible(host: &str, learned: &HashSet<String>) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    learned.contains(&host) || VISIBLE_DOMAINS.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Does the page look like a bot wall (Cloudflare challenge, captcha, block page) rather than content?
pub fn looks_blocked(title: &str, text: &str, status: Option<u16>) -> bool {
    if matches!(status, Some(403 | 429)) {
        return true;
    }
    let title = title.to_lowercase();
    const TITLES: &[&str] = &["just a moment", "attention required", "access denied", "are you a robot", "security check"];
    if TITLES.iter().any(|t| title.contains(t)) {
        return true;
    }
    // Wall markers only count on short pages: an article may well talk about captchas.
    const MARKERS: &[&str] =
        &["verify you are human", "captcha", "unusual traffic", "you've been blocked", "enable javascript and cookies"];
    text.chars().count() < 2000 && {
        let text = text.to_lowercase();
        MARKERS.iter().any(|m| text.contains(m))
    }
}

/// `web read --render`: an arbitrary page in the cookie-less context.
pub async fn job(ctx: &Ctx<'_>, _verb: &str, args: &Value) -> Result<Found> {
    let url = crate::validate::public_url(arg_str(args, "url")?)?.to_string();
    let mut events = ctx.tab.events();
    let script = "new Promise(r => setTimeout(r, 1500)).then(() => ({ url: location.href, title: document.title, \
                  text: (document.body?.innerText || '').slice(0, 3000), html: document.documentElement.outerHTML }))";
    let mut v = scrape(ctx.tab, &url, script).await?;
    // HTTP status of the main document, from the events seen while loading.
    let mut status = None;
    while let Ok(ev) = events.try_recv() {
        if status.is_none()
            && ev.method == "Network.responseReceived"
            && ev.session.as_deref() == Some(ctx.tab.session.as_str())
            && ev.params["type"] == "Document"
        {
            status = ev.params["response"]["status"].as_u64().and_then(|s| u16::try_from(s).ok());
        }
    }
    let wall = |v: &Value, status| looks_blocked(v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or(""), status);
    if wall(&v, status) {
        let host = url::Url::parse(&url)?.host_str().unwrap_or("the site").to_string();
        if ctx.headless {
            return Err(Blocked(host).into()); // the daemon retries in the visible browser
        }
        let check = || async { ctx.tab.eval(script).await.ok().filter(|v| !wall(v, None)) };
        v = solve_by_user(ctx, &format!("{host} shows a bot check"), check).await?;
    }
    let str_of = |k: &str| v[k].as_str().unwrap_or("");
    let final_url = v["url"].as_str().unwrap_or(&url).to_string();
    let doc = crate::platforms::web::with_hint(crate::platforms::web::extract(str_of("html"), &final_url)?, true);
    Ok(Found { url: Some(final_url), markdown: doc.markdown, data: doc.data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_learned_hosts_go_visible() {
        let learned: HashSet<String> = ["news.example.org".to_string()].into();
        assert!(needs_visible("www.reddit.com", &learned));
        assert!(needs_visible("x.com", &learned));
        assert!(needs_visible("news.example.org", &learned));
        assert!(!needs_visible("example.org", &learned));
        assert!(!needs_visible("notreddit.com", &learned));
        assert!(!needs_visible("blog.rust-lang.org", &learned));
    }

    #[test]
    fn recognizes_bot_walls() {
        assert!(looks_blocked("Just a moment...", "Checking your browser", Some(503)));
        assert!(looks_blocked("Attention Required! | Cloudflare", "", Some(200)));
        assert!(looks_blocked("", "Please verify you are human to continue.", Some(200)));
        assert!(looks_blocked("Access denied", "", Some(403)));
        assert!(looks_blocked("Example", "content", Some(429)));
    }

    #[test]
    fn real_content_is_not_a_wall() {
        assert!(!looks_blocked("Example Domain", "This domain is for use in examples.", Some(200)));
        assert!(!looks_blocked("Page not found", "Sorry", Some(404)));
        // An article that merely mentions captchas is long; markers only count on short pages.
        let article = format!("How to verify you are human with a captcha. {}", "Lorem ipsum. ".repeat(400));
        assert!(!looks_blocked("Captchas explained", &article, Some(200)));
    }
}
