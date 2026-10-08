//! The agent-browser daemon: owns one Chrome (dedicated profile, pipe-controlled),
//! listens on a private unix socket, runs fixed read-only scenarios one at a time.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use super::cdp::Cdp;
use super::proto::{Reply, Request};
use super::scenario::{Ctx, Found, NAV_TIMEOUT};
use super::tab::Tab;
use super::{chrome, ddg, reddit, render, x};

const MAX_REQUEST: u64 = 64 * 1024;

fn base_dir() -> PathBuf {
    dirs::runtime_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("spyglass")
}

pub fn socket_path() -> PathBuf {
    base_dir().join("browser.sock")
}

pub fn log_path() -> PathBuf {
    base_dir().join("daemon.log")
}

/// Identifies the exact binary: a daemon from an older build must not keep serving stale scenarios.
pub fn build_id() -> String {
    let exe = std::env::current_exe().ok();
    let mtime = exe.as_ref().and_then(|e| e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok());
    format!(
        "{}:{}:{}",
        env!("CARGO_PKG_VERSION"),
        exe.map(|e| e.display().to_string()).unwrap_or_default(),
        mtime.map(|d| d.as_secs()).unwrap_or(0)
    )
}

/// One line to the daemon log (its stderr is redirected there by the client).
fn log(line: &str) {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    eprintln!("{ts} {}", crate::output::oneline(line, 500));
}

pub fn profile_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("spyglass").join("browser-profile")
}

struct State {
    cdp: Cdp,
    headless: bool,
    version: String,
    /// Held for the whole job: one scenario at a time, gentle on platforms.
    turn: Mutex<()>,
    stop: mpsc::Sender<()>,
}

/// A cookie-less browser context; disposed after the job (or when dropped on cancel).
struct Scratch {
    cdp: Cdp,
    id: String,
    disposed: bool,
}

impl Scratch {
    async fn create(cdp: &Cdp) -> Result<Self> {
        let r = cdp.call("Target.createBrowserContext", json!({ "disposeOnDetach": false }), None).await?;
        let id = r["browserContextId"].as_str().context("no browserContextId")?.to_string();
        Ok(Self { cdp: cdp.clone(), id, disposed: false })
    }

    async fn dispose(mut self) {
        self.disposed = true;
        let _ = self.cdp.call("Target.disposeBrowserContext", json!({ "browserContextId": self.id }), None).await;
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.disposed {
            let (cdp, id) = (self.cdp.clone(), self.id.clone());
            tokio::spawn(async move {
                let _ = cdp.call("Target.disposeBrowserContext", json!({ "browserContextId": id }), None).await;
            });
        }
    }
}

pub async fn serve(headless: bool) -> Result<()> {
    crate::paths::ensure_private_dir(&base_dir())?;
    let sock = socket_path();
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            bail!("the agent browser is already running");
        }
        std::fs::remove_file(&sock)?;
    }
    // Claim the socket first: a concurrent second daemon fails here, before starting a Chrome.
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    let (cdp, child) = chrome::launch(&profile_dir(), headless)?;
    let guard = chrome::KillOnDrop::new(child); // any startup error below kills Chrome
    let version = cdp.call("Browser.getVersion", json!({}), None).await?["product"].as_str().unwrap_or("").to_string();
    let mut child = guard.disarm();

    let (stop, mut stopped) = mpsc::channel::<()>(4);
    let chrome_exit = stop.clone();
    std::thread::spawn(move || {
        let _ = child.wait(); // the user closed the window, or Browser.close
        let _ = chrome_exit.blocking_send(());
    });
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let state = Arc::new(State { cdp: cdp.clone(), headless, version, turn: Mutex::new(()), stop });
    loop {
        tokio::select! {
            conn = listener.accept() => {
                let Ok((stream, _)) = conn else { continue };
                tokio::spawn(handle(stream, state.clone()));
            }
            _ = stopped.recv() => break,
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    // Stop accepting before closing Chrome, so a successor daemon's socket is never removed.
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    let _ = cdp.call("Browser.close", json!({}), None).await;
    Ok(())
}

async fn handle(stream: UnixStream, state: Arc<State>) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let (tx, mut rx) = mpsc::channel::<Reply>(8);
    let worker = tokio::spawn(async move {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &state, &tx).await.unwrap_or_else(|e| Reply::Error { message: format!("{e:#}") }),
            Err(e) => Reply::Error { message: format!("bad request: {e}") },
        };
        let _ = tx.send(reply).await;
    });
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            reply = rx.recv() => {
                let Some(reply) = reply else { break };
                let mut out = serde_json::to_string(&reply).unwrap_or_default();
                out.push('\n');
                if write.write_all(out.as_bytes()).await.is_err() {
                    break;
                }
            }
            // The client went away (e.g. the agent's command timed out): cancel the job.
            _ = reader.read(&mut probe) => {
                worker.abort();
                return;
            }
        }
    }
    let _ = worker.await;
}

