//! `spyglass web read <url>` — fetch locally, extract the article, convert to Markdown.

use anyhow::{Result, bail};
use dom_smoothie::Readability;
use serde_json::json;

use crate::net::Net;
use crate::output::Doc;

/// PDFs are larger than pages.
const MAX_PAGE_BYTES: usize = 20 * 1024 * 1024;

pub async fn read(net: &Net, url: &str, links: bool) -> Result<Doc> {
    let accept = "text/html,application/xhtml+xml,application/pdf,text/plain;q=0.8";
    let page = net.get(url, &[("Accept", accept)], MAX_PAGE_BYTES).await?;
    let final_url = page.url.to_string();
    if is_pdf(&page.content_type, &page.bytes) {
        return pdf_to_doc(&page.bytes, &final_url);
    }
    let text = page.text();
    if page.content_type.starts_with("text/plain") || page.content_type.starts_with("text/markdown") {
        return Ok(Doc::new("web", Some(final_url), text.clone(), json!({ "text": text })));
    }
    if !page.content_type.is_empty() && !page.content_type.contains("html") {
        bail!("unsupported content type: {}", page.content_type);
    }
    let doc = with_hint(extract(&text, &final_url)?, false);
    Ok(if links { with_links(doc, &text, &final_url) } else { doc })
}

/// Append the "## Links" section (and the list in --json data).
pub fn with_links(mut doc: Doc, html: &str, base: &str) -> Doc {
    let (md, links) = links_section(html, base, 50);
    doc.markdown.push_str(&format!("\n\n{md}"));
    doc.data["links"] = serde_json::Value::Array(links);
    doc
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
/// Above this share of link text, an "article" is really a list of links.
const MAX_ARTICLE_LINK_RATIO: f64 = 0.5;

/// Readability-style extraction of the main content, rendered as Markdown; pages that are
/// not articles fall back to the whole page without scripts and site chrome.
pub fn extract(html: &str, url: &str) -> Result<Doc> {
    if let Some(doc) = job_posting(html, url) {
        return Ok(doc);
    }
    let cleaned = strip_consent(html);
    let html = cleaned.as_str();
    let article = Readability::new(html, Some(url), None).ok().and_then(|mut r| r.parse().ok());
    let Some(article) = article.filter(|a| a.text_content.trim().chars().count() >= MIN_ARTICLE_CHARS) else {
        return Ok(whole_page(html, url));
    };
    let body = html_to_markdown(&article.content);
    // Mostly links means Readability picked navigation (e.g. "similar jobs"), not the content.
    if link_ratio(&body) > MAX_ARTICLE_LINK_RATIO {
        return Ok(whole_page(html, url));
    }
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
    let doc = Doc::new("web", Some(url.to_string()), markdown, data);
    // Readability sometimes picks a promo box on listing pages: compare with the whole page.
    Ok(if doc.markdown.split_whitespace().count() < THIN_ARTICLE_WORDS { richer(doc, whole_page(html, url)) } else { doc })
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

pub fn is_pdf(content_type: &str, bytes: &[u8]) -> bool {
    content_type.starts_with("application/pdf") || bytes.starts_with(b"%PDF-")
}

/// Text of a PDF, page by page. Parsing untrusted PDFs runs on its own thread with a time
/// limit, and a parser panic becomes an error instead of taking the process down.
pub fn pdf_to_doc(bytes: &[u8], url: &str) -> Result<Doc> {
    use std::sync::mpsc::RecvTimeoutError;
    let data = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(pdf_extract::extract_text_from_mem_by_pages(&data));
    });
    let pages = match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(pages)) => pages,
        Ok(Err(e)) => bail!("could not read the PDF: {e}"),
        Err(RecvTimeoutError::Timeout) => bail!("reading the PDF took longer than 30s"),
        Err(RecvTimeoutError::Disconnected) => bail!("could not read the PDF (the parser failed)"),
    };
    let name = url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments()?.next_back().filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| "document.pdf".into());
    let texts: Vec<&str> = pages.iter().map(|p| p.trim()).collect();
    let body = if texts.len() == 1 {
        texts[0].to_string()
    } else {
        texts
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.is_empty())
            .map(|(i, t)| format!("## Page {}\n\n{t}", i + 1))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let data = json!({ "title": name, "extraction": "pdf", "pages": pages.len(), "markdown": body });
    Ok(Doc::new("web", Some(url.to_string()), format!("# {name}\n\n{body}"), data))
}

