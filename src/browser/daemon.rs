//! The agent-browser daemon: owns the agent's Chrome instances (pipe-controlled; a headless one
//! tried first and a visible one with the persistent profile, both started on demand),
//! listens on a private unix socket, runs fixed read-only scenarios one at a time.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use super::cdp::Cdp;
use super::proto::{Reply, Request};
use super::scenario::{Blocked, Ctx, Found, NAV_TIMEOUT, Site, require_visible};
use super::tab::{Guard, Tab};
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
    /// `browser start --headless`: never open a window (servers); hand-offs fail fast.
    headless_only: bool,
    /// With windows: the persistent profile (logins). Started on first need.
    visible: Mutex<Option<Browser>>,
    /// No window, throwaway profile: tried first for arbitrary pages. Started on first need.
    quiet: Mutex<Option<Browser>>,
    quiet_profile: PathBuf,
    /// Hosts that blocked the headless browser this session: go visible directly.
    learned: std::sync::Mutex<HashSet<String>>,
    /// Held for the whole job: one scenario at a time, gentle on platforms.
    turn: Mutex<()>,
    stop: mpsc::Sender<()>,
}

/// A running Chrome. `alive` turns false when the process exits; it is then relaunched on demand.
struct Browser {
    cdp: Cdp,
    version: String,
    alive: Arc<AtomicBool>,
}

impl State {
    /// The visible or the headless browser, launching (or relaunching) it when needed.
    async fn browser(&self, visible: bool) -> Result<Cdp> {
        let visible = visible && !self.headless_only;
        let slot = if visible { &self.visible } else { &self.quiet };
        let mut current = slot.lock().await;
        if let Some(b) = current.as_ref().filter(|b| b.alive.load(Ordering::SeqCst)) {
            return Ok(b.cdp.clone());
        }
        let profile = if visible { profile_dir() } else { self.quiet_profile.clone() };
        let (cdp, child) = chrome::launch(&profile, !visible)?;
        let guard = chrome::KillOnDrop::new(child); // a startup error below kills this Chrome
        let version = cdp.call("Browser.getVersion", json!({}), None).await?["product"].as_str().unwrap_or("").to_string();
        let mut child = guard.disarm();
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        std::thread::spawn(move || {
            let _ = child.wait(); // closed by the user, crashed, or Browser.close
            flag.store(false, Ordering::SeqCst);
        });
        log(&format!("started {} browser {version}", if visible { "visible" } else { "headless" }));
        *current = Some(Browser { cdp: cdp.clone(), version, alive });
        Ok(cdp)
    }

    /// "visible: Chrome/155 (1 tab)" or "visible: off" for status output.
    async fn describe(&self, name: &str, slot: &Mutex<Option<Browser>>) -> (String, usize) {
        let current = slot.lock().await;
        let Some(b) = current.as_ref().filter(|b| b.alive.load(Ordering::SeqCst)) else { return (format!("{name}: off"), 0) };
        let targets = b.cdp.call("Target.getTargets", json!({}), None).await.unwrap_or_default();
        let tabs = targets["targetInfos"].as_array().map(|t| t.iter().filter(|t| t["type"] == "page").count()).unwrap_or(0);
        (format!("{name}: {} ({tabs} tabs)", b.version), tabs)
    }