async fn dispatch(req: Request, state: &State, notify: &mpsc::Sender<Reply>) -> Result<Reply> {
    match req {
        Request::Ping => {
            let targets = state.cdp.call("Target.getTargets", json!({}), None).await?;
            let tabs = targets["targetInfos"].as_array().map(|t| t.iter().filter(|t| t["type"] == "page").count()).unwrap_or(0);
            let mode = if state.headless { "headless" } else { "visible" };
            Ok(Found {
                url: None,
                markdown: format!("agent browser running ({}, {mode}, {tabs} tabs)", state.version),
                data: json!({ "headless": state.headless, "version": state.version, "build": build_id(), "tabs": tabs }),
            }
            .into())
        }
        Request::Stop => {
            let _ = state.stop.send(()).await;
            Ok(Found { url: None, markdown: "agent browser stopping".into(), data: Value::Null }.into())
        }
        Request::Login { platform } => {
            let url = match platform.as_str() {
                "x" => x::LOGIN_URL,
                "reddit" => reddit::LOGIN_URL,
                _ => bail!("unknown platform {platform:?}; supported: x, reddit"),
            };
            let tab = Tab::open(&state.cdp, None, None).await?.keep_open();
            let _ = tab.goto(url, NAV_TIMEOUT).await;
            tab.focus().await?;
            let markdown = format!("Log in to {platform} in the agent browser window. The session stays in the agent profile only.");
            Ok(Found { url: Some(url.into()), markdown, data: Value::Null }.into())
        }
        Request::Job { platform, verb, args } => {
            let _turn = match state.turn.try_lock() {
                Ok(turn) => turn,
                Err(_) => {
                    let _ = notify.send(Reply::Waiting { message: "another browser job is running; queued".into() }).await;
                    state.turn.lock().await
                }
            };
            let site = match platform.as_str() {
                "reddit" => reddit::SITE,
                "x" => x::SITE,
                "search" => ddg::SITE,
                "web" => render::SITE,
                _ => bail!("unknown platform {platform:?}"),
            };
            // Every job gets its own tab and closes it: nothing is left open afterwards.
            // Arbitrary pages run in a fresh cookie-less context that is disposed afterwards.
            let scratch = match site.scratch {
                true => Some(Scratch::create(&state.cdp).await?),
                false => None,
            };
            let tab = Tab::open(&state.cdp, scratch.as_ref().map(|s| s.id.as_str()), Some(site.allow)).await?;
            let ctx = Ctx { cdp: &state.cdp, tab: &tab, notify, headless: state.headless };
            log(&format!("job {platform} {verb}"));
            let found = match platform.as_str() {
                "reddit" => reddit::job(&ctx, &verb, &args).await,
                "x" => x::job(&ctx, &verb, &args).await,
                "search" => ddg::job(&ctx, &verb, &args).await,
                _ => render::job(&ctx, &verb, &args).await,
            };
            if let Err(e) = &found {
                log(&format!("job {platform} {verb} failed: {e:#}"));
            }
            tab.close().await;
            if let Some(scratch) = scratch {
                scratch.dispose().await;
            }
            found.map(Reply::from)
        }
    }
}
