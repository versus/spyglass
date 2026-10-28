//! `web download`: a file through the agent browser (cookie-less context, same network guard
//! and bot-check hand-off as `--render`). Chrome writes it into a private directory; the CLI
//! then moves it to the path the user chose.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::render::looks_blocked;
use super::scenario::{Blocked, Ctx, Found, NAV_TIMEOUT, arg_str, solve_by_user};

/// Larger downloads are cancelled.
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Where Chrome writes downloads (files are named by Chrome's download guid).
pub fn dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("spyglass").join("downloads")
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    None,
    Begun { guid: String, name: String },
    Done { guid: String, name: String, bytes: u64 },
    Failed(String),
}

/// Follow the first download of this job; cancel it when it grows past the cap.
fn watch_downloads(ctx: &Ctx<'_>) -> (watch::Receiver<Status>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = watch::channel(Status::None);
    let mut events = ctx.cdp.subscribe();
    let cdp = ctx.cdp.clone();
    let context = ctx.context.map(str::to_string);
    let task = tokio::spawn(async move {
        let mut current: Option<(String, String)> = None;
        while let Ok(ev) = events.recv().await {
            let p = &ev.params;
            match ev.method.as_str() {
                "Browser.downloadWillBegin" if current.is_none() => {
                    let guid = p["guid"].as_str().unwrap_or("").to_string();
                    let name = p["suggestedFilename"].as_str().unwrap_or("download").to_string();
                    current = Some((guid.clone(), name.clone()));
                    let _ = tx.send(Status::Begun { guid, name });
                }
                "Browser.downloadProgress" => {
                    let Some((guid, name)) = current.clone().filter(|(g, _)| p["guid"] == g.as_str()) else { continue };
                    let bytes = p["receivedBytes"].as_u64().unwrap_or(0).max(p["totalBytes"].as_u64().unwrap_or(0));
                    match p["state"].as_str() {
                        _ if bytes > MAX_BYTES => {
                            let mut params = json!({ "guid": guid });
                            if let Some(c) = &context {
                                params["browserContextId"] = json!(c);
                            }
                            let _ = cdp.call("Browser.cancelDownload", params, None).await;
                            let _ = tx.send(Status::Failed(format!("the file is larger than {} MB", MAX_BYTES >> 20)));
                            break;
                        }
                        Some("completed") => {
                            let bytes = p["receivedBytes"].as_u64().unwrap_or(0);
                            let _ = tx.send(Status::Done { guid, name, bytes });
                            break;
                        }
                        Some("canceled") => {
                            let _ = tx.send(Status::Failed("the download was cancelled".into()));
                            break;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });
    (rx, task)
}

/// Aborts the watcher even when the job is cancelled.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn wait_status(rx: &mut watch::Receiver<Status>, wait: Duration, pred: impl Fn(&Status) -> bool) -> bool {
    tokio::time::timeout(wait, rx.wait_for(|s| pred(s))).await.is_ok_and(|r| r.is_ok())
}

/// Save the page's own resource as a download (a file Chrome displays instead of downloading:
/// PDF, image, text). Our script; the file name is passed as a JSON string literal.
fn save_script(name: &str) -> String {
    let name = serde_json::to_string(name).unwrap_or_default();
    format!(
        "(async () => {{ const r = await fetch(location.href); if (!r.ok) return 'HTTP ' + r.status;
            const a = document.createElement('a'); a.href = URL.createObjectURL(await r.blob());
            a.download = {name}; a.click(); return null; }})()"
    )
}

/// Last path segment of the URL, or "download".
fn name_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments()?.next_back().filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| "download".into())
}

