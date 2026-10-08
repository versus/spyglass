//! `spyglass web read --via-jina <url>` — last resort, only when the user agrees:
//! the free Jina Reader (r.jina.ai, owned by Elastic, US) fetches the page for us.
//! The URL leaves this machine, so links that look private are refused.

use anyhow::{Result, bail};
use serde_json::json;
use url::Url;

use crate::net::{DEFAULT_MAX_BYTES, Net};
use crate::output::Doc;
use crate::validate;

const READER: &str = "https://r.jina.ai/";

/// Words in query parameter names that suggest a private or authenticated link.
/// Matched as whole words of the name (`api_key`, `X-Amz-Signature`), not substrings (`keyword`).
const SECRET_WORDS: &[&str] = &[
    "token",
    "key",
    "apikey",
    "sig",
    "signature",
    "secret",
    "session",
    "sessionid",
    "sid",
    "password",
    "passwd",
    "pwd",
    "auth",
    "code",
    "credential",
    "credentials",
    "otp",
    "jwt",
];

pub async fn read(net: &Net, input: &str) -> Result<Doc> {
    let url = validate::public_url(input)?;
    refuse_private(&url)?;
    let body = net.get(&format!("{READER}{url}"), &[("Accept", "text/plain")], DEFAULT_MAX_BYTES).await?.text();
    parse(&body, url.as_str())
}

/// Never hand links with secret-looking parameters (or credentials) to a third party.
pub fn refuse_private(url: &Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        bail!("this link carries credentials; not sending it to Jina");
    }
    for (name, _) in url.query_pairs() {
        let lower = name.to_lowercase();
        if lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| SECRET_WORDS.contains(&word)) {
            bail!("this link looks private (parameter `{name}`); not sending it to Jina");
        }
    }
    Ok(())
}

/// Jina answers with "Title: …", "URL Source: …", "Markdown Content:" then the page.
pub fn parse(body: &str, url: &str) -> Result<Doc> {
    let title = body.lines().find_map(|l| l.strip_prefix("Title: ")).unwrap_or("").trim();
    if body.contains("requiring CAPTCHA") || title.to_lowercase().starts_with("just a moment") {
        bail!("Jina was blocked by the site too (bot check)");
    }
    let content = body.split_once("Markdown Content:").map(|(_, c)| c).unwrap_or(body).trim();
    let markdown = if title.is_empty() { content.to_string() } else { format!("# {title}\n\n{content}") };
    Ok(Doc::new("jina", Some(url.to_string()), markdown, json!({ "title": title, "via": "r.jina.ai", "markdown": content })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_looking_links_are_refused() {
        for bad in [
            "https://e.com/reset?token=abc",
            "https://docs.example.com/d/1?usp=sharing&key=K",
            "https://s3.example.com/f.pdf?X-Amz-Signature=x",
            "https://e.com/cb?code=123&state=x",
            "https://e.com/p?session_id=1",
        ] {
            assert!(refuse_private(&Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
        for bad in [
            "https://e.com/a?api_key=1",
            "https://e.com/a?apikey=1",
            "https://e.com/a?access-token=1",
            "https://e.com/a?sessionid=1",
            "https://e.com/a?jwt=x",
        ] {
            assert!(refuse_private(&Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
        for ok in [
            "https://blog.rust-lang.org/2026/10/01/Rust-1.99.0/",
            "https://e.com/search?q=rust&page=2",
            "https://justjoin.it/job-offers/wroclaw/devops?keyword=devsecops",
            "https://e.com/?monkey=1&authuser=0&barcode=5",
        ] {
            assert!(refuse_private(&Url::parse(ok).unwrap()).is_ok(), "{ok}");
        }
    }

    #[test]
    fn parses_reader_output() {
        let body = "Title: Rust 1.99\n\nURL Source: https://blog.rust-lang.org/x\n\nPublished Time: 2026-10-01\n\nMarkdown Content:\nThe Rust team is happy...\n\nMore text.";
        let doc = parse(body, "https://blog.rust-lang.org/x").unwrap();
        assert_eq!(doc.source, "jina");
        assert!(doc.markdown.starts_with("# Rust 1.99\n\nThe Rust team is happy"), "{}", doc.markdown);
        assert!(!doc.markdown.contains("URL Source:"));
        assert_eq!(doc.data["via"], "r.jina.ai");
    }

    #[test]
    fn jina_captcha_pages_are_errors() {
        let body = "Title: Just a moment...\n\nURL Source: https://e.com/\n\nWarning: Target URL returned error 403: Forbidden\nWarning: This page maybe requiring CAPTCHA, please make sure you are authorized to access this page.\n\nMarkdown Content:\nPerforming security verification";
        let err = parse(body, "https://e.com/").unwrap_err().to_string();
        assert!(err.contains("Jina") && err.contains("blocked"), "{err}");
    }
}
