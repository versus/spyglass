//! One browser tab driven by a fixed scenario: network allowlist, navigation,
//! JSON response capture, evaluation of our own (never agent-supplied) scripts.

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

pub struct Tab {
    cdp: Cdp,
    pub target_id: String,
    pub session: String,
    guard: Option<JoinHandle<()>>,
    /// Current network policy of a guarded tab; scenarios switch it when reusing the tab.
    allow: Option<Arc<std::sync::Mutex<Allow>>>,
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
        let allow = allow.map(|a| Arc::new(std::sync::Mutex::new(a)));
        let guard = match &allow {
            Some(allow) => Some(Self::install_guard(cdp, &session, allow.clone()).await?),
            None => None,
        };
        for domain in ["Page.enable", "Network.enable", "Runtime.enable"] {
            cdp.call(domain, json!({}), s).await?;
        }
        Ok(Tab { cdp: cdp.clone(), target_id, session, guard, allow, close_on_drop: true })
    }

    async fn install_guard(cdp: &Cdp, session: &str, allow: Arc<std::sync::Mutex<Allow>>) -> Result<JoinHandle<()>> {
        let mut events = cdp.subscribe();
        cdp.call("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] }), Some(session)).await?;
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
                    let url = ev.params["request"]["url"].as_str().unwrap_or("");
                    let kind = ev.params["resourceType"].as_str().unwrap_or("");
                    let ok = allow.lock().map(|a| request_allowed(url, kind, &a)).unwrap_or(false);
                    let (method, params) = if ok {
                        ("Fetch.continueRequest", json!({ "requestId": id }))
                    } else {
                        ("Fetch.failRequest", json!({ "requestId": id, "errorReason": "BlockedByClient" }))
                    };
                    let (cdp, session) = (cdp.clone(), session.clone());
                    tokio::spawn(async move {
                        let _ = cdp.call(method, params, Some(&session)).await;
                    });
                }
            }
        }))
    }

    /// Switch the network policy (when a scenario reuses this tab).
    pub fn set_allow(&self, new: Allow) {
        if let Some(Ok(mut current)) = self.allow.as_ref().map(|a| a.lock()) {
            *current = new;
        }
    }

    /// Does the tab still exist (the user may have closed it)?
    pub async fn alive(&self) -> bool {
        self.eval("1").await.is_ok()
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
        self.wait_for(&mut events, timeout, |e| e.method == "Page.loadEventFired").await.map(|_| ())
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

    #[test]
    fn inline_data_is_allowed() {
        assert!(request_allowed("data:text/css,body{}", "Stylesheet", &REDDIT));
        assert!(request_allowed("blob:https://www.reddit.com/uuid", "XHR", &REDDIT));
    }
}
