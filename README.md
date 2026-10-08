# spyglass

Read-only internet access for AI agents, built security-first. One Rust binary, `spyglass`,
plus a ~550-token skill. A clean-room replacement for Agent-Reach.

```
spyglass web read <url> [--render]      spyglass github repo|readme|file|search-repos|search-code|issues|issue|prs|pr|releases
spyglass search "<query>"               spyglass youtube video|transcript|search
spyglass rss <url>                      spyglass reddit search|sub|post        (agent browser, no login)
spyglass doctor                         spyglass x search|user|post            (agent browser, log in once)
```

Every result is wrapped in `<untrusted source="…" url="…">…</untrusted>` and sanitized.

## Install

```sh
cargo build --release --locked && install -m 755 target/release/spyglass ~/.local/bin/
spyglass doctor
spyglass skill --install            # writes ~/.claude/skills/spyglass/SKILL.md (or: spyglass skill > SKILL.md)
spyglass secrets set brave-api-key  # optional: paid search API instead of DuckDuckGo; stored in the OS keyring
```

Runtime dependencies: Google Chrome or Chromium (search, Reddit, X, `--render`), `yt-dlp` (YouTube).
No Python, no Node.js, no Docker.

## The agent browser

Search (DuckDuckGo), Reddit and X run in a **visible** Chrome window with its own profile
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
| Prompt injection in fetched content | `<untrusted>` envelope; ANSI/bidi/zero-width/tag-character stripping; size caps |
| Agent tricked into acting (posting, DMs) | no write commands exist; browser scenarios are fixed scripts, the agent passes only queries/URLs |
| SSRF to localhost / cloud metadata / LAN | URL validation + DNS resolver that rejects non-public IPs + re-check on every redirect; env proxies ignored |
| Shell / argument injection | no shell anywhere; argv only; `--` before user values; strict validators |
| Hostile external tool config (`yt-dlp --exec`) | `--ignore-config`, cleared environment, throwaway `HOME`, timeout, output cap |
| Malicious page attacking logged-in sessions | arbitrary URLs render in a separate cookie-less browser context |
| Browser hijack by local processes | Chrome is driven over `--remote-debugging-pipe`; no TCP port; daemon socket is `0600` |
| Network exfiltration from scenario tabs | per-scenario domain allowlist; media blocked |
| Secret leaks | OS keyring; secrets only in request headers; tests assert they never reach output or the audit log |
| Supply chain | `Cargo.lock`, `cargo-deny`, SHA-pinned CI actions, no remote instructions, no self-update |

Audit log (no secrets, no bodies): `~/.local/state/spyglass/audit.jsonl`.

## Development

TDD, KISS, YAGNI, DRY. `cargo test` (no network), `cargo clippy --all-targets -- -D warnings`,
`cargo test -- --ignored` runs the real-Chrome test. Plan: `PLAN.md`; MVP criteria: `MVP.md`.

## License

MIT — see [LICENSE](LICENSE). Clean-room implementation: no code was copied from Agent-Reach.