pub async fn job(ctx: &Ctx<'_>, args: &Value) -> Result<Found> {
    let url = crate::validate::public_url(arg_str(args, "url")?)?.to_string();
    let dir = dir();
    crate::paths::ensure_private_dir(&dir)?;
    let mut params = json!({ "behavior": "allowAndName", "downloadPath": dir, "eventsEnabled": true });
    if let Some(c) = ctx.context {
        params["browserContextId"] = json!(c);
    }
    ctx.cdp.call("Browser.setDownloadBehavior", params, None).await?;
    let (mut status, task) = watch_downloads(ctx);
    let _watcher = AbortOnDrop(task);
    let started = |s: &Status| *s != Status::None;

    // A file served as an attachment aborts the navigation and starts a download.
    let nav = ctx.tab.goto(&url, NAV_TIMEOUT).await;
    if !wait_status(&mut status, Duration::from_millis(500), started).await {
        if let Err(e) = nav {
            if !wait_status(&mut status, Duration::from_secs(2), started).await {
                return Err(e);
            }
        } else {
            let page = "({ title: document.title, text: (document.body?.innerText || '').slice(0, 3000) })";
            let wall = |v: &Value| looks_blocked(v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or(""), None);
            if ctx.tab.eval(page).await.is_ok_and(|v| wall(&v)) {
                let host = url::Url::parse(&url)?.host_str().unwrap_or("the site").to_string();
                if ctx.headless {
                    return Err(Blocked(host).into());
                }
                // Solved when the page shows content, or the check redirected straight into the download.
                let check = || async {
                    let begun = started(&status.borrow());
                    (begun || ctx.tab.eval(page).await.is_ok_and(|v| !wall(&v))).then_some(())
                };
                solve_by_user(ctx, &format!("{host} shows a bot check"), check).await?;
            }
            if !started(&status.borrow()) {
                if let Some(err) = ctx.tab.eval(&save_script(&name_from_url(&url))).await?.as_str() {
                    bail!("download failed: {err}");
                }
                if !wait_status(&mut status, Duration::from_secs(10), started).await {
                    bail!("the browser did not start a download");
                }
            }
        }
    }
    let finished = |s: &Status| matches!(s, Status::Done { .. } | Status::Failed(_));
    if !wait_status(&mut status, DOWNLOAD_TIMEOUT, finished).await {
        let begun = status.borrow().clone();
        if let Status::Begun { guid, .. } = begun {
            let mut params = json!({ "guid": guid });
            if let Some(c) = ctx.context {
                params["browserContextId"] = json!(c);
            }
            let _ = ctx.cdp.call("Browser.cancelDownload", params, None).await;
        }
        bail!("the download did not finish within {}s", DOWNLOAD_TIMEOUT.as_secs());
    }
    let done = status.borrow().clone();
    match done {
        Status::Done { guid, name, bytes } => Ok(Found {
            url: Some(url),
            markdown: format!("Downloaded {name} ({bytes} bytes)"),
            data: json!({ "guid": guid, "name": name, "bytes": bytes }),
        }),
        Status::Failed(e) => bail!(e),
        _ => unreachable!("waited for a finished download"),
    }
}

/// CLI side: move the finished download out of the private directory to `out` (never overwrites).
pub fn take(guid: &str, out: &Path) -> Result<()> {
    if guid.is_empty() || !guid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("bad download id");
    }
    let src = dir().join(guid);
    if out.exists() {
        let _ = std::fs::remove_file(&src);
        bail!("{} already exists", out.display());
    }
    // Different filesystems: copy, then remove.
    if std::fs::rename(&src, out).is_err() {
        std::fs::copy(&src, out).with_context(|| format!("writing {}", out.display()))?;
        let _ = std::fs::remove_file(&src);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_comes_from_the_url_path() {
        assert_eq!(name_from_url("https://e.com/papers/a.pdf?x=1"), "a.pdf");
        assert_eq!(name_from_url("https://e.com/"), "download");
    }

    #[test]
    fn the_name_cannot_break_out_of_the_script() {
        let js = save_script(r#"a"; alert(1); ""#);
        assert!(js.contains(r#"a.download = "a\"; alert(1); \"""#), "{js}");
    }

    #[test]
    fn only_download_ids_are_taken_from_the_daemon() {
        let out = std::env::temp_dir().join("spyglass-take-test");
        for bad in ["", "../etc/passwd", "a/b", ".."] {
            assert!(take(bad, &out).unwrap_err().to_string().contains("bad download id"), "{bad}");
        }
    }
}
