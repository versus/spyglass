//! Web search through DuckDuckGo in the agent browser (DuckDuckGo rejects non-browser TLS clients).

use anyhow::Result;
use serde_json::Value;

use super::scenario::{Ctx, Found, NAV_TIMEOUT, Site, arg_str, arg_usize, solve_by_user};
use super::tab::Allow;
use crate::platforms::search;

pub const SITE: Site = Site { allow: Allow::Domains(&["duckduckgo.com"]), scratch: false, visible: true };

/// `search` via DuckDuckGo's HTML endpoint, with a captcha hand-off to the user.
pub async fn job(ctx: &Ctx<'_>, _verb: &str, args: &Value) -> Result<Found> {
    let tab = ctx.tab;
    let q = crate::validate::enc(&crate::validate::query(arg_str(args, "query")?)?);
    let url = format!("https://html.duckduckgo.com/html/?q={q}");
    tab.goto(&url, NAV_TIMEOUT).await?;
    let page = || async { search::parse_ddg(tab.eval("document.documentElement.outerHTML").await?.as_str().unwrap_or("")) };
    let mut results = match page().await {
        Ok(r) => r,
        Err(_) => solve_by_user(ctx, "A DuckDuckGo captcha", || async { page().await.ok() }).await?,
    };
    results.truncate(arg_usize(args, "limit", 8));
    Ok(Found { url: Some(url), markdown: search::render_ddg(&results), data: Value::Array(results) })
}
