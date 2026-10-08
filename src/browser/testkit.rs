//! Test helpers: an in-memory Chrome for CDP-level tests.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::cdp::Cdp;

pub type Sent = Arc<std::sync::Mutex<Vec<Value>>>;

/// A fake Chrome that records every command and lets the test inject events.
pub fn recording_chrome() -> (Cdp, mpsc::Sender<String>, Sent) {
    scripted_chrome(|_| None)
}

/// Same, but commands matching `fails` get a protocol error (like an unsupported domain).
pub fn recording_chrome_failing(fails: impl Fn(&Value) -> bool + Send + 'static) -> (Cdp, mpsc::Sender<String>, Sent) {
    scripted_chrome(move |m| fails(m).then(|| Err("not found".to_string())))
}

/// The general fake: `respond` may answer a command (Ok = result, Err = protocol error);
/// `None` falls back to defaults (target/session ids, otherwise an empty result).
pub fn scripted_chrome(
    mut respond: impl FnMut(&Value) -> Option<Result<Value, String>> + Send + 'static,
) -> (Cdp, mpsc::Sender<String>, Sent) {
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let (in_tx, in_rx) = mpsc::channel::<String>(64);
    let sent: Sent = Arc::default();
    let (log, reply) = (sent.clone(), in_tx.clone());
    tokio::spawn(async move {
        while let Some(raw) = out_rx.recv().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            log.lock().unwrap().push(msg.clone());
            let default = match msg["method"].as_str().unwrap() {
                "Target.createTarget" => json!({ "targetId": "T1" }),
                "Target.attachToTarget" => json!({ "sessionId": "S1" }),
                "Target.createBrowserContext" => json!({ "browserContextId": "C1" }),
                _ => json!({}),
            };
            let answer = match respond(&msg).unwrap_or(Ok(default)) {
                Ok(result) => json!({ "id": msg["id"], "result": result }),
                Err(message) => json!({ "id": msg["id"], "error": { "code": -32601, "message": message } }),
            };
            reply.send(answer.to_string()).await.unwrap();
        }
    });
    (Cdp::new(out_tx, in_rx), in_tx, sent)
}

/// Result shape of `Runtime.evaluate` with `returnByValue`.
pub fn eval_result(value: Value) -> Result<Value, String> {
    Ok(json!({ "result": { "value": value } }))
}

/// Index of the first `method` sent on `session` ("" = browser-level, no session).
pub fn position(sent: &Sent, method: &str, session: &str) -> Option<usize> {
    sent.lock()
        .unwrap()
        .iter()
        .position(|m| m["method"] == method && (m["sessionId"] == session || (session.is_empty() && m.get("sessionId").is_none())))
}
