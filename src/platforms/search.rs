//! `spyglass search <query>` — web search via Brave Search API or Exa (API key in keyring).

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::net::Net;
use crate::output::{Doc, oneline, strip_tags, unescape};
use crate::secrets::{self, Secret};
use crate::validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    /// DuckDuckGo HTML results: no key, unofficial (may rate-limit).
    Ddg,
    Brave,
    Exa,
}

/// Explicit choice wins; then a configured API key; otherwise keyless DuckDuckGo.
pub fn choose(explicit: Option<Provider>, has_brave: bool, has_exa: bool) -> Result<Provider> {
    match (explicit, has_brave, has_exa) {
        (Some(Provider::Brave), false, _) => bail!("Brave key missing: run `spyglass secrets set brave-api-key`"),
        (Some(Provider::Exa), _, false) => bail!("Exa key missing: run `spyglass secrets set exa-api-key`"),
        (Some(p), _, _) => Ok(p),
        (None, true, _) => Ok(Provider::Brave),
        (None, false, true) => Ok(Provider::Exa),
        (None, false, false) => Ok(Provider::Ddg),
    }
}

pub async fn search(net: &Net, query: &str, limit: usize, explicit: Option<Provider>) -> Result<Doc> {
    let q = validate::query(query)?;
    let brave = secrets::get(Secret::BraveApiKey);
    let exa = secrets::get(Secret::ExaApiKey);
    let (md, data) = match choose(explicit, brave.is_some(), exa.is_some())? {
        // DuckDuckGo rejects non-browser TLS clients, so it runs in the agent browser.
        Provider::Ddg => return crate::browser::client::doc("search", "ddg", json!({ "query": q, "limit": limit })).await,
        Provider::Brave => {
            let key = brave.unwrap_or_default();
            let q = validate::enc(&q);
            let url = format!("https://api.search.brave.com/res/v1/web/search?q={q}&count={limit}");
            let v = net.get_json(&url, &[("Accept", "application/json"), ("X-Subscription-Token", &key)]).await?;
            (render_brave(&v), v["web"]["results"].clone())
        }
        Provider::Exa => {
            let key = exa.unwrap_or_default();
            let body = json!({ "query": q, "numResults": limit, "contents": { "text": { "maxCharacters": 400 } } });
            let v = net.post_json("https://api.exa.ai/search", &[("x-api-key", &key)], &body).await?;
            (render_exa(&v), v["results"].clone())
        }
    };
    Ok(Doc::new("search", None, md, data))
}

/// Real target of a DuckDuckGo result link; `None` for ads, trackers and non-http targets.
pub fn ddg_target(href: &str) -> Option<String> {
    let full = if href.starts_with("//") { format!("https:{href}") } else { href.to_string() };
    let url = url::Url::parse(&full).ok()?;
    let target = match url.host_str() {
        Some(h) if h == "duckduckgo.com" || h.ends_with(".duckduckgo.com") => {
            if url.path() != "/l/" {
                return None; // ads and click trackers
            }
            url.query_pairs().find(|(k, _)| k == "uddg")?.1.into_owned()
        }
        _ => full,
    };
    if !(target.starts_with("https://") || target.starts_with("http://")) {
        return None;
    }
    validate::public_url(&target).ok().map(|u| u.to_string())
}

/// Organic results from DuckDuckGo's HTML endpoint.
pub fn parse_ddg(html: &str) -> Result<Vec<Value>> {
    if html.contains("anomaly-modal") || html.contains("challenge-form") {
        bail!("DuckDuckGo asked for a captcha (rate limit). Wait a bit, or use `--provider brave|exa` with an API key");
    }
    let doc = dom_query::Document::from(html);
    let mut out = Vec::new();
    for result in doc.select("div.result").iter() {
        if result.has_class("result--ad") {
            continue;
        }
        let link = result.select("a.result__a");
        let Some(url) = link.attr("href").and_then(|h| ddg_target(&h)) else { continue };
        let title = oneline(&link.text(), 200);
        let snippet = oneline(&result.select(".result__snippet").text(), 300);
        out.push(json!({ "title": title, "url": url, "snippet": snippet }));
    }
    Ok(out)
}

pub fn render_ddg(results: &[Value]) -> String {
    render_results(results, "snippet", "date")
}

pub fn render_brave(v: &Value) -> String {
    render_results(v["web"]["results"].as_array().map(Vec::as_slice).unwrap_or_default(), "description", "age")
}

