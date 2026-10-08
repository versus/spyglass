//! `spyglass web read <url>` — fetch locally, extract the article, convert to Markdown.

use anyhow::{Result, bail};
use dom_smoothie::Readability;
use serde_json::json;

use crate::net::{DEFAULT_MAX_BYTES, Net};
use crate::output::Doc;

pub async fn read(net: &Net, url: &str) -> Result<Doc> {
    let page = net.get(url, &[("Accept", "text/html,application/xhtml+xml,text/plain;q=0.8")], DEFAULT_MAX_BYTES).await?;
    let final_url = page.url.to_string();
    if page.content_type.starts_with("text/plain") || page.content_type.starts_with("text/markdown") {
        return Ok(Doc::new("web", Some(final_url), page.body.clone(), json!({ "text": page.body })));
    }
    if !page.content_type.is_empty() && !page.content_type.contains("html") {
        bail!("unsupported content type: {}", page.content_type);
    }
    extract(&page.body, &final_url)
}

/// Readability-style extraction of the main content, rendered as Markdown.
pub fn extract(html: &str, url: &str) -> Result<Doc> {
    let article = Readability::new(html, Some(url), None)?.parse()?;
    let body = html_to_markdown(&article.content);
    let title = article.title.trim();
    let markdown = if title.is_empty() || body.starts_with(&format!("# {title}")) { body.clone() } else { format!("# {title}\n\n{body}") };
    let data = json!({
        "title": title,
        "byline": article.byline,
        "site_name": article.site_name,
        "published": article.published_time,
        "markdown": body,
    });
    Ok(Doc::new("web", Some(url.to_string()), markdown, data))
}

/// HTML fragment to Markdown; scripts, styles and embedded media are dropped.
pub fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "iframe", "svg", "img", "video", "audio", "form", "button"])
        .build();
    converter.convert(html).unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Rust 2026 roadmap | Blog</title>
<script>window.evil = 1;</script><style>body{}</style></head><body>
<nav><a href="/">Home</a> <a href="/about">About</a> <a href="/login">Login</a></nav>
<article><h1>Rust 2026 roadmap</h1>
<p>The Rust project published its roadmap for 2026. It focuses on async ergonomics, faster compile times
and better tooling for embedded developers. This paragraph is long enough to look like real content to a
readability algorithm, which scores paragraphs by length and comma count, so we add commas, clauses, and words.</p>
<p>Second paragraph with a <a href="/details">relative link</a> and more meaningful prose about the language,
its community, the compiler, the standard library, and the ecosystem of crates that people use every day.</p>
<pre><code>fn main() { println!("hi"); }</code></pre>
</article>
<footer>© 2026 Example Corp. All rights reserved.</footer></body></html>"#;

    #[test]
    fn extracts_article_as_markdown() {
        let doc = extract(PAGE, "https://blog.example.com/rust-2026").unwrap();
        assert_eq!(doc.source, "web");
        assert!(doc.markdown.starts_with("# Rust 2026 roadmap"), "{}", doc.markdown);
        assert!(doc.markdown.contains("async ergonomics"));
        assert!(doc.markdown.contains("https://blog.example.com/details"), "relative links become absolute");
        assert!(doc.markdown.contains("println!"));
        assert!(!doc.markdown.contains("window.evil"));
        assert!(!doc.markdown.contains("All rights reserved"));
        assert!(doc.data["title"].as_str().unwrap().contains("Rust 2026 roadmap"));
    }
}
