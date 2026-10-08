//! JavaScript-rendered pages in the cookie-less browser context.

use anyhow::Result;
use serde_json::Value;

use super::scenario::{Ctx, Found, Site, arg_str, scrape};
use super::tab::Allow;

pub const SITE: Site = Site { allow: Allow::AnyPublic, scratch: true };

/// `web read --render`: an arbitrary page in the cookie-less context.
pub async fn job(ctx: &Ctx<'_>, _verb: &str, args: &Value) -> Result<Found> {
    let url = crate::validate::public_url(arg_str(args, "url")?)?.to_string();
    let script = "new Promise(r => setTimeout(r, 1500)).then(() => ({ url: location.href, html: document.documentElement.outerHTML }))";
    let v = scrape(ctx.tab, &url, script).await?;
    let final_url = v["url"].as_str().unwrap_or(&url).to_string();
    let doc = crate::platforms::web::extract(v["html"].as_str().unwrap_or(""), &final_url)?;
    Ok(Found { url: Some(final_url), markdown: doc.markdown, data: doc.data })
}