pub fn render_exa(v: &Value) -> String {
    render_results(v["results"].as_array().map(Vec::as_slice).unwrap_or_default(), "text", "publishedDate")
}

/// One list format for every provider: `- [title](url) date` + snippet line.
fn render_results(items: &[Value], snippet_key: &str, date_key: &str) -> String {
    let mut md = String::new();
    for r in items {
        let date = r[date_key].as_str().map(|d| d.get(..10).filter(|_| d.contains('T')).unwrap_or(d)).unwrap_or("");
        let snippet = oneline(&unescape(&strip_tags(r[snippet_key].as_str().unwrap_or(""))), 300);
        md.push_str(&format!(
            "- [{}]({}) {date}\n  {snippet}\n",
            oneline(r["title"].as_str().unwrap_or(""), 160),
            r["url"].as_str().unwrap_or("")
        ));
    }
    if md.is_empty() { "No results.".into() } else { md }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_choice() {
        assert_eq!(choose(None, false, false).unwrap(), Provider::Ddg, "works without any key");
        assert_eq!(choose(None, true, true).unwrap(), Provider::Brave, "a configured key signals preference");
        assert_eq!(choose(None, false, true).unwrap(), Provider::Exa);
        assert_eq!(choose(Some(Provider::Ddg), true, true).unwrap(), Provider::Ddg);
        assert_eq!(choose(Some(Provider::Exa), true, true).unwrap(), Provider::Exa);
        let err = choose(Some(Provider::Brave), false, true).unwrap_err().to_string();
        assert!(err.contains("spyglass secrets set brave-api-key"), "{err}");
    }

    const DDG: &str = include_str!("fixtures/ddg_results.html");

    #[test]
    fn parses_ddg_results_and_skips_ads() {
        let results = parse_ddg(DDG).unwrap();
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0]["title"], "tokio::runtime - Rust - Docs.rs");
        assert_eq!(results[0]["url"], "https://docs.rs/tokio/latest/tokio/runtime/");
        assert!(results[0]["snippet"].as_str().unwrap().starts_with("The Tokio runtime."));
        assert!(results.iter().all(|r| !r["title"].as_str().unwrap().contains("Sponsored")));
        let md = render_ddg(&results);
        assert!(md.contains("[tokio::runtime - Rust - Docs.rs](https://docs.rs/tokio/latest/tokio/runtime/)"));
    }

    #[test]
    fn decodes_ddg_redirects() {
        assert_eq!(ddg_target("//duckduckgo.com/l/?uddg=https%3A%2F%2Fe.com%2Fa%3Fb%3D1&rut=x").as_deref(), Some("https://e.com/a?b=1"));
        assert_eq!(ddg_target("https://direct.example/page").as_deref(), Some("https://direct.example/page"));
        assert_eq!(ddg_target("//duckduckgo.com/l/?uddg=javascript%3Aalert(1)"), None);
        assert_eq!(ddg_target("https://duckduckgo.com/y.js?ad_domain=x"), None, "ad click-trackers are dropped");
    }

    #[test]
    fn detects_ddg_challenge_and_empty_results() {
        let challenge = r#"<html><body><div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div><form id="challenge-form"></form></body></html>"#;
        let err = parse_ddg(challenge).unwrap_err().to_string();
        assert!(err.contains("DuckDuckGo") && err.contains("--provider"), "{err}");
        let empty = r#"<html><body><div class="no-results">No results.</div></body></html>"#;
        assert!(parse_ddg(empty).unwrap().is_empty());
    }

    #[test]
    fn renders_brave_results() {
        let v = json!({"web":{"results":[{"title":"Tokio","url":"https://tokio.rs/","description":"An <strong>asynchronous</strong> runtime","age":"2 days ago"}]}});
        let md = render_brave(&v);
        assert!(md.contains("[Tokio](https://tokio.rs/)"));
        assert!(md.contains("An asynchronous runtime"), "{md}");
        assert!(md.contains("2 days ago"));
    }

    #[test]
    fn renders_exa_results() {
        let v = json!({"results":[{"title":"Async Rust","url":"https://e.com/a","publishedDate":"2026-09-30T00:00:00.000Z","text":"Long text\nabout async"}]});
        let md = render_exa(&v);
        assert!(md.contains("[Async Rust](https://e.com/a)") && md.contains("2026-09-30") && md.contains("Long text about async"));
        assert_eq!(render_exa(&json!({"results":[]})), "No results.");
    }
}