    async fn close_all(&self) {
        for slot in [&self.visible, &self.quiet] {
            if let Some(b) = slot.lock().await.take() {
                let _ = b.cdp.call("Browser.close", json!({}), None).await;
            }
        }
    }
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

pub async fn serve(headless_only: bool) -> Result<()> {
    crate::paths::ensure_private_dir(&base_dir())?;
    let sock = socket_path();
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            bail!("the agent browser is already running");
        }
        std::fs::remove_file(&sock)?;
    }
    // Claim the socket first: a concurrent second daemon fails here. Browsers start lazily.
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    // Headless-only keeps the persistent profile (logins); otherwise headless gets a throwaway one.
    let quiet_profile = if headless_only { profile_dir() } else { base_dir().join(format!("headless-{}", std::process::id())) };

    let (stop, mut stopped) = mpsc::channel::<()>(4);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let state = Arc::new(State {
        headless_only,
        visible: Mutex::default(),
        quiet: Mutex::default(),
        quiet_profile: quiet_profile.clone(),
        learned: std::sync::Mutex::default(),
        turn: Mutex::new(()),
        stop,
    });
    log(&format!("daemon started ({})", if headless_only { "headless only" } else { "headless first, visible when needed" }));
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
    state.close_all().await;
    if !headless_only {
        let _ = std::fs::remove_dir_all(&quiet_profile);
    }
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
            let (visible, v_tabs) = state.describe("visible", &state.visible).await;
            let (quiet, q_tabs) = state.describe("headless", &state.quiet).await;
            let mode = if state.headless_only { "headless only" } else { "headless first" };
            Ok(Found {
                url: None,
                markdown: format!("agent browser running ({mode}; {visible}; {quiet})"),
                data: json!({ "headless": state.headless_only, "build": build_id(), "tabs": v_tabs + q_tabs }),
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
            require_visible(state.headless_only, &format!("A login to {platform}"))?;
            let cdp = state.browser(true).await?;
            let tab = Tab::open(&cdp, None, None).await?.keep_open();
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
            // Arbitrary pages: headless first, unless the host is known to block headless browsers.
            let host =
                args["url"].as_str().and_then(|u| crate::validate::public_url(u).ok()).and_then(|u| u.host_str().map(str::to_string));
            let learned_visible =
                host.as_deref().is_some_and(|h| render::needs_visible(h, &state.learned.lock().unwrap_or_else(|e| e.into_inner())));
            let visible = site.visible || learned_visible;
            match run_job(state, &site, &platform, &verb, &args, notify, visible).await {
                Err(e) if !visible && !state.headless_only && e.is::<Blocked>() => {
                    log(&format!("{e}; retrying in the visible browser"));
                    if let Some(h) = host {
                        state.learned.lock().unwrap_or_else(|e| e.into_inner()).insert(h);
                    }
                    run_job(state, &site, &platform, &verb, &args, notify, true).await
                }
                other => other,
            }
            .map(Reply::from)
        }
    }
}

/// One scenario in its own tab (and, for arbitrary pages, its own cookie-less context);
/// both are closed afterwards, so nothing is left open.
async fn run_job(
    state: &State,
    site: &Site,
    platform: &str,
    verb: &str,
    args: &Value,
    notify: &mpsc::Sender<Reply>,
    visible: bool,
) -> Result<Found> {
    let cdp = state.browser(visible).await?;
    let scratch = match site.scratch {
        true => Some(Scratch::create(&cdp).await?),
        false => None,
    };
    let headless = !visible || state.headless_only;
    let guard = Guard { allow: site.allow.clone(), block_media: headless };
    let tab = Tab::open(&cdp, scratch.as_ref().map(|s| s.id.as_str()), Some(guard)).await?;
    let ctx = Ctx { cdp: &cdp, tab: &tab, notify, headless };
    log(&format!("job {platform} {verb} ({})", if headless { "headless" } else { "visible" }));
    let found = match platform {
        "reddit" => reddit::job(&ctx, verb, args).await,
        "x" => x::job(&ctx, verb, args).await,
        "search" => ddg::job(&ctx, verb, args).await,
        _ => render::job(&ctx, verb, args).await,
    };
    if let Err(e) = &found {
        log(&format!("job {platform} {verb} failed: {e:#}"));
    }
    tab.close().await;
    if let Some(scratch) = scratch {
        scratch.dispose().await;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::testkit::{position, recording_chrome};

    #[tokio::test]
    async fn a_cancelled_job_still_disposes_its_cookie_less_context() {
        let (cdp, _inject, sent) = recording_chrome();
        let scratch = Scratch::create(&cdp).await.unwrap();
        drop(scratch); // the job was aborted: no explicit dispose()
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let disposed =
            sent.lock().unwrap().iter().any(|m| m["method"] == "Target.disposeBrowserContext" && m["params"]["browserContextId"] == "C1");
        assert!(disposed, "context left behind for the next job");
        assert!(position(&sent, "Target.createBrowserContext", "").is_some());
    }
}
