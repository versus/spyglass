//! Input validation at the trust boundary: every value the agent passes is checked here.

use std::net::IpAddr;

use anyhow::{Result, bail};
use url::Url;

const BLOCKED_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".lan", ".home.arpa", ".localdomain"];

/// Percent-encode a query-string component.
pub fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Parse a URL that must point to a public HTTP(S) host.
pub fn public_url(input: &str) -> Result<Url> {
    if input.chars().any(char::is_control) {
        bail!("only public http(s) URLs are allowed");
    }
    let input = input.trim();
    if input.is_empty() || input.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\') {
        bail!("only public http(s) URLs are allowed");
    }
    let candidate = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = Url::parse(&candidate).map_err(|_| anyhow::anyhow!("invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http(s) URLs are allowed");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with credentials are not allowed");
    }
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost" || !d.contains('.') || BLOCKED_SUFFIXES.iter().any(|s| d.ends_with(s)) {
                bail!("non-public host: {d}");
            }
        }
        Some(url::Host::Ipv4(ip)) if !is_public_ip(ip.into()) => bail!("non-public address: {ip}"),
        Some(url::Host::Ipv6(ip)) if !is_public_ip(ip.into()) => bail!("non-public address: {ip}"),
        Some(_) => {}
        None => bail!("URL has no host"),
    }
    Ok(url)
}

/// Is this address routable on the public internet? Conservative: unknown special ranges are rejected.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64/10
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || (a == 192 && b == 0 && c == 0)) // IETF protocol assignments
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link local
                || (seg[0] & 0xffc0) == 0xfec0 // site local (deprecated, still routed internally)
                || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation
                || (seg[0] == 0x2001 && seg[1] < 0x0200) // 2001::/23 IETF protocol assignments (Teredo, benchmarking, ORCHID…)
                || (seg[0] & 0xfff0) == 0x3ff0 // 3fff::/20 documentation
                || seg[0] == 0x5f00 // 5f00::/16 SRv6 SIDs
                || seg[0] == 0x2002 // 6to4 (may embed private v4)
                || (seg[0] == 0x64 && seg[1] == 0xff9b) // NAT64
                || (seg[0] == 0x100 && seg[1..4] == [0, 0, 0]) // discard
                || seg[0] == 0) // ::/16 incl. IPv4-compatible
        }
    }
}

/// Free-text query: non-empty, bounded, no control characters.
pub fn query(input: &str) -> Result<String> {
    let q = input.trim();
    if q.is_empty() {
        bail!("query is empty");
    }
    if q.chars().count() > 500 {
        bail!("query is longer than 500 characters");
    }
    if q.chars().any(char::is_control) {
        bail!("query contains control characters");
    }
    Ok(q.to_string())
}

fn is_name(s: &str, min: usize, max: usize, extra: &[char]) -> bool {
    (min..=max).contains(&s.chars().count()) && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

/// `owner/repo` on GitHub (a github.com URL is accepted too).
pub fn github_repo(input: &str) -> Result<(String, String)> {
    let mut s = input.trim();
    if s.contains("://") {
        let url = url_on(s, &["github.com"])?;
        let path = url.path().trim_matches('/').to_string();
        let mut parts = path.splitn(3, '/');
        let (o, r) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        return github_repo(&format!("{o}/{r}"));
    }
    s = s.strip_suffix(".git").unwrap_or(s);
    let Some((owner, repo)) = s.split_once('/') else { bail!("expected owner/repo") };
    let owner_ok = is_name(owner, 1, 39, &['-']) && !owner.starts_with('-');
    let repo_ok = is_name(repo, 1, 100, &['-', '_', '.']) && repo != "." && repo != "..";
    if !owner_ok || !repo_ok {
        bail!("invalid GitHub repository: expected owner/repo");
    }
    Ok((owner.to_string(), repo.to_string()))
}

/// Repository-relative file path.
pub fn repo_path(input: &str) -> Result<String> {
    let p = input.trim();
    let bad_char = |c: char| c.is_control() || matches!(c, '\\' | '?' | '#');
    if p.is_empty() || p.len() > 512 || p.starts_with('/') || p.chars().any(bad_char) {
        bail!("invalid repository path");
    }
    if p.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        bail!("invalid repository path");
    }
    Ok(p.to_string())
}

/// X/Twitter handle without `@`.
pub fn x_handle(input: &str) -> Result<String> {
    let h = input.trim().trim_start_matches('@');
    if !is_name(h, 1, 15, &['_']) {
        bail!("invalid X handle");
    }
    Ok(h.to_string())
}

/// Subreddit name without `r/`.
pub fn subreddit(input: &str) -> Result<String> {
    let s = input.trim().trim_start_matches('/');
    let s = s.strip_prefix("r/").unwrap_or(s);
    if !is_name(s, 2, 21, &['_']) {
        bail!("invalid subreddit name");
    }
    Ok(s.to_string())
}

