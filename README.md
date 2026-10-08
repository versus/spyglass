# spyglass

Read-only internet access for AI agents, built security-first. One Rust binary, `spyglass`,
plus a ~550-token skill. A clean-room replacement for Agent-Reach.

```
spyglass web read <url> [--render] [--links]
spyglass github repo|readme|file|search-repos|search-code|issues|issue|prs|pr|releases
spyglass search "<query>"               spyglass youtube video|transcript|search
spyglass rss <url>                      spyglass reddit search|sub|post        (agent browser, no login)
spyglass doctor                         spyglass x search|user|post            (agent browser, log in once)
```

Every result is wrapped in `<untrusted source="…" url="…">…</untrusted>` and sanitized.

## Side-by-side: an agent with and without spyglass

Same model (Claude Sonnet), same prompt, one run each (October 2026).
**Without**: the agent's built-in web tools (WebFetch, WebSearch). **With**: spyglass only.

| Task | Without spyglass | With spyglass |
|---|---|---|
| **Summarize a YouTube talk** (RustConf 2026 keynote, 29 min) with key points and timestamps | ❌ No content. The YouTube page returned only site navigation and the captions endpoint returned nothing. The agent fell back to the conference abstract and said it could not describe the talk. | ✅ Summary built from the full transcript: 7 key points with timestamps and a conclusion. |
| **What's new in Rust 1.99, and what does r/rust think?** | ⚠️ Half done. It read the release blog, but only after guessing the URL because search did not find it. Reddit was unreachable (blocked, and a mirror returned 403), so there were no opinions. | ✅ Both parts. Release blog plus GitHub release notes, and 4 discussion threads from r/rust with authors and scores. |
| **What do people on r/niri say about switching from Hyprland?** 3 quotes with authors | ❌ None. reddit.com and old.reddit.com could not be fetched, search refused reddit.com, and the mirror did not resolve. | ✅ 3 real quotes with usernames, upvotes and thread links, plus a note that the subreddit leans pro-niri. |
| **tokio-rs/tokio: latest release and the newest issues** | ✅ Done from page summaries. It also caught an issue opened and closed the same day. | ✅ Done from raw API data: the full changelog with PR numbers. Open issues only by default (`--state all` includes closed ones). |

The built-in tools work well on ordinary pages and GitHub. spyglass matters where they cannot reach: video transcripts and Reddit.

## Requirements

| Dependency | Needed for | Install |
|---|---|---|
| Linux or macOS | everything | Windows is not supported yet |
| Google Chrome or Chromium | `search`, `reddit`, `x`, `web read --render` (the agent browser) | your package manager, or google.com/chrome |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | `youtube` | `pacman -S yt-dlp` · `brew install yt-dlp` · `pipx install yt-dlp` |
| [Deno](https://deno.com) | `youtube`: yt-dlp runs YouTube's player JavaScript with it (the only runtime it enables by default) | `pacman -S deno` · `brew install deno` |
| OS keyring (Secret Service: GNOME Keyring or KWallet; macOS Keychain) | `secrets set` (optional API keys) | usually already present on desktops |
| `notify-send` (libnotify) | optional desktop notifications when the browser needs you (Linux) | `pacman -S libnotify` |

`web read`, `rss` and `github` need nothing extra. `spyglass doctor` shows what is missing.

To build from source you also need Rust 1.87+, a C compiler and CMake (for the `aws-lc` crypto library).

## Install

Prebuilt binaries for Linux (x86_64, arm64) and macOS (Apple Silicon, Intel) are on the
[releases page](https://github.com/versus/spyglass/releases). Each archive has a SHA-256 file
and a signed build-provenance attestation:

```sh
gh attestation verify spyglass-v0.1.0-aarch64-apple-darwin.tar.gz --repo versus/spyglass
tar xzf spyglass-*.tar.gz && install -m 755 spyglass-*/spyglass ~/.local/bin/
# macOS, if downloaded with a browser: xattr -d com.apple.quarantine ~/.local/bin/spyglass
```

From source:

```sh
cargo build --release --locked && install -m 755 target/release/spyglass ~/.local/bin/
spyglass doctor
spyglass skill --install            # writes ~/.claude/skills/spyglass/SKILL.md (or: spyglass skill > SKILL.md)
spyglass secrets set brave-api-key  # optional: paid search API instead of DuckDuckGo; stored in the OS keyring
```

No Python runtime, no Node.js, no Docker for spyglass itself. See [Requirements](#requirements).

## The agent browser

Pages are tried in a **headless** Chrome first (no window). Sites that block headless browsers
(Reddit, X, DuckDuckGo search and a few others, plus any site that showed a bot check this session)
run in a **visible** Chrome window with its own profile
(`~/.local/share/spyglass/browser-profile`), started on demand. You can watch what
the agent does and log in where needed:

```sh
spyglass browser login x     # log in yourself; the session stays in the agent profile only
spyglass browser status | stop
```

If a job needs a login or DuckDuckGo shows a captcha, the window comes to the front, a desktop notification appears,
and the agent sees `spyglass: Not logged in to X…` while the job waits (up to 5 minutes).
Use a dedicated account: automation is against the platforms' terms and accounts can be limited.

## Security model

| Threat | Defense |
|---|---|
| Prompt injection in fetched content | `<untrusted>` envelope (errors too, since they can quote remote content); ANSI/bidi/zero-width/line-separator/tag-character stripping; size caps |
| Agent tricked into acting (posting, DMs) | no write commands exist; browser scenarios are fixed scripts, the agent passes only queries/URLs |
| SSRF to localhost / cloud metadata / LAN | HTTP: URL validation + DNS resolver that rejects non-public IPs and pins the connection + re-check on every redirect; env proxies ignored. Browser: every request is checked by name and by DNS before Chrome may send it (residual risk: DNS rebinding between our lookup and Chrome's) |
| Shell / argument injection | no shell anywhere; argv only; `--` before user values; strict validators |
| Hostile external tool config (`yt-dlp --exec`) | `--ignore-config`, cleared environment, throwaway `HOME`, timeout, output cap |
| Malicious page attacking logged-in sessions | arbitrary URLs render in a separate cookie-less browser context |
| Browser hijack by local processes | Chrome is driven over `--remote-debugging-pipe`; no TCP port; daemon socket is `0600` |
| Network exfiltration from scenario tabs | per-scenario domain allowlist; media blocked |
| Secret leaks | OS keyring; secrets only in request headers of API calls that never follow redirects; not inherited by the daemon or Chrome; tests assert they never reach output or the audit log |
| Supply chain | `Cargo.lock`, `cargo-deny`, SHA-pinned CI actions, no remote instructions, no self-update |

Audit log (no secrets, no bodies): `~/.local/state/spyglass/audit.jsonl` (macOS: `~/Library/Application Support/spyglass/audit.jsonl`).

## Development

TDD, KISS, YAGNI, DRY. `cargo test` (no network), `cargo clippy --all-targets -- -D warnings`,
`cargo test -- --ignored` runs the real-Chrome test.

## License

MIT — see [LICENSE](LICENSE). Clean-room implementation: no code was copied from Agent-Reach.
