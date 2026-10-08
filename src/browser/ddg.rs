//! Web search through DuckDuckGo in the agent browser (DuckDuckGo rejects non-browser TLS clients).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::scenario::{Ctx, Found, NAV_TIMEOUT, Site, arg_str, arg_usize, ask_user, require_visible, wait_until};
use super::tab::Allow;
use crate::platforms::search;

pub const SITE: Site = Site { allow: Allow::Domains(&["duckduckgo.com"]), scratch: false };

/// `search` via DuckDuckGo's HTML endpoint, with a captcha hand-off to the user.
pub async fn job(ctx: &Ctx<'_>, _verb: &str, args: &Value) -> Result<Found> {
    let Ctx { tab, notify, .. } = *ctx;
    let q = crate::validate::enc(&crate::validate::query(arg_str(args, "query")?)?);
    let url = format!("https://html.duckduckgo.com/html/?q={q}");
    tab.goto(&url, NAV_TIMEOUT).await?;
    let page = || async { search::parse_ddg(tab.eval("document.documentElement.outerHTML").await?.as_str().unwrap_or("")) };
    let mut results = match page().await {
        Ok(r) => r,
        Err(_) => {
            // A captcha: hand the tab to the user and continue once it is solved.
            require_visible(ctx.headless, "A DuckDuckGo captcha")?;
            tab.focus().await?;
            ask_user(notify, "DuckDuckGo shows a captcha. Please solve it in the agent browser window (waiting up to 120s).").await;
            wait_until(Duration::from_secs(120), || async { page().await.ok() })
                .await
                .context("the DuckDuckGo captcha was not solved in time")?
        }
    };
    results.truncate(arg_usize(args, "limit", 8));
    Ok(Found { url: Some(url), markdown: search::render_ddg(&results), data: Value::Array(results) })
}
