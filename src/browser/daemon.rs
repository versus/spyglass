//! The agent-browser daemon: owns one Chrome (dedicated profile, pipe-controlled),
//! listens on a private unix socket, runs fixed read-only scenarios one at a time.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use super::cdp::Cdp;
use super::proto::{Reply, Request};
use super::tab::{Allow, Tab};
use super::{chrome, reddit, x};

const NAV_TIMEOUT: Duration = Duration::from_secs(45);
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

pub fn profile_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("spyglass").join("browser-profile")
}

struct State {
    cdp: Cdp,
    headless: bool,
    version: String,
    /// The agent's tabs. Locked for the whole job: one scenario at a time, gentle on platforms.
    work: Mutex<WorkTabs>,
    stop: mpsc::Sender<()>,
}

/// Scenarios reuse one visible tab instead of opening a new one per job.
#[derive(Default)]
struct WorkTabs {
    /// Logged-in profile: platform scenarios (fixed domain allowlists).
    main: Option<Tab>,
    /// Cookie-less context (its own window) for arbitrary pages.
    scratch: Option<Tab>,
    scratch_ctx: Option<String>,
}

impl WorkTabs {
    async fn tab(&mut self, cdp: &Cdp, scratch: bool, allow: Allow) -> Result<&Tab> {
        if scratch && self.scratch_ctx.is_none() {
            let r = cdp.call("Target.createBrowserContext", json!({ "disposeOnDetach": false }), None).await?;
            self.scratch_ctx = r["browserContextId"].as_str().map(str::to_string);
        }
        let ctx = if scratch { self.scratch_ctx.clone() } else { None };
        let slot = if scratch { &mut self.scratch } else { &mut self.main };
        let alive = match slot {
            Some(t) => t.alive().await,
            None => false,
        };
        if !alive {
            *slot = Some(Tab::open(cdp, ctx.as_deref(), Some(allow.clone())).await?.keep_open());
        }
        let tab = slot.as_ref().context("no agent tab")?;
        tab.set_allow(allow);
        Ok(tab)
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
    let state = Arc::new(State { cdp: cdp.clone(), headless, version, work: Mutex::default(), stop });
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
            Ok(done(
                None,
                format!("agent browser running ({}, {mode}, {tabs} tabs)", state.version),
                json!({ "headless": state.headless, "version": state.version, "build": build_id(), "tabs": tabs }),
            ))
        }
        Request::Stop => {
            let _ = state.stop.send(()).await;
            Ok(done(None, "agent browser stopping".into(), Value::Null))
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
            Ok(done(
                Some(url.into()),
                format!("Log in to {platform} in the agent browser window. The session stays in the agent profile only."),
                Value::Null,
            ))
        }
        Request::Job { platform, verb, args } => {
            let mut work = match state.work.try_lock() {
                Ok(work) => work,
                Err(_) => {
                    let _ = notify.send(Reply::Waiting { message: "another browser job is running; queued".into() }).await;
                    state.work.lock().await
                }
            };
            let cdp = &state.cdp;
            match platform.as_str() {
                "reddit" => reddit_job(work.tab(cdp, false, reddit::ALLOW).await?, &verb, &args).await,
                "x" => {
                    let tab = work.tab(cdp, false, x::ALLOW).await?;
                    x::job(cdp, tab, &verb, &args, notify).await.map(|(url, md, data)| done(Some(url), md, data))
                }
                "web" if verb == "render" => web_render(work.tab(cdp, true, Allow::AnyPublic).await?, &args).await,
                "search" if verb == "ddg" => {
                    ddg_search(work.tab(cdp, false, Allow::Domains(&["duckduckgo.com"])).await?, &args, notify).await
                }
                _ => bail!("unknown job {platform} {verb}"),
            }
        }
    }
}

fn done(url: Option<String>, markdown: String, data: Value) -> Reply {
    Reply::Done { url, markdown, data }
}

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().with_context(|| format!("missing argument {key}"))
}

pub fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args[key].as_u64().map(|n| n as usize).unwrap_or(default).clamp(1, 100)
}

/// Load `url` in the agent tab and run one of our scripts.
async fn scrape(tab: &Tab, url: &str, script: &str) -> Result<Value> {
    tab.goto(url, NAV_TIMEOUT).await?;
    tab.eval(script).await
}

/// Page script: wait (≤10 s) until `ready` matches or the block page shows, then extract.
fn when_ready(ready: &str, blocked_marker: &str, extract: &str) -> String {
    format!(
        "new Promise(resolve => {{ const t0 = Date.now(); const tick = () => {{
            if ((document.body?.innerText || '').includes({blocked_marker:?})) return resolve({{ blocked: true }});
            if (document.querySelector({ready:?}) || Date.now() - t0 > 10000) return resolve({{ data: {extract} }});
            setTimeout(tick, 250); }}; tick(); }})"
    )
}

