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
    Ok(with_hint(extract(&page.body, &final_url)?, false))
}

/// Append the thin-result note, if any, to the Markdown the agent sees.
pub fn with_hint(mut doc: Doc, rendered: bool) -> Doc {
    if let Some(note) = thin_hint(&doc.markdown, rendered) {
        doc.markdown.push_str(&format!("\n\n> Note: {note}"));
    }
    doc
}

/// Below this much article text, Readability probably picked the wrong block (listings, home pages).
const MIN_ARTICLE_CHARS: usize = 250;

/// Readability-style extraction of the main content, rendered as Markdown; pages that are
/// not articles fall back to the whole page without scripts and site chrome.
pub fn extract(html: &str, url: &str) -> Result<Doc> {
    let article = Readability::new(html, Some(url), None).ok().and_then(|mut r| r.parse().ok());
    let Some(article) = article.filter(|a| a.text_content.trim().chars().count() >= MIN_ARTICLE_CHARS) else {
        return Ok(whole_page(html, url));
    };
    let body = html_to_markdown(&article.content);
    let title = article.title.trim();
    let markdown = if title.is_empty() || body.starts_with(&format!("# {title}")) { body.clone() } else { format!("# {title}\n\n{body}") };
    let data = json!({
        "title": title,
        "byline": article.byline,
        "site_name": article.site_name,
        "published": article.published_time,
        "extraction": "article",
        "markdown": body,
    });
    Ok(Doc::new("web", Some(url.to_string()), markdown, data))
}

/// The whole page as Markdown: no scripts, navigation, header/footer or forms; absolute links.
fn whole_page(html: &str, url: &str) -> Doc {
    let page = dom_query::Document::from(html);
    let title = page.select("title").text().trim().to_string();
    page.select("script, style, noscript, template, svg, iframe, form, header, footer, nav, aside, [role=navigation], [role=banner], [role=contentinfo], [aria-hidden=true]").remove();
    if let Ok(base) = url::Url::parse(url) {
        for a in page.select("a[href]").iter() {
            if let Some(abs) = a.attr("href").and_then(|h| base.join(&h).ok()) {
                a.set_attr("href", abs.as_str());
            }
        }
    }
    let body = html_to_markdown(&page.select("body").inner_html());
    let markdown = if title.is_empty() { body.clone() } else { format!("# {title}\n\n{body}") };
    Doc::new("web", Some(url.to_string()), markdown, json!({ "title": title, "extraction": "page", "markdown": body }))
}

/// A note for the agent when little text came out (likely a JS-rendered page).
pub fn thin_hint(markdown: &str, rendered: bool) -> Option<&'static str> {
    if markdown.split_whitespace().count() >= 50 {
        return None;
    }
    Some(if rendered {
        "Little text was extracted from this page."
    } else {
        "Little text was extracted; the page may need JavaScript: try `spyglass web read --render <url>`."
    })
}

/// HTML fragment to Markdown; scripts, styles and embedded media are dropped.
pub fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "iframe", "svg", "img", "video", "audio", "form", "button"])
        .build();
    drop_empty_links(&converter.convert(html).unwrap_or_default()).trim().to_string()
}

/// Icon-only links (vote arrows, share buttons) become `[](url)`: noise for the agent.
fn drop_empty_links(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find("[](") {
        out.push_str(&rest[..i]);
        match rest[i..].find(')') {
            Some(end) => rest = &rest[i + end + 1..],
            None => {
                rest = &rest[i..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
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

    const LISTING: &str = r#"<!doctype html><html><head><title>Shop</title><script>x()</script></head><body>
<header><a href="/">Logo</a> <a href="/cart">Cart</a></header>
<main><h1>Homes for sale</h1><ul>
<li><a href="/h/1">2 bd, Seattle</a> $500k</li><li><a href="/h/2">3 bd, Tacoma</a> $420k</li>
<li><a href="/h/3">1 bd, Bellevue</a> $390k</li></ul></main>
<footer>© 2026 Shop Inc. All rights reserved.</footer></body></html>"#;

    #[test]
    fn non_article_pages_fall_back_to_the_whole_page() {
        let doc = extract(LISTING, "https://shop.example.com/").unwrap();
        assert_eq!(doc.data["extraction"], "page");
        assert!(doc.markdown.contains("Homes for sale"), "{}", doc.markdown);
        assert!(doc.markdown.contains("[3 bd, Tacoma](https://shop.example.com/h/2)") || doc.markdown.contains("3 bd, Tacoma"));
        assert!(!doc.markdown.contains("x()"), "no scripts");
        assert!(!doc.markdown.contains("All rights reserved"), "no footer");
        assert!(!doc.markdown.contains("Cart"), "no header chrome");
        let icons =
            extract(&LISTING.replace("<main>", r#"<main><a href="/vote"><img src="up.gif"></a>"#), "https://shop.example.com/").unwrap();
        assert!(!icons.markdown.contains("[]("), "{}", icons.markdown);
    }

    #[test]
    fn thin_results_suggest_render() {
        assert!(thin_hint("Loading…", false).unwrap().contains("--render"));
        assert!(thin_hint("Loading…", true).is_none() || !thin_hint("Loading…", true).unwrap().contains("--render"));
        assert!(thin_hint(&"word ".repeat(100), false).is_none());
    }

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
        assert_eq!(doc.data["extraction"], "article");
    }
}
