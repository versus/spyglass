//! One browser tab driven by a fixed scenario: network allowlist, navigation,
//! JSON response capture, evaluation of our own (never agent-supplied) scripts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::cdp::{Cdp, Event};

/// Which hosts a tab may talk to.
#[derive(Debug, Clone)]
pub enum Allow {
    /// Only these domains and their subdomains (platform scenarios).
    Domains(&'static [&'static str]),
    /// Any public http(s) host (arbitrary pages in the cookie-less context).
    AnyPublic,
}

/// Decide whether the browser may perform a request. Media is always blocked.
pub fn request_allowed(url: &str, resource_type: &str, allow: &Allow) -> bool {
    if matches!(resource_type, "Image" | "Media" | "Font") {
        return false;
    }
    if url.starts_with("data:") || url.starts_with("blob:") {
        return true;
    }
    match allow {
        Allow::AnyPublic => crate::validate::public_url(url).is_ok() && url.starts_with("http"),
        Allow::Domains(domains) => url.starts_with("https://") && crate::validate::url_on(url, domains).is_ok(),
    }
}

/// Full decision: the static rules above, then the host must resolve to public
/// addresses only (Chrome does its own DNS, so a name alone proves nothing).
pub async fn request_permitted<R, F>(url: &str, resource_type: &str, allow: &Allow, resolve: R) -> bool
where
    R: Fn(String) -> F,
    F: std::future::Future<Output = Option<Vec<std::net::IpAddr>>>,
{
    if !request_allowed(url, resource_type, allow) {
        return false;
    }
    // data:/blob: have no host; literal IPs were already checked by the static rules.
    let Some(url::Host::Domain(host)) = url::Url::parse(url).ok().and_then(|u| u.host().map(|h| h.to_owned())) else {
        return true;
    };
    match resolve(host).await {
        Some(addrs) if !addrs.is_empty() => addrs.iter().all(|ip| crate::validate::is_public_ip(*ip)),
        _ => false,
    }
}

async fn cached_resolve(cache: Arc<std::sync::Mutex<HashMap<String, Option<Vec<IpAddr>>>>>, host: String) -> Option<Vec<IpAddr>> {
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&host) {
        return hit.clone();
    }
    let addrs = crate::net::resolve(&host).await;
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(host, addrs.clone());
    addrs
}

pub struct Tab {
    cdp: Cdp,
    pub target_id: String,
    pub session: String,
    guard: Option<JoinHandle<()>>,
    /// Scenario tabs close themselves even when the job is cancelled; login tabs stay for the user.
    close_on_drop: bool,
}

impl Drop for Tab {
    fn drop(&mut self) {
        if let Some(g) = &self.guard {
            g.abort();
        }
        if self.close_on_drop {
            let (cdp, target) = (self.cdp.clone(), self.target_id.clone());
            tokio::spawn(async move {
                let _ = cdp.call("Target.closeTarget", json!({ "targetId": target }), None).await;
            });
        }
    }
}

