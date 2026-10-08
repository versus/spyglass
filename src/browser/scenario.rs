//! Shared toolkit for browser scenarios: the job context, the result type and helpers.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::mpsc;

use super::cdp::Cdp;
use super::proto::Reply;
use super::tab::{Allow, Tab};

pub const NAV_TIMEOUT: Duration = Duration::from_secs(45);

/// Where a scenario runs: its network allowlist and browser context.
pub struct Site {
    pub allow: Allow,
    /// The cookie-less context (arbitrary pages) instead of the logged-in profile.
    pub scratch: bool,
    /// Needs the visible browser: the site blocks headless ones.
    pub visible: bool,
}

/// Everything a scenario gets: the browser, its agent tab, and a channel to the agent.
pub struct Ctx<'a> {
    pub cdp: &'a Cdp,
    pub tab: &'a Tab,
    pub notify: &'a mpsc::Sender<Reply>,
    /// No window: nobody can solve a captcha or log in.
    pub headless: bool,
}

/// The page is a bot wall: the daemon retries in the visible browser.
#[derive(Debug)]
pub struct Blocked(pub String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} blocked the headless browser", self.0)
    }
}

impl std::error::Error for Blocked {}

/// What every scenario returns.
pub struct Found {
    pub url: Option<String>,
    pub markdown: String,
    pub data: Value,
}

impl From<Found> for Reply {
    fn from(f: Found) -> Self {
        Reply::Done { url: f.url, markdown: f.markdown, data: f.data }
    }
}

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().with_context(|| format!("missing argument {key}"))
}

pub fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args[key].as_u64().map(|n| n as usize).unwrap_or(default).clamp(1, 100)
}

/// Load `url` in the agent tab and run one of our scripts.
pub async fn scrape(tab: &Tab, url: &str, script: &str) -> Result<Value> {
    tab.goto(url, NAV_TIMEOUT).await?;
    tab.eval(script).await
}

/// Page script: wait (≤10 s) until `ready` matches or the block page shows, then extract.
pub fn when_ready(ready: &str, blocked_marker: &str, extract: &str) -> String {
    format!(
        "new Promise(resolve => {{ const t0 = Date.now(); const tick = () => {{
            if ((document.body?.innerText || '').includes({blocked_marker:?})) return resolve({{ blocked: true }});
            if (document.querySelector({ready:?}) || Date.now() - t0 > 10000) return resolve({{ data: {extract} }});
            setTimeout(tick, 250); }}; tick(); }})"
    )
}

/// Tell the user (desktop notification + the agent's stderr) that the browser needs them.
/// A hand-off to the user needs a visible window; in headless mode fail fast instead of waiting.
pub fn require_visible(headless: bool, need: &str) -> Result<()> {
    if headless {
        bail!("{need} needs a person, but the agent browser runs headless: run `spyglass browser stop`, then `spyglass browser start`");
    }
    Ok(())
}

pub async fn ask_user(notify: &mpsc::Sender<Reply>, message: &str) {
    // notify-send needs the session bus and display; nothing else is passed.
    let session = crate::tools::Extra {
        pass_env: &["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "DISPLAY", "WAYLAND_DISPLAY"],
        ..Default::default()
    };
    let args = ["--app-name=spyglass", "--", "spyglass", message];
    let _ = crate::tools::run_with("notify-send", &args, Duration::from_secs(5), 1024, &session).await;
    let _ = notify.send(Reply::Waiting { message: message.to_string() }).await;
}

/// A captcha or bot check is in the way: bring the tab to the user, then poll `check`
/// until it yields (the check passed) or two minutes elapse.
pub async fn solve_by_user<T, F, Fut>(ctx: &Ctx<'_>, what: &str, check: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    require_visible(ctx.headless, what)?;
    ctx.tab.focus().await?;
    ask_user(ctx.notify, &format!("{what}: please complete it in the agent browser window (waiting up to 120s).")).await;
    wait_until(Duration::from_secs(120), check).await.with_context(|| format!("{what} was not completed in time"))
}

/// Poll `check` every 2 s until it yields a value or `wait` elapses.
pub async fn wait_until<T, F, Fut>(wait: Duration, check: F) -> Option<T>
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

/// Make sure the persistent profile is logged in to a platform. If not, open the
/// login page in a visible tab, tell the user, and wait for the session cookie.
pub async fn ensure_session(
    ctx: &Ctx<'_>,
    platform: &str,
    (cookie_url, cookie): (&str, &str),
    login_url: &str,
    wait: Duration,
) -> Result<()> {
    let Ctx { cdp, tab, notify, headless } = *ctx;
    if tab.cookie(cookie_url, cookie).await?.is_some() {
        return Ok(());
    }
    require_visible(headless, &format!("A login to {platform}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_needs_a_visible_browser() {
        assert!(require_visible(false, "a captcha").is_ok());
        let err = require_visible(true, "a captcha").unwrap_err().to_string();
        assert!(err.contains("a captcha") && err.contains("spyglass browser start"), "{err}");
    }
}