/// "## Links" with unique absolute http(s) links (text → URL), for `--links`.
pub fn links_section(html: &str, base: &str, max: usize) -> (String, Vec<serde_json::Value>) {
    let page = dom_query::Document::from(html);
    let base = url::Url::parse(base).ok();
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for a in page.select("a[href]").iter() {
        let Some(href) = a.attr("href").filter(|h| !h.trim_start().starts_with('#')) else { continue };
        let Some(mut target) = base.as_ref().and_then(|b| b.join(&href).ok()) else { continue };
        if !matches!(target.scheme(), "http" | "https") {
            continue;
        }
        target.set_fragment(None);
        if seen.insert(target.to_string()) {
            links.push(json!({ "text": crate::output::oneline(&a.text(), 120), "url": target.to_string() }));
        }
        if links.len() == max {
            break;
        }
    }
    let mut md = String::from("## Links\n");
    for l in &links {
        let (text, url) = (l["text"].as_str().unwrap_or(""), l["url"].as_str().unwrap_or(""));
        md.push_str(&if text.is_empty() { format!("- {url}\n") } else { format!("- [{text}]({url})\n") });
    }
    (md, links)
}

/// A JSON-LD `JobPosting` (schema.org, required by Google for Jobs) as a clean posting.
fn job_posting(html: &str, url: &str) -> Option<Doc> {
    use serde_json::Value;
    let page = dom_query::Document::from(html);
    let scripts: Vec<Value> =
        page.select(r#"script[type="application/ld+json"]"#).iter().filter_map(|s| serde_json::from_str::<Value>(&s.text()).ok()).collect();
    // A script may hold one object, an array, or an @graph.
    let job = scripts
        .iter()
        .flat_map(|v| match v {
            Value::Array(items) => items.clone(),
            v if v["@graph"].is_array() => v["@graph"].as_array().cloned().unwrap_or_default(),
            v => vec![v.clone()],
        })
        .find(|v| v["@type"] == "JobPosting")?;
    let text = |v: &Value| v.as_str().unwrap_or("").trim().to_string();
    let title = text(&job["title"]);
    let address = &job["jobLocation"]["address"];
    let place = [text(&address["addressLocality"]), text(&address["addressCountry"])]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let date = text(&job["datePosted"]).chars().take(10).collect::<String>();
    let facts: Vec<String> = [text(&job["hiringOrganization"]["name"]), place, text(&job["employmentType"]), date]
        .into_iter()
        .filter(|f| !f.is_empty())
        .collect();
    let description = html_to_markdown(&crate::output::unescape(job["description"].as_str()?));
    let markdown = format!("# {title}\n\n{}\n\n{description}", facts.join(" · "));
    Some(Doc::new("web", Some(url.to_string()), markdown, json!({ "title": title, "extraction": "job-posting", "job": job })))
}

/// Remove cookie/consent banners (known consent frameworks, plus short elements whose
/// id/class/label says cookie or consent and whose text sounds like a consent prompt),
/// and unhide the content such a modal hid.
pub fn strip_consent(html: &str) -> String {
    const FRAMEWORKS: &str = "#onetrust-consent-sdk, #onetrust-banner-sdk, #CybotCookiebotDialog, #usercentrics-root, \
        #didomi-host, #cookiescript_injected, #truste-consent-track, .qc-cmp2-container, .cc-window";
    const CANDIDATES: &str = r#"[id*="ookie"], [class*="ookie"], [id*="onsent"], [class*="onsent"], [aria-label*="ookie"], [aria-label*="onsent"], [role="dialog"], [role="alertdialog"]"#;
    // Phrases of consent prompts, not of content that merely mentions cookies.
    const PHRASES: &[&str] = &[
        "we use cookies",
        "uses cookies",
        "use of cookies",
        "cookie settings",
        "cookie policy",
        "cookie preferences",
        "accept all",
        "reject all",
        "decline all",
        "allow all",
        "manage preferences",
        "accept cookies",
    ];
    let page = dom_query::Document::from(html);
    page.select(FRAMEWORKS).remove();
    for el in page.select(CANDIDATES).iter() {
        let text = el.text().to_lowercase();
        if text.chars().count() < 1500 && PHRASES.iter().any(|p| text.contains(p)) {
            el.remove();
        }
    }
    // While a modal (the banner) is open, libraries hide the whole app behind it with
    // aria-hidden/inert. Unhide large containers; small decorative ones stay hidden.
    for el in page.select(r#"[aria-hidden="true"], [inert]"#).iter() {
        if el.text().chars().count() > 200 {
            el.remove_attrs(&["aria-hidden", "inert"]);
        }
    }
    page.html().to_string()
}

/// Words of prose an article needs before we trust Readability over the whole page.
const THIN_ARTICLE_WORDS: usize = 80;

/// Keep a substantial article; if it is thin, prefer the whole page when that has more text.
fn richer(article: Doc, page: Doc) -> Doc {
    let words = |d: &Doc| d.markdown.split_whitespace().count();
    if words(&article) >= THIN_ARTICLE_WORDS || words(&page) <= words(&article) { article } else { page }
}

/// Share of visible Markdown text that sits inside `[link text](url)`.
pub fn link_ratio(md: &str) -> f64 {
    let (mut link, mut total) = (0usize, 0usize);
    let mut rest = md;
    while !rest.is_empty() {
        // A link: `[text](url)` — count the text as link text, skip the URL.
        if let Some(after) = rest.strip_prefix('[') {
            if let Some((text, tail)) = after.split_once("](") {
                if let Some((_, after_url)) = tail.split_once(')') {
                    let n = text.chars().filter(|c| !c.is_whitespace()).count();
                    link += n;
                    total += n;
                    rest = after_url;
                    continue;
                }
            }
        }
        let mut chars = rest.chars();
        if chars.next().is_some_and(|c| !c.is_whitespace()) {
            total += 1;
        }
        rest = chars.as_str();
    }
    if total == 0 { 0.0 } else { link as f64 / total as f64 }
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
    fn reads_pdf_text() {
        let doc = pdf_to_doc(include_bytes!("fixtures/hello.pdf"), "https://e.com/papers/hello.pdf").unwrap();
        assert!(doc.markdown.contains("Hello PDF World"), "{}", doc.markdown);
        assert!(doc.markdown.starts_with("# hello.pdf"));
        assert_eq!(doc.data["extraction"], "pdf");
        assert_eq!(doc.data["pages"], 1);
    }

    #[test]
    fn broken_pdf_is_an_error_not_a_crash() {
        assert!(pdf_to_doc(b"%PDF-1.4\n1 0 obj << garbage", "https://e.com/x.pdf").is_err());
    }

    #[test]
    fn recognizes_pdf_by_type_or_signature() {
        assert!(is_pdf("application/pdf", b""));
        assert!(is_pdf("application/octet-stream", b"%PDF-1.7 ..."));
        assert!(!is_pdf("text/html", b"<html>"));
    }

    #[test]
    fn links_section_lists_unique_absolute_web_links() {
        let html = r##"<body><a href="/a">Alpha</a> <a href="https://e.com/a">Alpha again</a>
            <a href="b.html">  Beta
            link </a> <a href="#top">Top</a> <a href="javascript:x()">JS</a> <a href="mailto:a@b.c">Mail</a>
            <a href="https://other.org/x?y=1">Other</a> <a href="/empty"></a></body>"##;
        let (md, links) = links_section(html, "https://e.com/dir/page", 50);
        assert!(md.starts_with("## Links\n"));
        assert_eq!(links.len(), 4, "{md}");
        assert!(md.contains("- [Alpha](https://e.com/a)"));
        assert!(md.contains("- [Beta link](https://e.com/dir/b.html)"));
        assert!(md.contains("- [Other](https://other.org/x?y=1)"));
        assert!(md.contains("https://e.com/empty"), "a link without text keeps its URL");
        assert!(!md.contains("javascript") && !md.contains("mailto") && !md.contains("#top"));
        assert_eq!(links_section(html, "https://e.com/", 2).1.len(), 2);
    }

    #[test]
    fn link_ratio_measures_how_much_text_is_links() {
        assert!(link_ratio("Plain prose with no links at all.") < 0.01);
        let nav =
            "- [Senior DevOps](https://e.com/1) Luxoft\n- [Cloud Engineer](https://e.com/2) Atos\n- [DevOps Engineer](https://e.com/3)";
        assert!(link_ratio(nav) > 0.5, "{}", link_ratio(nav));
        let article = format!("{} see [docs](https://e.com/d).", "Long paragraph of real content. ".repeat(20));
        assert!(link_ratio(&article) < 0.1);
    }

    #[test]
    fn link_heavy_readability_picks_fall_back_to_the_page() {
        // Readability may pick a "similar jobs" list over the real description.
        let similar: String =
            (0..30).map(|i| format!(r#"<li><a href="/jobs/{i}">Senior DevOps Engineer position number {i} at Example</a></li>"#)).collect();
        let html = format!(
            r#"<html><head><title>Cloud DevOps Engineer</title></head><body>
            <div class="description"><p>We are hiring a Cloud DevOps Engineer to build AWS infrastructure.</p></div>
            <section><h2>Similar jobs</h2><ul>{similar}</ul></section></body></html>"#
        );
        let doc = extract(&html, "https://jobs.example.com/view/1").unwrap();
        assert!(doc.markdown.contains("We are hiring a Cloud DevOps Engineer"), "{}", doc.markdown);
    }

    const JOB: &str = r#"<html><head><title>Job | Board</title>
<script type="application/ld+json">{"@context":"http://schema.org","@type":"JobPosting","title":"Cloud DevOps Engineer",
"datePosted":"2026-10-07T10:00:00.000Z","employmentType":"FULL_TIME",
"hiringOrganization":{"@type":"Organization","name":"Renesas Electronics"},
"jobLocation":{"@type":"Place","address":{"@type":"PostalAddress","addressLocality":"Wrocław","addressCountry":"PL"}},
"description":"&lt;strong&gt;About the Role&lt;/strong&gt;&lt;br&gt;We are hiring a Cloud DevOps Engineer.&lt;ul&gt;&lt;li&gt;Build AWS infrastructure&lt;/li&gt;&lt;li&gt;Own Terraform&lt;/li&gt;&lt;/ul&gt;"}</script>
</head><body><nav>Jobs Menu</nav><div>Similar jobs: <a href="/1">Other job</a></div></body></html>"#;

    #[test]
    fn job_postings_come_from_structured_data() {
        let doc = extract(JOB, "https://jobs.example.com/view/1").unwrap();
        assert_eq!(doc.data["extraction"], "job-posting");
        let md = &doc.markdown;
        assert!(md.starts_with("# Cloud DevOps Engineer"), "{md}");
        for part in
            ["Renesas Electronics", "Wrocław, PL", "FULL_TIME", "2026-10-07", "**About the Role**", "We are hiring", "Own Terraform"]
        {
            assert!(md.contains(part), "missing {part}: {md}");
        }
        assert!(!md.contains("Similar jobs"));
    }

    const CONSENT: &str = r#"<html><head><title>Jobs</title></head><body>
<div id="onetrust-consent-sdk"><p>We use cookies to improve your experience. Accept all? Privacy policy.</p></div>
<div class="cookie-banner-x"><button>Accept all</button> Our website uses cookies. Decline all. Customize.</div>
<div role="dialog" aria-label="Cookie consent"><p>This website uses cookies. Save &amp; Close.</p></div>
<main><h1>DevSecOps Engineer</h1><p>Real listing content about securing CI/CD pipelines in Wrocław.</p></main>
<section class="cookie-recipe"><h2>Grandma's cookies</h2><p>Mix butter and sugar, then accept that the dough is sticky.
Bake twelve minutes at 180 degrees, cool on a rack, and share with the whole office team.</p></section>
</body></html>"#;

    #[test]
    fn consent_banners_are_removed_before_extraction() {
        let cleaned = strip_consent(CONSENT);
        assert!(!cleaned.contains("We use cookies"), "OneTrust");
        assert!(!cleaned.contains("Our website uses cookies"), "class*=cookie banner");
        assert!(!cleaned.contains("This website uses cookies"), "consent dialog");
        assert!(cleaned.contains("Real listing content"));
        assert!(cleaned.contains("Grandma's cookies"), "content that merely mentions cookies stays");
        let doc = extract(CONSENT, "https://jobs.example.com/").unwrap();
        assert!(!doc.markdown.contains("Accept all"), "{}", doc.markdown);
    }

    #[test]
    fn a_thin_article_loses_to_a_richer_whole_page() {
        let promo = Doc::new("web", None, "# Jobs\n\nCreate an account and search smarter.".into(), json!({ "extraction": "article" }));
        let listing: String = (0..30).map(|i| format!("- DevSecOps Engineer {i} at Company {i}, Wrocław, B2B\n")).collect();
        let page = Doc::new("web", None, format!("# Jobs\n\n{listing}"), json!({ "extraction": "page" }));
        assert_eq!(richer(promo.clone(), page.clone()).data["extraction"], "page");
        let article = Doc::new("web", None, "word ".repeat(400), json!({ "extraction": "article" }));
        assert_eq!(richer(article, page).data["extraction"], "article", "a real article stays");
    }

    #[test]
    fn content_hidden_behind_a_consent_modal_comes_back() {
        // Modal libraries mark the whole app aria-hidden while the banner is open.
        let listing: String =
            (0..20).map(|i| format!("<li><a href=\"/o/{i}\">DevSecOps Engineer {i}</a> Company {i}, Wrocław, 20 000 PLN</li>")).collect();
        let html = format!(
            r#"<html><head><title>Offers</title></head><body>
            <div aria-hidden="true"><main><h1>Offers in Wrocław</h1><ul>{listing}</ul></main></div>
            <div role="dialog"><p>This website uses cookies. Accept all or Decline all.</p></div>
            <span aria-hidden="true">★</span></body></html>"#
        );
        let doc = extract(&html, "https://jobs.example.com/").unwrap();
        assert!(doc.markdown.contains("DevSecOps Engineer 7"), "{}", doc.markdown);
        assert!(!doc.markdown.contains("uses cookies"));
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