/// A public URL whose host is one of `domains` or a subdomain of one.
pub fn url_on(input: &str, domains: &[&str]) -> Result<Url> {
    let url = public_url(input)?;
    let host = url.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    if !domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
        bail!("URL must be on {}", domains.join(", "));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_public_urls() {
        assert_eq!(public_url("https://example.com/a?b=1").unwrap().as_str(), "https://example.com/a?b=1");
        assert_eq!(public_url("example.com/x").unwrap().as_str(), "https://example.com/x");
        assert!(public_url("http://8.8.8.8/").is_ok());
        assert!(public_url("https://[2606:4700:4700::1111]/").is_ok());
    }

    #[test]
    fn rejects_non_public_targets() {
        for bad in [
            "http://localhost/",
            "http://LOCALHOST./",
            "http://foo.localhost/",
            "http://printer.local/",
            "http://db.internal/",
            "http://router.lan/",
            "http://nas.home.arpa/",
            "http://intranet/",
            "http://127.0.0.1/",
            "http://0x7f.1/",
            "http://2130706433/",
            "http://0/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://172.16.5.4/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
        ] {
            assert!(public_url(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn rejects_bad_schemes_and_shapes() {
        for bad in [
            "file:///etc/passwd",
            "gopher://example.com/",
            "ftp://example.com/",
            "javascript:alert(1)",
            "https://user:pass@example.com/",
            "https://example.com@evil.com/",
            "https://exa mple.com/",
            "https://example.com/\n",
            "",
            "https://example.com:99999/",
        ] {
            assert!(public_url(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn public_ip_classification() {
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2a00:1450:4001::1".parse().unwrap()));
        for ip in [
            "0.0.0.0",
            "127.0.0.53",
            "10.1.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} must not be public");
        }
    }

    #[test]
    fn internal_and_reserved_ipv6_ranges_are_not_public() {
        for ip in ["fec0::1", "feff::1", "2001:2::1", "2001:10::1", "2001:20::1", "2001:1ff::1", "3fff::1", "5f00::1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} must not be public");
        }
        // Real global unicast still passes (Google, Cloudflare).
        for ip in ["2001:4860:4860::8888", "2606:4700:4700::1111", "2a00:1450:4001::1"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[test]
    fn query_rules() {
        assert_eq!(query("  rust async  ").unwrap(), "rust async");
        assert_eq!(query("a \"quoted\" $(x) `y`; --exec=z").unwrap(), "a \"quoted\" $(x) `y`; --exec=z");
        assert!(query("").is_err());
        assert!(query("   ").is_err());
        assert!(query("a\nb").is_err());
        assert!(query("a\0b").is_err());
        assert!(query(&"x".repeat(501)).is_err());
    }

    #[test]
    fn github_repo_rules() {
        assert_eq!(github_repo("rust-lang/rust").unwrap(), ("rust-lang".into(), "rust".into()));
        assert_eq!(github_repo("https://github.com/tokio-rs/tokio").unwrap(), ("tokio-rs".into(), "tokio".into()));
        assert_eq!(github_repo("a/b.c_d-e").unwrap(), ("a".into(), "b.c_d-e".into()));
        for bad in ["rust", "a/b/c", "../x", "a/..", "a/.", "-a/b", "a b/c", "a/b?x=1", "https://evil.com/a/b"] {
            assert!(github_repo(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn repo_path_rules() {
        assert_eq!(repo_path("src/main.rs").unwrap(), "src/main.rs");
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "a//b", "a\\b", "a?b", "a#b", "a\nb"] {
            assert!(repo_path(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn handle_rules() {
        assert_eq!(x_handle("@jack").unwrap(), "jack");
        assert_eq!(x_handle("Some_User1").unwrap(), "Some_User1");
        assert!(x_handle("a-b").is_err());
        assert!(x_handle(&"a".repeat(16)).is_err());
        assert_eq!(subreddit("r/rust").unwrap(), "rust");
        assert_eq!(subreddit("LocalLLaMA").unwrap(), "LocalLLaMA");
        assert!(subreddit("a").is_err());
        assert!(subreddit("../x").is_err());
    }

    #[test]
    fn url_on_domain_rules() {
        let yt = ["youtube.com", "youtu.be"];
        assert!(url_on("https://www.youtube.com/watch?v=abc", &yt).is_ok());
        assert!(url_on("https://youtu.be/abc", &yt).is_ok());
        assert!(url_on("https://youtube.com.evil.com/x", &yt).is_err());
        assert!(url_on("https://evilyoutube.com/x", &yt).is_err());
        assert!(url_on("https://youtube.com@evil.com/x", &yt).is_err());
    }
}
