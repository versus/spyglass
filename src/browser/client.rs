//! CLI side of the agent browser: talk to the daemon, start it on demand.

use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::daemon::{log_path, socket_path};
use super::proto::{Reply, Request};

/// Send a request; `waiting` notices go to stderr, the final reply is returned.
pub async fn request(req: &Request) -> Result<Reply> {
    let stream = UnixStream::connect(socket_path()).await.context("the agent browser is not running")?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<Reply>(&line)? {
            Reply::Waiting { message } => eprintln!("spyglass: {}", crate::output::sanitize(&message)),
            reply => return Ok(reply),
        }
    }
    bail!("the agent browser closed the connection")
}

pub async fn is_running() -> bool {
    matches!(request(&Request::Ping).await, Ok(Reply::Done { .. }))
}

/// Running and built from this very binary?
async fn is_current() -> Option<bool> {
    match request(&Request::Ping).await {
        Ok(Reply::Done { data, .. }) => Some(data["build"].as_str() == Some(super::daemon::build_id().as_str())),
        _ => None,
    }
}

/// Start the daemon in the background (its own process group) and wait until it answers.
pub async fn start(headless: bool) -> Result<()> {
    match is_current().await {
        Some(true) => return Ok(()),
        Some(false) => {
            // spyglass was upgraded: replace the old daemon.
            let _ = request(&Request::Stop).await;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if !is_running().await {
                    break;
                }
            }
        }
        None => {}
    }
    let log_file = log_path();
    if let Some(dir) = log_file.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let log = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_file)?
    };
    daemon_command(&std::env::current_exe()?, headless).stderr(log).spawn().context("starting the agent browser")?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match is_current().await {
            Some(true) => return Ok(()),
            // The old daemon outlived the upgrade: never run jobs on stale scenarios.
            Some(false) => bail!("an older agent browser is still running; run `spyglass browser stop` and retry"),
            None => {}
        }
    }
    bail!("the agent browser did not start; see {}", log_file.display())
}

/// The daemon runs in its own process group (survives the terminal) and without API keys.
fn daemon_command(exe: &std::path::Path, headless: bool) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["browser", "daemon"]);
    if headless {
        cmd.arg("--headless");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
    crate::secrets::scrub_env(&mut cmd);
    cmd
}

/// Run a job, starting the (visible) agent browser first if needed.
pub async fn job(platform: &str, verb: &str, args: serde_json::Value) -> Result<Reply> {
    start(false).await?;
    request(&Request::Job { platform: platform.into(), verb: verb.into(), args }).await
}

/// Run a browser scenario and turn the daemon reply into a document.
pub async fn doc(source: &'static str, verb: &str, args: serde_json::Value) -> Result<crate::output::Doc> {
    match job(source, verb, args).await? {
        Reply::Done { url, markdown, data } => Ok(crate::output::Doc::new(source, url, markdown, data)),
        Reply::Error { message } | Reply::Waiting { message } => bail!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_never_inherits_api_keys() {
        assert!(crate::secrets::scrubs_all(&daemon_command(std::path::Path::new("/bin/spyglass"), false)));
    }
}
