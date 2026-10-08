//! Reddit through the agent browser (logged-out works; `.json` endpoints are blocked by Reddit).
//! Data comes from the server-rendered DOM via our fixed scripts below.

use anyhow::{Result, bail};
use serde_json::Value;

use super::scenario::{Ctx, Found, Site, arg_str, arg_usize, scrape, when_ready};
use super::tab::Allow;
use crate::output::{oneline, str_at as s};
use crate::validate::{self, enc};

pub const LOGIN_URL: &str = "https://www.reddit.com/login/";
pub const SITE: Site =
    Site { allow: Allow::Domains(&["reddit.com", "redditstatic.com", "redditmedia.com"]), scratch: false, visible: true };
pub const BLOCKED_MARKER: &str = "blocked by network security";
pub const SEARCH_READY: &str = r#"a[data-testid="post-title"]"#;
/// Comments render after the post; a post without comments has nothing more to wait for.
pub const POST_READY: &str = r#"shreddit-comment, shreddit-post[comment-count="0"]"#;

/// Posts of a listing page (subreddit feed).
pub const LISTING_JS: &str = r#"(() => [...document.querySelectorAll('shreddit-post')].map(p => ({
  title: p.getAttribute('post-title'), author: p.getAttribute('author'),
  subreddit: p.getAttribute('subreddit-prefixed-name'), score: p.getAttribute('score'),
  comments: p.getAttribute('comment-count'), created: p.getAttribute('created-timestamp'),
  permalink: p.getAttribute('permalink'), link: p.getAttribute('content-href'), type: p.getAttribute('post-type')
})))()"#;

/// One post with its body and comment tree.
pub const POST_JS: &str = r#"(() => {
  const p = document.querySelector('shreddit-post');
  if (!p) return null;
  return {
    title: p.getAttribute('post-title'), author: p.getAttribute('author'),
    subreddit: p.getAttribute('subreddit-prefixed-name'), score: p.getAttribute('score'),
    comments: p.getAttribute('comment-count'), created: p.getAttribute('created-timestamp'),
    permalink: p.getAttribute('permalink'), link: p.getAttribute('content-href'),
    body: (p.querySelector('[slot="text-body"]')?.innerText || '').trim(),
    replies: [...document.querySelectorAll('shreddit-comment')].map(c => ({
      author: c.getAttribute('author'), score: c.getAttribute('score'), depth: Number(c.getAttribute('depth') || 0),
      created: c.getAttribute('created'),
      text: ([...c.querySelectorAll('[slot="comment"]')].find(e => e.closest('shreddit-comment') === c)?.innerText || '').trim()
    }))
  };
})()"#;

/// Search results (site-wide and in-subreddit pages use different containers; titles are common).
pub const SEARCH_JS: &str = r#"(() => { const seen = new Set(); return [...document.querySelectorAll('a[data-testid="post-title"]')].map(a => {
  const href = a.getAttribute('href');
  if (!href || seen.has(href)) return null;
  seen.add(href);
  const u = a.closest('[data-testid="search-post-with-content-preview"], [data-testid="search-post-unit"], [data-testid="sdui-post-unit"]') || a.parentElement;
  let ctx = {};
  try { ctx = JSON.parse((a.closest('search-telemetry-tracker') || u.querySelector('search-telemetry-tracker'))?.getAttribute('data-faceplate-tracking-context') || '{}'); } catch (e) {}
  const text = u.innerText || '';
  return {
    title: ctx.post?.title || a.getAttribute('aria-label') || a.innerText, author: ctx.profile?.name,
    subreddit: ctx.subreddit?.name || (href.match(/^\/r\/([^/]+)/) || [])[1], snippet: ctx.search?.snippet, permalink: href,
    votes: (text.match(/([\d.,]+k?)\s+votes?/i) || [])[1], comments: (text.match(/([\d.,]+k?)\s+comments?/i) || [])[1],
    age: (text.match(/(\d+\s*\w+ ago)/) || [])[1]
  };
}).filter(Boolean); })()"#;

const BASE: &str = "https://www.reddit.com";

pub fn search_url(query: &str, sub: Option<&str>) -> Result<String> {
    let q = enc(&validate::query(query)?);
    Ok(match sub {
        Some(sub) => format!("{BASE}/r/{}/search/?q={q}&type=posts&restrict_sr=1", validate::subreddit(sub)?),
        None => format!("{BASE}/search/?q={q}&type=posts"),
    })
}

