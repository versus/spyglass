//! Launch the system Chrome with a dedicated profile, controlled only through
//! `--remote-debugging-pipe` (fd 3: commands in, fd 4: responses out).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Stdio};

use anyhow::{Context, Result};
use command_fds::{CommandFdExt, FdMapping};
use tokio::sync::mpsc;

use super::cdp::Cdp;
use crate::tools;

const BROWSERS: &[&str] = &["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"];

/// macOS apps are not on PATH.
const APP_PATHS: &[&str] =
    &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/Applications/Chromium.app/Contents/MacOS/Chromium"];

pub fn find_chrome() -> Option<std::path::PathBuf> {
    BROWSERS.iter().find_map(|b| tools::find(b)).or_else(|| first_existing(APP_PATHS))
}

fn first_existing(paths: &[&str]) -> Option<std::path::PathBuf> {
    paths.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// Flags for a quiet, private, extension-free profile. Chrome's own sandbox stays on.
pub fn chrome_args(profile: &Path, headless: bool) -> Vec<String> {
    let mut args = vec![
        "--remote-debugging-pipe".to_string(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-sync".into(),
        "--disable-extensions".into(),
        "--disable-component-extensions-with-background-pages".into(),
        "--disable-background-networking".into(),
        "--disable-features=AutofillServerCommunication,Translate,MediaRouter,OptimizationHints".into(),
        // Windows appear only when a scenario opens its tab: no empty startup window.
        "--no-startup-window".into(),
    ];
    if headless {
        args.push("--headless=new".into());
    }
    args
}

pub fn launch(profile: &Path, headless: bool) -> Result<(Cdp, Child)> {
    let bin = find_chrome().context("Chrome/Chromium not found in PATH")?;
    crate::paths::ensure_private_dir(profile)?;
    let (to_chrome_r, mut to_chrome_w) = std::io::pipe()?;
    let (from_chrome_r, from_chrome_w) = std::io::pipe()?;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(chrome_args(profile, headless)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.fd_mappings(vec![
        FdMapping { parent_fd: to_chrome_r.into(), child_fd: 3 },
        FdMapping { parent_fd: from_chrome_w.into(), child_fd: 4 },
    ])?;
    let child = cmd.spawn().context("starting Chrome")?;
    drop(cmd); // close our copies of the child's pipe ends so EOF is detected

    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    std::thread::spawn(move || {
        while let Some(msg) = out_rx.blocking_recv() {
            if to_chrome_w.write_all(msg.as_bytes()).and_then(|_| to_chrome_w.write_all(&[0])).is_err() {
                break;
            }
        }
    });
    let (in_tx, in_rx) = mpsc::channel::<String>(1024);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(from_chrome_r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(0, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if buf.last() == Some(&0) {
                        buf.pop();
                    }
                    if in_tx.blocking_send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok((Cdp::new(out_tx, in_rx), child))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_never_open_a_debugging_port_or_disable_sandbox() {
        let args = chrome_args(Path::new("/tmp/p"), true);
        assert!(args.contains(&"--remote-debugging-pipe".to_string()));
        assert!(args.iter().all(|a| !a.starts_with("--remote-debugging-port") && !a.starts_with("--remote-debugging-address")));
        assert!(args.iter().all(|a| a != "--no-sandbox"));
        assert!(args.contains(&"--headless=new".to_string()));
        assert!(!chrome_args(Path::new("/tmp/p"), false).contains(&"--headless=new".to_string()));
        assert!(args.contains(&"--no-startup-window".to_string()), "no empty startup window");
        assert!(!args.iter().any(|a| a == "about:blank"));
    }

    #[test]
    fn falls_back_to_app_bundle_paths() {
        let dir = std::env::temp_dir().join(format!("spyglass-chrome-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("Google Chrome");
        std::fs::write(&app, "").unwrap();
        let missing = dir.join("missing");
        let paths = [missing.to_str().unwrap(), app.to_str().unwrap()];
        assert_eq!(first_existing(&paths), Some(app.clone()));
        assert_eq!(first_existing(&paths[..1]), None);
        assert!(APP_PATHS.iter().any(|p| p.contains("Google Chrome.app")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Needs a real Chrome: `cargo test -- --ignored launches_real_chrome`.
    #[tokio::test]
    #[ignore]
    async fn launches_real_chrome_over_pipe_without_tcp_port() {
        let profile = std::env::temp_dir().join(format!("spyglass-chrome-test-{}", std::process::id()));
        let (cdp, mut child) = launch(&profile, true).unwrap();
        let v = cdp.call("Browser.getVersion", serde_json::json!({}), None).await.unwrap();
        assert!(v["product"].as_str().unwrap().contains("Chrome"), "{v}");
        let ss = std::process::Command::new("ss").args(["-ltnpH"]).output().unwrap();
        let listening = String::from_utf8_lossy(&ss.stdout);
        assert!(!listening.contains(&format!("pid={},", child.id())), "Chrome must not listen on TCP: {listening}");
        let _ = cdp.call("Browser.close", serde_json::json!({}), None).await;
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(profile);
    }
}
