//! Black-box tests of the `spyglass` binary. No network access needed.

use std::process::{Command, Output};

const MARKER: &str = "SECRET-MARKER-7f3a9c";

fn spyglass(args: &[&str], state_dir: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spyglass"))
        .args(args)
        .env("SPYGLASS_GITHUB_TOKEN", MARKER)
        .env("SPYGLASS_BRAVE_API_KEY", MARKER)
        .env("SPYGLASS_EXA_API_KEY", MARKER)
        .env("XDG_STATE_HOME", state_dir)
        .env("HOME", state_dir)
        .output()
        .expect("run spyglass")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn temp_state(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spyglass-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn secrets_never_appear_in_output_or_audit_log() {
    let state = temp_state("secrets");
    let runs = [
        spyglass(&["secrets", "list"], &state),
        spyglass(&["web", "read", "https://nonexistent.invalid/"], &state),
        spyglass(&["rss", "https://nonexistent.invalid/feed"], &state),
        spyglass(&["web", "read", "http://169.254.169.254/latest/meta-data/"], &state),
    ];
    for o in &runs {
        assert!(!text(o).contains(MARKER), "secret leaked: {}", text(o));
    }
    assert!(text(&runs[0]).contains("github-token   set"));
    // Linux: $XDG_STATE_HOME/spyglass; macOS has no XDG state dir: ~/Library/Application Support/spyglass.
    let dir = if cfg!(target_os = "macos") { state.join("Library/Application Support") } else { state.clone() };
    let audit = std::fs::read_to_string(dir.join("spyglass/audit.jsonl")).unwrap();
    assert_eq!(audit.lines().count(), runs.len());
    assert!(!audit.contains(MARKER));
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn ssrf_targets_fail_without_network_access() {
    let state = temp_state("ssrf");
    for url in ["http://127.0.0.1:22/", "http://[::1]/", "http://0x7f.1/", "http://metadata.google.internal/", "file:///etc/passwd"] {
        let o = spyglass(&["web", "read", url], &state);
        assert!(!o.status.success(), "{url} must be refused");
        assert!(text(&o).contains("error:"), "{}", text(&o));
    }
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn write_commands_do_not_exist() {
    let state = temp_state("write");
    for args in [&["x", "tweet", "hi"][..], &["github", "pr-create"], &["reddit", "comment", "x", "y"]] {
        let o = spyglass(args, &state);
        assert!(!o.status.success());
        assert!(text(&o).contains("unrecognized subcommand"), "{}", text(&o));
    }
    std::fs::remove_dir_all(state).unwrap();
}