async fn reddit_job(tab: &Tab, verb: &str, args: &Value) -> Result<Reply> {
    let limit = arg_usize(args, "limit", 10);
    let (url, ready, script) = match verb {
        "search" => (reddit::search_url(arg_str(args, "query")?, args["sub"].as_str())?, reddit::SEARCH_READY, reddit::SEARCH_JS),
        "sub" => (reddit::sub_url(arg_str(args, "name")?, args["sort"].as_str().unwrap_or("hot"))?, "shreddit-post", reddit::LISTING_JS),
        "post" => (reddit::post_url(arg_str(args, "post")?)?, reddit::POST_READY, reddit::POST_JS),
        _ => bail!("unknown reddit command {verb}"),
    };
    let script = when_ready(ready, reddit::BLOCKED_MARKER, script);
    let v = scrape(tab, &url, &script).await?;
    if v["blocked"] == true {
        bail!(
            "Reddit blocked the agent browser. Use the visible mode (`spyglass browser start`) or log in: `spyglass browser login reddit`"
        );
    }
    let data = v["data"].clone();
    let md = match verb {
        "search" => reddit::render_search(&data, limit),
        "sub" => reddit::render_listing(&data, limit),
        _ if data.is_null() => bail!("no post found at {url}"),
        _ => reddit::render_post(&data, arg_usize(args, "comments", 30)),
    };
    Ok(done(Some(url), md, data))
}

async fn ddg_search(tab: &Tab, args: &Value, notify: &mpsc::Sender<Reply>) -> Result<Reply> {
    use crate::platforms::search;
    let q: String = url::form_urlencoded::byte_serialize(crate::validate::query(arg_str(args, "query")?)?.as_bytes()).collect();
    let url = format!("https://html.duckduckgo.com/html/?q={q}");
    tab.goto(&url, NAV_TIMEOUT).await?;
    let page = || async { search::parse_ddg(tab.eval("document.documentElement.outerHTML").await?.as_str().unwrap_or("")) };
    let mut results = match page().await {
        Ok(r) => r,
        Err(_) => {
            // A captcha: hand the tab to the user and continue once it is solved.
            tab.focus().await?;
            ask_user(notify, "DuckDuckGo shows a captcha. Please solve it in the agent browser window (waiting up to 120s).").await;
            wait_until(Duration::from_secs(120), || async { page().await.ok() })
                .await
                .context("the DuckDuckGo captcha was not solved in time")?
        }
    };
    results.truncate(arg_usize(args, "limit", 8));
    Ok(done(Some(url), search::render_ddg(&results), Value::Array(results)))
}

/// Tell the user (desktop notification + the agent's stderr) that the browser needs them.
async fn ask_user(notify: &mpsc::Sender<Reply>, message: &str) {
    // notify-send needs the session bus and display; nothing else is passed.
    let session = crate::tools::Extra {
        pass_env: &["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "DISPLAY", "WAYLAND_DISPLAY"],
        ..Default::default()
    };
    let args = ["--app-name=spyglass", "--", "spyglass", message];
    let _ = crate::tools::run_with("notify-send", &args, Duration::from_secs(5), 1024, &session).await;
    let _ = notify.send(Reply::Waiting { message: message.to_string() }).await;
}

/// Poll `check` every 2 s until it yields a value or `wait` elapses.
async fn wait_until<T, F, Fut>(wait: Duration, check: F) -> Option<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(v) = check().await {
            return Some(v);
        }
    }
    None
}

async fn web_render(tab: &Tab, args: &Value) -> Result<Reply> {
    let url = crate::validate::public_url(arg_str(args, "url")?)?.to_string();
    let script = "new Promise(r => setTimeout(r, 1500)).then(() => ({ url: location.href, html: document.documentElement.outerHTML }))";
    let v = scrape(tab, &url, script).await?;
    let final_url = v["url"].as_str().unwrap_or(&url).to_string();
    let doc = crate::platforms::web::extract(v["html"].as_str().unwrap_or(""), &final_url)?;
    Ok(done(Some(final_url), doc.markdown, doc.data))
}

/// Make sure the persistent profile is logged in to a platform. If not, open the
/// login page in a visible tab, tell the user, and wait for the session cookie.
pub async fn ensure_session(
    cdp: &Cdp,
    tab: &Tab,
    platform: &str,
    (cookie_url, cookie): (&str, &str),
    login_url: &str,
    wait: Duration,
    notify: &mpsc::Sender<Reply>,
) -> Result<()> {
    if tab.cookie(cookie_url, cookie).await?.is_some() {
        return Ok(());
    }
    let login = Tab::open(cdp, None, None).await?.keep_open();
    let _ = login.goto(login_url, NAV_TIMEOUT).await;
    let _ = login.focus().await;
    ask_user(
        notify,
        &format!("Not logged in to {platform}. Please log in in the agent browser window (waiting up to {}s).", wait.as_secs()),
    )
    .await;
    let logged_in = wait_until(wait, || async { tab.cookie(cookie_url, cookie).await.ok().flatten() }).await;
    // Close it either way, so login tabs do not pile up across attempts.
    login.close().await;
    match logged_in {
        Some(_) => Ok(()),
        None => bail!("timed out waiting for the {platform} login"),
    }
}

/// Load `url` and return the body of the first GraphQL response for operation `op`.
pub async fn capture_graphql(tab: &Tab, url: &str, op: &str, wait: Duration) -> Result<String> {
    let mut events = tab.events();
    tab.goto(url, NAV_TIMEOUT).await?;
    let marker = "/graphql/";
    let op_path = format!("/{op}");
    let response = tab
        .wait_for(&mut events, wait, |e| {
            e.method == "Network.responseReceived"
                && e.params["response"]["url"].as_str().is_some_and(|u| u.contains(marker) && u.contains(&op_path))
        })
        .await
        .with_context(|| format!("the page did not load {op} data"))?;
    let id = response.params["requestId"].as_str().unwrap_or("").to_string();
    tab.wait_for(&mut events, wait, |e| e.method == "Network.loadingFinished" && e.params["requestId"] == id.as_str()).await?;
    tab.response_body(&id).await
}
