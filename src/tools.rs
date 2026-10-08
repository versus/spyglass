//! Running external programs (yt-dlp) safely:
//! no shell, argv only, absolute binary path, cleared environment, throwaway HOME,
//! no stdin, timeout, capped output.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

/// Locate `name` in absolute PATH entries only (never `.` or relative dirs).
pub fn find(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Run `name args…` and return stdout. Non-zero exit is an error carrying a stderr excerpt.
/// Additions to the otherwise empty child environment.
#[derive(Default)]
pub struct Extra<'a> {
    /// Directories prepended to PATH (e.g. where `deno` lives, for yt-dlp).
    pub path_dirs: &'a [PathBuf],
    /// Variables copied from our environment (e.g. D-Bus/display for notify-send).
    pub pass_env: &'a [&'a str],
}

pub async fn run(name: &str, args: &[&str], timeout: Duration, max_bytes: usize) -> Result<String> {
    run_with(name, args, timeout, max_bytes, &Extra::default()).await
}

pub async fn run_with(name: &str, args: &[&str], timeout: Duration, max_bytes: usize, extra: &Extra<'_>) -> Result<String> {
    let bin = find(name).ok_or_else(|| anyhow::anyhow!("`{name}` is not installed (not found in PATH)"))?;
    let home = ScratchDir::new()?;
    let dirs: Vec<String> =
        extra.path_dirs.iter().chain(bin.parent().map(Path::to_path_buf).as_ref()).map(|d| d.display().to_string()).collect();
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env_clear();
    for var in extra.pass_env.iter().filter(|v| !v.starts_with("SPYGLASS_")) {
        if let Some(value) = std::env::var_os(var) {
            cmd.env(var, value);
        }
    }
    let mut child = cmd
        .args(args)
        .env("PATH", format!("{}:/usr/bin:/bin", dirs.join(":")))
        .env("HOME", &home.0)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0) // its own group, so helpers it starts (e.g. deno) can be killed with it
        .spawn()
        .with_context(|| format!("starting {name}"))?;
    let _group = child.id().map(KillGroup); // whatever happens below, nothing outlives this call
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    // Drain stderr concurrently so a chatty child cannot block; keep only the head.
    let stderr_task = tokio::spawn(async move {
        let mut head = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if head.len() < STDERR_KEEP {
                head.extend_from_slice(&buf[..n]);
            }
        }
        head
    });
    let work = async {
        let mut out = Vec::new();
        (&mut stdout).take(max_bytes as u64 + 1).read_to_end(&mut out).await?;
        if out.len() > max_bytes {
            bail!("{name} produced more than {max_bytes} bytes of output");
        }
        let status = child.wait().await?;
        Ok((status, out))
    };
    let (status, out) = match tokio::time::timeout(timeout, work).await {
        Ok(r) => r?,
        Err(_) => bail!("{name} timed out after {}s", timeout.as_secs_f32()),
    };
    if !status.success() {
        // A grandchild may hold stderr open after the child exited: don't wait for it long.
        let head = tokio::time::timeout(Duration::from_secs(1), stderr_task).await.ok().and_then(Result::ok).unwrap_or_default();
        let err = String::from_utf8_lossy(&head).into_owned();
        let excerpt: String = err.chars().take(500).collect();
        bail!("{name} failed ({status}): {}", excerpt.trim());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

const STDERR_KEEP: usize = 64 * 1024;

/// Kills a whole process group (the child and anything it started) on drop.
struct KillGroup(u32);

impl Drop for KillGroup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Private temporary directory removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("spyglass-{}-{nanos}", std::process::id()));
        crate::paths::ensure_private_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn finds_only_existing_binaries() {
        assert!(find("printf").is_some());
        assert!(find("definitely-not-a-binary-xyz").is_none());
        assert!(find("../printf").is_none());
        assert!(find("/usr/bin/printf").is_none(), "names only, no paths");
    }

    #[tokio::test]
    async fn each_argument_is_exactly_one_argv_element() {
        let hostile = ["a \"quoted\" b", "it's", "$(touch /tmp/pwned)", "`id`", "x; rm -rf /", "--exec=evil", "new\nline"];
        let mut args = vec!["%s\\n--\\n"];
        args.extend(hostile);
        let out = run("printf", &args, T, 4096).await.unwrap();
        let parts: Vec<&str> = out.split("\n--\n").filter(|p| !p.is_empty()).collect();
        assert_eq!(parts, hostile);
    }

    #[tokio::test]
    async fn extra_path_dirs_and_passed_variables_reach_the_child() {
        let dir = std::env::temp_dir().join("spyglass-extra-path");
        let extra = Extra { path_dirs: std::slice::from_ref(&dir), pass_env: &["USER", "SPYGLASS_GITHUB_TOKEN"] };
        let out = run_with("env", &[], T, 4096, &extra).await.unwrap();
        let path = out.lines().find(|l| l.starts_with("PATH=")).unwrap();
        assert!(path.starts_with(&format!("PATH={}:", dir.display())), "{path}");
        if let Ok(user) = std::env::var("USER") {
            assert!(out.contains(&format!("USER={user}")));
        }
        assert!(!out.contains("SPYGLASS_"), "secrets can never be passed through");
        let plain = run("env", &[], T, 4096).await.unwrap();
        assert!(!plain.lines().any(|l| l.starts_with("USER=")));
    }

    #[tokio::test]
    async fn environment_is_cleared() {
        let out = run("env", &[], T, 4096).await.unwrap();
        assert!(!out.contains("SPYGLASS_"), "secrets env must not leak: {out}");
        assert!(out.lines().any(|l| l.starts_with("HOME=") && !l.contains(&std::env::var("HOME").unwrap())));
    }

    #[tokio::test]
    async fn enforces_timeout_and_output_cap() {
        assert!(run("sleep", &["5"], Duration::from_millis(200), 100).await.is_err());
        assert!(run("yes", &[], T, 1000).await.is_err());
    }

    #[tokio::test]
    async fn a_grandchild_holding_stderr_cannot_outlive_the_timeout() {
        let started = std::time::Instant::now();
        let r = run("sh", &["-c", "sleep 30 1>&2 & exit 3"], Duration::from_secs(5), 1024).await;
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_secs(8), "hung on stderr: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn leftover_grandchildren_are_killed() {
        let pid = run("sh", &["-c", "sleep 30 1>&2 & echo $!"], T, 1024).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill").args(["-0", pid.trim()]).stderr(Stdio::null()).status().unwrap().success();
        assert!(!alive, "grandchild {pid} still running");
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_with_stderr() {
        let err = run("ls", &["/definitely/missing/path"], T, 1000).await.unwrap_err().to_string();
        assert!(err.contains("missing"), "{err}");
    }
}