impl Tab {
    /// Open a tab (in `context`, or the persistent profile when `None`).
    /// With `allow`, every request passes the network guard; `None` is only for
    /// user-driven login tabs (SSO flows span arbitrary domains).
    pub async fn open(cdp: &Cdp, context: Option<&str>, allow: Option<Allow>) -> Result<Tab> {
        let mut params = json!({ "url": "about:blank" });
        if let Some(ctx) = context {
            params["browserContextId"] = json!(ctx);
        }
        let target_id = cdp.call("Target.createTarget", params, None).await?["targetId"].as_str().context("no targetId")?.to_string();
        let session = cdp.call("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None).await?["sessionId"]
            .as_str()
            .context("no sessionId")?
            .to_string();
        let s = Some(session.as_str());
        let guard = match allow {
            Some(allow) => Some(Self::install_guard(cdp, &session, allow).await?),
            None => None,
        };
        for domain in ["Page.enable", "Network.enable", "Runtime.enable"] {
            cdp.call(domain, json!({}), s).await?;
        }
        cdp.call("Page.setLifecycleEventsEnabled", json!({ "enabled": true }), s).await?;
        Ok(Tab { cdp: cdp.clone(), target_id, session, guard, close_on_drop: true })
    }

    async fn install_guard(cdp: &Cdp, session: &str, allow: Allow) -> Result<JoinHandle<()>> {
        let mut events = cdp.subscribe();
        cdp.call("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] }), Some(session)).await?;
        // Per-tab DNS cache: host -> resolved addresses (None = did not resolve).
        let dns: Arc<std::sync::Mutex<HashMap<String, Option<Vec<IpAddr>>>>> = Arc::default();
        Ok(tokio::spawn({
            let (cdp, session) = (cdp.clone(), session.to_string());
            async move {
                loop {
                    let ev = match events.recv().await {
                        Ok(ev) => ev,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    };
                    if ev.method != "Fetch.requestPaused" || ev.session.as_deref() != Some(session.as_str()) {
                        continue;
                    }
                    let id = ev.params["requestId"].clone();
                    let url = ev.params["request"]["url"].as_str().unwrap_or("").to_string();
                    let kind = ev.params["resourceType"].as_str().unwrap_or("").to_string();
                    let policy = allow.clone();
                    let (cdp, session, dns) = (cdp.clone(), session.clone(), dns.clone());
                    // Decide off the event loop: a DNS lookup must not stall other requests.
                    tokio::spawn(async move {
                        let ok = request_permitted(&url, &kind, &policy, |host| cached_resolve(dns.clone(), host)).await;
                        let (method, params) = if ok {
                            ("Fetch.continueRequest", json!({ "requestId": id }))
                        } else {
                            ("Fetch.failRequest", json!({ "requestId": id, "errorReason": "BlockedByClient" }))
                        };
                        let _ = cdp.call(method, params, Some(&session)).await;
                    });
                }
            }
        }))
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.cdp.subscribe()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call(method, params, Some(&self.session)).await
    }

    /// Navigate and wait for the load event.
    pub async fn goto(&self, url: &str, timeout: Duration) -> Result<()> {
        let mut events = self.events();
        let nav = self.call("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = nav["errorText"].as_str() {
            bail!("navigation failed: {err}");
        }
        // A reused tab may still deliver the previous page's `load`: wait for ours (same loaderId).
        let Some(loader) = nav["loaderId"].as_str() else { return Ok(()) }; // same-document navigation
        self.wait_for(&mut events, timeout, |e| {
            e.method == "Page.lifecycleEvent" && e.params["name"] == "load" && e.params["loaderId"] == loader
        })
        .await
        .map(|_| ())
    }

    /// Wait for an event of this tab matching `pred`.
    pub async fn wait_for(
        &self,
        events: &mut broadcast::Receiver<Event>,
        timeout: Duration,
        pred: impl Fn(&Event) -> bool,
    ) -> Result<Event> {
        let fut = async {
            loop {
                match events.recv().await {
                    Ok(ev) if ev.session.as_deref() == Some(self.session.as_str()) && pred(&ev) => return Ok(ev),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => bail!("browser closed"),
                }
            }
        };
        tokio::time::timeout(timeout, fut).await.context("timed out waiting for the page")?
    }

    /// Evaluate one of our embedded scripts and return its JSON value.
    pub async fn eval(&self, script: &str) -> Result<Value> {
        let r = self.call("Runtime.evaluate", json!({ "expression": script, "returnByValue": true, "awaitPromise": true })).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("page script failed: {}", ex["exception"]["description"].as_str().unwrap_or("exception"));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Body of a finished network response.
    pub async fn response_body(&self, request_id: &str) -> Result<String> {
        let r = self.call("Network.getResponseBody", json!({ "requestId": request_id })).await?;
        Ok(r["body"].as_str().unwrap_or("").to_string())
    }

    /// Bring the tab to the front (used when the user has to act).
    pub async fn focus(&self) -> Result<()> {
        self.cdp.call("Target.activateTarget", json!({ "targetId": self.target_id }), None).await.map(|_| ())
    }

    pub async fn close(mut self) {
        self.close_on_drop = false;
        let _ = self.cdp.call("Target.closeTarget", json!({ "targetId": self.target_id }), None).await;
    }

    /// Leave this tab open for the user when it goes out of scope (login pages).
    pub fn keep_open(mut self) -> Self {
        self.close_on_drop = false;
        self
    }

    /// Cookie value for `url` (including httpOnly cookies), e.g. to detect a logged-in session.
    pub async fn cookie(&self, url: &str, name: &str) -> Result<Option<String>> {
        let r = self.call("Network.getCookies", json!({ "urls": [url] })).await?;
        Ok(r["cookies"].as_array().into_iter().flatten().find(|c| c["name"] == name).and_then(|c| c["value"].as_str()).map(str::to_string))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDDIT: Allow = Allow::Domains(&["reddit.com", "redditstatic.com"]);

    /// A scripted in-memory Chrome: answers commands, and after `Page.navigate`
    /// emits a stale `load` from the previous navigation before the real one.
    fn fake_chrome() -> (Cdp, Arc<std::sync::atomic::AtomicBool>) {
        use tokio::sync::mpsc;
        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        let (in_tx, in_rx) = mpsc::channel::<String>(64);
        let real_load_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = real_load_sent.clone();
        tokio::spawn(async move {
            while let Some(raw) = out_rx.recv().await {
                let msg: Value = serde_json::from_str(&raw).unwrap();
                let result = match msg["method"].as_str().unwrap() {
                    "Target.createTarget" => json!({ "targetId": "T1" }),
                    "Target.attachToTarget" => json!({ "sessionId": "S1" }),
                    "Page.navigate" => json!({ "frameId": "F1", "loaderId": "L2" }),
                    _ => json!({}),
                };
                let navigate = msg["method"] == "Page.navigate";
                in_tx.send(json!({ "id": msg["id"], "result": result }).to_string()).await.unwrap();
                if navigate {
                    let ev = |loader: &str| {
                        json!({ "method": "Page.lifecycleEvent", "sessionId": "S1",
                                "params": { "frameId": "F1", "loaderId": loader, "name": "load" } })
                        .to_string()
                    };
                    in_tx.send(ev("L1")).await.unwrap(); // stale: previous page in this reused tab
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    in_tx.send(ev("L2")).await.unwrap();
                }
            }
        });
        (Cdp::new(out_tx, in_rx), real_load_sent)
    }

    #[tokio::test]
    async fn goto_waits_for_the_load_of_its_own_navigation() {
        let (cdp, real_load_sent) = fake_chrome();
        let tab = Tab::open(&cdp, None, None).await.unwrap().keep_open();
        tab.goto("https://example.com/", Duration::from_secs(5)).await.unwrap();
        assert!(real_load_sent.load(std::sync::atomic::Ordering::SeqCst), "returned on a stale load event");
    }

    #[test]
    fn domain_allowlist() {
        assert!(request_allowed("https://www.reddit.com/r/rust/", "Document", &REDDIT));
        assert!(request_allowed("https://www.redditstatic.com/x.js", "Script", &REDDIT));
        assert!(!request_allowed("https://evil.com/collect?x=1", "XHR", &REDDIT));
        assert!(!request_allowed("https://reddit.com.evil.com/", "Document", &REDDIT));
        assert!(!request_allowed("http://www.reddit.com/", "Document", &REDDIT), "plain http is not allowed for platforms");
    }

    #[test]
    fn media_is_always_blocked() {
        for kind in ["Image", "Media", "Font"] {
            assert!(!request_allowed("https://www.reddit.com/a.png", kind, &REDDIT));
            assert!(!request_allowed("https://cdn.example.com/a.png", kind, &Allow::AnyPublic));
        }
    }

    #[test]
    fn any_public_still_blocks_private_and_odd_schemes() {
        assert!(request_allowed("https://example.com/", "Document", &Allow::AnyPublic));
        assert!(request_allowed("http://example.com/", "Document", &Allow::AnyPublic));
        for bad in [
            "http://127.0.0.1:8080/",
            "http://192.168.0.1/admin",
            "http://localhost/",
            "file:///etc/passwd",
            "chrome://settings",
            "ws://example.com/",
        ] {
            assert!(!request_allowed(bad, "Document", &Allow::AnyPublic), "{bad}");
        }
    }

    fn ips(list: &[&str]) -> Option<Vec<std::net::IpAddr>> {
        Some(list.iter().map(|s| s.parse().unwrap()).collect())
    }

    #[tokio::test]
    async fn dns_must_resolve_to_public_addresses_only() {
        let private = |_: String| async { ips(&["93.184.216.34", "127.0.0.1"]) };
        let public = |_: String| async { ips(&["93.184.216.34"]) };
        let unknown = |_: String| async { None };
        let url = "https://rebind.attacker.example/latest/meta-data/";
        assert!(!request_permitted(url, "Document", &Allow::AnyPublic, private).await, "DNS rebinding to loopback");
        assert!(!request_permitted(url, "Document", &Allow::AnyPublic, unknown).await);
        assert!(request_permitted(url, "Document", &Allow::AnyPublic, public).await);
        assert!(!request_permitted("https://www.reddit.com/", "Document", &REDDIT, private).await);
        assert!(request_permitted("https://www.reddit.com/", "Document", &REDDIT, public).await);
    }

    #[tokio::test]
    async fn static_rules_apply_before_any_dns_lookup() {
        let never = |_: String| async { panic!("must not resolve") };
        assert!(!request_permitted("https://evil.com/", "Document", &REDDIT, never).await);
        assert!(!request_permitted("https://www.reddit.com/a.png", "Image", &REDDIT, never).await);
        assert!(request_permitted("data:text/css,a{}", "Stylesheet", &REDDIT, never).await);
    }

    #[test]
    fn inline_data_is_allowed() {
        assert!(request_allowed("data:text/css,body{}", "Stylesheet", &REDDIT));
        assert!(request_allowed("blob:https://www.reddit.com/uuid", "XHR", &REDDIT));
    }
}
