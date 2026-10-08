//! `spyglass rss <url>` — latest entries of an RSS/Atom/JSON feed.

use anyhow::{Context, Result};
use serde_json::json;

use crate::net::{DEFAULT_MAX_BYTES, Net};
use crate::output::{Doc, oneline};
use crate::platforms::web::html_to_markdown;

pub async fn read(net: &Net, url: &str, limit: usize) -> Result<Doc> {
    let feed = net.get(url, &[("Accept", "application/rss+xml,application/atom+xml,application/xml,*/*;q=0.5")], DEFAULT_MAX_BYTES).await?;
    parse(feed.body.as_bytes(), feed.url.as_str(), limit)
}

pub fn parse(bytes: &[u8], url: &str, limit: usize) -> Result<Doc> {
    let feed = feed_rs::parser::parse(bytes).context("not an RSS/Atom/JSON feed")?;
    let title = feed.title.as_ref().map(|t| t.content.trim().to_string()).unwrap_or_else(|| url.to_string());
    let mut md = format!("# {title}\n");
    let mut entries = Vec::new();
    for entry in feed.entries.iter().take(limit) {
        let e_title = entry.title.as_ref().map(|t| oneline(&t.content, 200)).unwrap_or_else(|| "(untitled)".into());
        let link = entry.links.first().map(|l| l.href.clone()).unwrap_or_default();
        let date = entry.published.or(entry.updated).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
        let summary = entry
            .summary
            .as_ref()
            .map(|t| t.content.as_str())
            .or_else(|| entry.content.as_ref().and_then(|c| c.body.as_deref()))
            .map(|html| oneline(&html_to_markdown(html), 300))
            .unwrap_or_default();
        md.push_str(&format!("\n- [{e_title}]({link})"));
        if !date.is_empty() {
            md.push_str(&format!(" — {date}"));
        }
        if !summary.is_empty() {
            md.push_str(&format!("\n  {summary}"));
        }
        entries.push(json!({ "title": e_title, "link": link, "date": date, "summary": summary }));
    }
    Ok(Doc::new("rss", Some(url.to_string()), md, json!({ "title": title, "entries": entries })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Example News</title>
<item><title>First post</title><link>https://ex.com/1</link><pubDate>Tue, 06 Oct 2026 10:00:00 GMT</pubDate>
<description>&lt;p&gt;Hello &lt;b&gt;world&lt;/b&gt;&lt;/p&gt;</description></item>
<item><title>Second post</title><link>https://ex.com/2</link></item>
<item><title>Third post</title><link>https://ex.com/3</link></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?><feed xmlns="http://www.w3.org/2005/Atom">
<title>Atom Blog</title><entry><title>Atom entry</title><link href="https://a.com/e"/>
<updated>2026-10-01T12:00:00Z</updated><summary>Short summary</summary></entry></feed>"#;

    #[test]
    fn parses_rss_with_limit() {
        let doc = parse(RSS.as_bytes(), "https://ex.com/feed", 2).unwrap();
        assert!(doc.markdown.starts_with("# Example News"));
        assert!(doc.markdown.contains("[First post](https://ex.com/1)"));
        assert!(doc.markdown.contains("2026-10-06"));
        assert!(doc.markdown.contains("Hello **world**") || doc.markdown.contains("Hello world"));
        assert!(doc.markdown.contains("Second post"));
        assert!(!doc.markdown.contains("Third post"));
        assert_eq!(doc.data["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn parses_atom() {
        let doc = parse(ATOM.as_bytes(), "https://a.com/atom", 10).unwrap();
        assert!(doc.markdown.contains("[Atom entry](https://a.com/e)"));
        assert!(doc.markdown.contains("Short summary"));
    }

    #[test]
    fn rejects_non_feeds() {
        assert!(parse(b"<html><body>nope</body></html>", "https://x.com", 5).is_err());
    }
}