pub fn sub_url(sub: &str, sort: &str) -> Result<String> {
    let sub = validate::subreddit(sub)?;
    match sort {
        "top" => Ok(format!("{BASE}/r/{sub}/top/?t=week")),
        "hot" | "new" | "rising" => Ok(format!("{BASE}/r/{sub}/{sort}/")),
        _ => bail!("sort must be hot, new, top or rising"),
    }
}

pub fn post_url(input: &str) -> Result<String> {
    let input = input.trim();
    if (4..=12).contains(&input.len()) && input.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        return Ok(format!("{BASE}/comments/{input}/"));
    }
    let mut url = validate::url_on(input, &["reddit.com"])?;
    if !url.path().contains("/comments/") {
        bail!("not a Reddit post URL");
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn abs(permalink: &str) -> String {
    if permalink.starts_with('/') { format!("{BASE}{permalink}") } else { permalink.to_string() }
}

/// Outbound link of a link post (self posts point back to Reddit).
fn external(v: &Value) -> Option<&str> {
    Some(s(v, "link")).filter(|l| l.starts_with("http") && !l.contains("reddit.com"))
}

fn meta(v: &Value) -> String {
    format!(
        "{} · u/{} · {} points · {} comments · {}",
        s(v, "subreddit"),
        s(v, "author"),
        s(v, "score"),
        s(v, "comments"),
        s(v, "created").get(..10).unwrap_or("")
    )
}

pub fn render_listing(v: &Value, limit: usize) -> String {
    let mut md = String::new();
    for p in v.as_array().into_iter().flatten().take(limit) {
        md.push_str(&format!("- [{}]({}) — {}\n", oneline(s(p, "title"), 200), abs(s(p, "permalink")), meta(p)));
        if let Some(link) = external(p) {
            md.push_str(&format!("  → {link}\n"));
        }
    }
    if md.is_empty() { "No posts.".into() } else { md }
}

pub fn render_search(v: &Value, limit: usize) -> String {
    let mut md = String::new();
    for p in v.as_array().into_iter().flatten().take(limit) {
        md.push_str(&format!(
            "- [{}]({}) — r/{} · u/{} · {} votes · {} comments · {}\n",
            oneline(s(p, "title"), 200),
            abs(s(p, "permalink")),
            s(p, "subreddit"),
            s(p, "author"),
            s(p, "votes"),
            s(p, "comments"),
            s(p, "age")
        ));
        let snippet = s(p, "snippet");
        if !snippet.is_empty() {
            md.push_str(&format!("  {}\n", oneline(snippet, 300)));
        }
    }
    if md.is_empty() { "No results.".into() } else { md }
}

pub fn render_post(v: &Value, max_comments: usize) -> String {
    let mut md = format!("# {}\n\n{}\n", s(v, "title"), meta(v));
    if let Some(link) = external(v) {
        md.push_str(&format!("→ {link}\n"));
    }
    let body = s(v, "body");
    if !body.is_empty() {
        md.push_str(&format!("\n{body}\n"));
    }
    let replies: Vec<&Value> = v["replies"].as_array().into_iter().flatten().take(max_comments).collect();
    if !replies.is_empty() {
        md.push_str("\n## Comments\n");
        for c in replies {
            let indent = "  ".repeat(c["depth"].as_u64().unwrap_or(0).min(8) as usize);
            md.push_str(&format!("{indent}- u/{} ({}): {}\n", s(c, "author"), s(c, "score"), oneline(s(c, "text"), 600)));
        }
    }
    md
}

/// `reddit search|sub|post` in the agent tab.
pub async fn job(ctx: &Ctx<'_>, verb: &str, args: &Value) -> Result<Found> {
    let limit = arg_usize(args, "limit", 10);
    let (url, ready, script) = match verb {
        "search" => (search_url(arg_str(args, "query")?, args["sub"].as_str())?, SEARCH_READY, SEARCH_JS),
        "sub" => (sub_url(arg_str(args, "name")?, args["sort"].as_str().unwrap_or("hot"))?, "shreddit-post", LISTING_JS),
        "post" => (post_url(arg_str(args, "post")?)?, POST_READY, POST_JS),
        _ => bail!("unknown reddit command {verb}"),
    };
    let script = when_ready(ready, BLOCKED_MARKER, script);
    let v = scrape(ctx.tab, &url, &script).await?;
    if v["blocked"] == true {
        bail!(
            "Reddit blocked the agent browser. Use the visible mode (`spyglass browser start`) or log in: `spyglass browser login reddit`"
        );
    }
    let data = v["data"].clone();
    let md = match verb {
        "search" => render_search(&data, limit),
        "sub" => render_listing(&data, limit),
        _ if data.is_null() => bail!("no post found at {url}"),
        _ => render_post(&data, arg_usize(args, "comments", 30)),
    };
    Ok(Found { url: Some(url), markdown: md, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urls_are_built_from_validated_input() {
        assert_eq!(search_url("tokio runtime", None).unwrap(), "https://www.reddit.com/search/?q=tokio+runtime&type=posts");
        assert_eq!(
            search_url("a&b=c", Some("r/rust")).unwrap(),
            "https://www.reddit.com/r/rust/search/?q=a%26b%3Dc&type=posts&restrict_sr=1"
        );
        assert_eq!(sub_url("rust", "top").unwrap(), "https://www.reddit.com/r/rust/top/?t=week");
        assert_eq!(sub_url("r/rust", "hot").unwrap(), "https://www.reddit.com/r/rust/hot/");
        assert!(sub_url("rust", "controversial;drop").is_err());
        assert!(sub_url("../../evil", "hot").is_err());
        assert_eq!(post_url("1wuyg33").unwrap(), "https://www.reddit.com/comments/1wuyg33/");
        assert_eq!(
            post_url("https://www.reddit.com/r/rust/comments/1wuyg33/rust_1990_is_out/").unwrap(),
            "https://www.reddit.com/r/rust/comments/1wuyg33/rust_1990_is_out/"
        );
        assert!(post_url("https://evil.com/r/rust/comments/1wuyg33/").is_err());
        assert!(post_url("https://www.reddit.com/r/rust/").is_err());
        assert!(post_url("../x").is_err());
    }

    #[test]
    fn renders_listing() {
        let v = json!([
            {"title":"Rust 1.99.0 is out","author":"manpacket","subreddit":"r/rust","score":"766","comments":"107",
             "created":"2026-10-01T12:43:28.711000+0000","permalink":"/r/rust/comments/1wuyg33/rust_1990_is_out/","link":"https://blog.rust-lang.org/x","type":"link"},
            {"title":"Second","author":"b","subreddit":"r/rust","score":"1","comments":"0","permalink":"/r/rust/comments/2/s/"}
        ]);
        let md = render_listing(&v, 1);
        assert!(md.contains("[Rust 1.99.0 is out](https://www.reddit.com/r/rust/comments/1wuyg33/rust_1990_is_out/)"), "{md}");
        assert!(md.contains("766 points") && md.contains("107 comments") && md.contains("u/manpacket") && md.contains("2026-10-01"));
        assert!(md.contains("→ https://blog.rust-lang.org/x"));
        assert!(!md.contains("Second"));
    }

    #[test]
    fn renders_search() {
        let v = json!([{"title":"Principles for fast Tokio applications","author":"Shnatsel","subreddit":"rust",
            "snippet":"multiple \"runtime\" system","permalink":"/r/rust/comments/1wg8hfo/p/","votes":"217","comments":"23","age":"24d ago"}]);
        let md = render_search(&v, 10);
        assert!(md.contains("[Principles for fast Tokio applications](https://www.reddit.com/r/rust/comments/1wg8hfo/p/)"));
        assert!(md.contains("r/rust") && md.contains("217 votes") && md.contains("23 comments") && md.contains("24d ago"));
        assert!(md.contains("multiple \"runtime\" system"));
        assert_eq!(render_search(&json!([]), 10), "No results.");
    }

    #[test]
    fn renders_post_with_indented_comments() {
        let v = json!({"title":"Rust 1.99.0 is out","author":"manpacket","subreddit":"r/rust","score":"766","comments":"107",
        "created":"2026-10-01T12:43:28.711000+0000","permalink":"/r/rust/comments/1wuyg33/x/","link":"https://blog.rust-lang.org/x",
        "body":"Release notes inside",
        "replies":[
            {"author":"ann","score":"472","depth":0,"created":"2026-10-01T13:06:05.816000+0000","text":"Can't wait for Rust 2"},
            {"author":"bob","score":"12","depth":1,"text":"Never"},
            {"author":"cid","score":"3","depth":0,"text":"third"}
        ]});
        let md = render_post(&v, 2);
        assert!(md.starts_with("# Rust 1.99.0 is out"));
        assert!(md.contains("Release notes inside") && md.contains("→ https://blog.rust-lang.org/x"));
        assert!(md.contains("- u/ann (472): Can't wait for Rust 2"));
        assert!(md.contains("  - u/bob (12): Never"));
        assert!(!md.contains("third"));
    }
}
