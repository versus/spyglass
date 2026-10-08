//! The only HTTP client in the program. SSRF-safe by construction:
//! - every URL (including each redirect hop) passes `validate::public_url`;
//! - DNS answers are filtered by our resolver, and the connection goes to the
//!   addresses we checked, so DNS rebinding cannot reach private networks;
//! - environment proxies are ignored; bodies are size-capped after decompression.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::Value;
use url::Url;

use crate::validate;

pub const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
const USER_AGENT: &str = concat!("spyglass/", env!("CARGO_PKG_VERSION"));

pub struct Fetched {
    pub url: Url,
    pub content_type: String,
    pub body: String,
}

pub struct Net {
    /// Page fetches: follows redirects, re-validating every hop.
    client: reqwest::Client,
    /// API calls carrying keys in custom headers: never follows redirects, because
    /// reqwest only strips standard auth headers on a cross-host redirect.
    api: reqwest::Client,
    /// Tests only: allow loopback servers.
    allow_private: bool,
}

impl Net {
    pub fn new() -> Result<Self> {
        Self::build(false)
    }

    fn build(allow_private: bool) -> Result<Self> {
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if !allow_private && validate::public_url(attempt.url().as_str()).is_err() {
                attempt.error("redirect to a non-public URL blocked")
            } else {
                attempt.follow()
            }
        });
        let builder = |policy| {
            let mut b = reqwest::Client::builder()
                .no_proxy()
                .redirect(policy)
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30));
            if !allow_private {
                b = b.dns_resolver(Arc::new(PublicOnlyResolver));
            }
            b.build().context("building HTTP client")
        };
        Ok(Self { client: builder(redirects)?, api: builder(reqwest::redirect::Policy::none())?, allow_private })
    }

    fn target(&self, url: &str) -> Result<Url> {
        if self.allow_private {
            return Ok(Url::parse(url)?);
        }
        validate::public_url(url)
    }

    pub async fn get(&self, url: &str, headers: &[(&str, &str)], max_bytes: usize) -> Result<Fetched> {
        let req = with_headers(self.client.get(self.target(url)?), headers);
        self.send(req, max_bytes).await
    }

    pub async fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        let req = with_headers(self.api.get(self.target(url)?), headers);
        let fetched = self.send(req, DEFAULT_MAX_BYTES).await?;
        serde_json::from_str(&fetched.body).context("response is not valid JSON")
    }

    pub async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
        let req = with_headers(self.api.post(self.target(url)?).json(body), headers);
        let fetched = self.send(req, DEFAULT_MAX_BYTES).await?;
        serde_json::from_str(&fetched.body).context("response is not valid JSON")
    }

    async fn send(&self, req: reqwest::RequestBuilder, max_bytes: usize) -> Result<Fetched> {
        let resp = req.send().await.map_err(describe)?;
        let status = resp.status();
        let url = resp.url().clone();
        let content_type =
            resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let bytes = read_capped(resp.bytes_stream(), max_bytes).await?;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if !status.is_success() {
            let excerpt: String = body.chars().take(300).collect();
            bail!("HTTP {status} from {}: {}", url.host_str().unwrap_or(""), excerpt.trim());
        }
        Ok(Fetched { url, content_type, body })
    }
}

fn with_headers(mut req: reqwest::RequestBuilder, headers: &[(&str, &str)]) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req
}

/// Flatten reqwest's nested error chain into one readable line.
fn describe(err: reqwest::Error) -> anyhow::Error {
    let mut msg = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    anyhow::anyhow!(msg)
}

struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let ok = check_resolved(&host, addrs)?;
            Ok(Box::new(ok.into_iter()) as Addrs)
        })
    }
}

/// Plain DNS lookup (for the browser guard, which cannot use reqwest's resolver).
pub async fn resolve(host: &str) -> Option<Vec<std::net::IpAddr>> {
    let addrs = tokio::net::lookup_host((host, 0)).await.ok()?;
    Some(addrs.map(|a| a.ip()).collect())
}

/// Keep resolved addresses only if *all* of them are public.
fn check_resolved(host: &str, addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>> {
    if addrs.is_empty() {
        bail!("{host} did not resolve");
    }
    if let Some(bad) = addrs.iter().find(|a| !validate::is_public_ip(a.ip())) {
        bail!("{host} resolves to non-public address {}", bad.ip());
    }
    Ok(addrs)
}

/// Collect a byte stream, failing as soon as it exceeds `max` bytes.
async fn read_capped<S, B, E>(mut stream: S, max: usize) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        // Check before copying: one decompressed chunk can be large.
        if out.len() + chunk.as_ref().len() > max {
            bail!("response is larger than {max} bytes");
        }
        out.extend_from_slice(chunk.as_ref());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn resolver_rejects_any_private_answer() {
        assert!(check_resolved("e.com", vec![sa("93.184.216.34:0")]).is_ok());
        assert!(check_resolved("e.com", vec![sa("93.184.216.34:0"), sa("127.0.0.1:0")]).is_err());
        assert!(check_resolved("e.com", vec![sa("[::ffff:192.168.0.1]:0")]).is_err());
        assert!(check_resolved("e.com", vec![]).is_err());
    }

    #[tokio::test]
    async fn read_capped_enforces_limit() {
        let ok = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![1u8; 10]), Ok(vec![2u8; 10])]);
        assert_eq!(read_capped(ok, 20).await.unwrap().len(), 20);
        let big = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![1u8; 10]), Ok(vec![2u8; 11])]);
        assert!(read_capped(big, 20).await.is_err());
    }

    /// Tiny HTTP server: answers every request with `response`, counts connections.
    async fn serve(response: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        (format!("{}:{}", addr.ip(), addr.port()), hits)
    }

    #[tokio::test]
    async fn api_calls_with_keys_never_follow_redirects() {
        let (target, target_hits) = serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}".into()).await;
        let port = target.rsplit(':').next().unwrap();
        // Different host name than the redirecting server: a cross-host redirect.
        let redirect = format!("HTTP/1.1 302 Found\r\nlocation: http://localhost:{port}/steal\r\ncontent-length: 0\r\n\r\n");
        let (origin, _) = serve(redirect).await;
        let net = Net::build(true).unwrap();
        let key = [("X-Subscription-Token", "SECRET")];
        assert!(net.get_json(&format!("http://{origin}/api"), &key).await.is_err());
        assert!(net.post_json(&format!("http://{origin}/api"), &key, &serde_json::json!({})).await.is_err());
        assert_eq!(target_hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the key must not reach the redirect target");
        // Plain page fetches still follow redirects.
        assert!(net.get(&format!("http://{origin}/page"), &[], 1024).await.is_ok());
        assert_eq!(target_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refuses_private_targets_before_any_io() {
        let net = Net::new().unwrap();
        for u in ["http://127.0.0.1:9/", "http://localhost/", "http://169.254.169.254/", "file:///etc/passwd"] {
            assert!(net.get(u, &[], 1024).await.is_err(), "{u}");
        }
    }
}
