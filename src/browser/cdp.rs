//! Minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//! Messages are JSON terminated by NUL. No TCP port is ever opened.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A protocol event: method, params and the flattened session it belongs to.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone)]
pub struct Cdp {
    out: mpsc::Sender<String>,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    events: broadcast::Sender<Event>,
}

impl Cdp {
    /// Build a client from raw message channels (one message per item, no NUL).
    pub fn new(out: mpsc::Sender<String>, mut incoming: mpsc::Receiver<String>) -> Self {
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(8192);
        let router_pending = pending.clone();
        let router_events = events.clone();
        tokio::spawn(async move {
            while let Some(raw) = incoming.recv().await {
                let Ok(msg) = serde_json::from_str::<Value>(&raw) else { continue };
                if let Some(id) = msg["id"].as_u64() {
                    if let Some(tx) = router_pending.lock().await.remove(&id) {
                        let result = match msg.get("error") {
                            Some(err) => Err(anyhow!("{}", err["message"].as_str().unwrap_or("protocol error"))),
                            None => Ok(msg["result"].clone()),
                        };
                        let _ = tx.send(result);
                    }
                } else if let Some(method) = msg["method"].as_str() {
                    let _ = router_events.send(Event {
                        method: method.to_string(),
                        params: msg["params"].clone(),
                        session: msg["sessionId"].as_str().map(str::to_string),
                    });
                }
            }
            for (_, tx) in router_pending.lock().await.drain() {
                let _ = tx.send(Err(anyhow!("browser connection closed")));
            }
        });
        Self { out, next_id: Arc::new(AtomicU64::new(1)), pending, events }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Send a command and wait for its result.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.out.send(msg.to_string()).await.is_err() {
            self.pending.lock().await.remove(&id);
            bail!("{method}: browser connection closed");
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result.map_err(|e| anyhow!("{method}: {e}")),
            Ok(Err(_)) => bail!("{method}: browser connection closed"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("{method}: timed out")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Cdp, mpsc::Receiver<String>, mpsc::Sender<String>) {
        let (out_tx, out_rx) = mpsc::channel(16);
        let (in_tx, in_rx) = mpsc::channel(16);
        (Cdp::new(out_tx, in_rx), out_rx, in_tx)
    }

    #[tokio::test]
    async fn call_sends_command_and_resolves_matching_response() {
        let (cdp, mut sent, incoming) = pair();
        let call = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call("Page.navigate", json!({"url": "https://e.com"}), Some("S1")).await }
        });
        let msg: Value = serde_json::from_str(&sent.recv().await.unwrap()).unwrap();
        assert_eq!(msg["method"], "Page.navigate");
        assert_eq!(msg["params"]["url"], "https://e.com");
        assert_eq!(msg["sessionId"], "S1");
        let id = msg["id"].as_u64().unwrap();
        incoming.send(json!({"id": id + 100, "result": {"wrong": true}}).to_string()).await.unwrap();
        incoming.send(json!({"id": id, "result": {"frameId": "F"}}).to_string()).await.unwrap();
        assert_eq!(call.await.unwrap().unwrap()["frameId"], "F");
    }

    #[tokio::test]
    async fn protocol_errors_become_errors() {
        let (cdp, mut sent, incoming) = pair();
        let call = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call("Bogus.method", json!({}), None).await }
        });
        let msg: Value = serde_json::from_str(&sent.recv().await.unwrap()).unwrap();
        assert!(msg.get("sessionId").is_none());
        incoming.send(json!({"id": msg["id"], "error": {"code": -32601, "message": "not found"}}).to_string()).await.unwrap();
        let err = call.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("Bogus.method") && err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn events_are_broadcast_with_session() {
        let (cdp, _sent, incoming) = pair();
        let mut events = cdp.subscribe();
        incoming
            .send(json!({"method": "Network.responseReceived", "params": {"requestId": "1"}, "sessionId": "S9"}).to_string())
            .await
            .unwrap();
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.method, "Network.responseReceived");
        assert_eq!(ev.params["requestId"], "1");
        assert_eq!(ev.session.as_deref(), Some("S9"));
    }

    #[tokio::test]
    async fn pending_calls_fail_when_browser_goes_away() {
        let (cdp, mut sent, incoming) = pair();
        let call = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call("Browser.getVersion", json!({}), None).await }
        });
        sent.recv().await.unwrap();
        drop(incoming);
        assert!(call.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn garbage_messages_are_ignored() {
        let (cdp, _sent, incoming) = pair();
        let mut events = cdp.subscribe();
        incoming.send("not json".into()).await.unwrap();
        incoming.send(json!({"method": "Page.loadEventFired", "params": {}}).to_string()).await.unwrap();
        assert_eq!(events.recv().await.unwrap().method, "Page.loadEventFired");
    }
}
