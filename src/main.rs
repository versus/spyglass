#![forbid(unsafe_code)]

mod audit;
mod browser;
mod doctor;
mod net;
mod output;
mod paths;
mod platforms;
mod secrets;
mod skill;
mod tools;
mod validate;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use output::{Doc, Render};

/// Read-only internet access for AI agents. All output is untrusted data.
#[derive(Parser, Debug)]
#[command(name = "spyglass", version, about)]
struct Cli {
    /// Maximum characters of output.
    #[arg(long, global = true, default_value_t = 8000)]
    max_chars: usize,
    /// Structured JSON instead of Markdown.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Web pages.
    #[command(subcommand)]
    Web(WebCmd),
    /// Latest entries of an RSS/Atom feed.
    Rss {
        url: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// GitHub repositories, files, issues, pull requests, releases (read-only).
    #[command(subcommand)]
    Github(GhCmd),
    /// Web search: DuckDuckGo via the agent browser (no key), or Brave/Exa with an API key.
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_enum)]
        provider: Option<platforms::search::Provider>,
    },
    /// YouTube videos: metadata, transcripts, search (needs yt-dlp).
    #[command(subcommand)]
    Youtube(YtCmd),
    /// Reddit via the agent browser (no login needed).
    #[command(subcommand)]
    Reddit(RedditCmd),
    /// X/Twitter via the agent browser (log in once: `spyglass browser login x`).
    #[command(subcommand)]
    X(XCmd),
    /// The visible agent browser (separate profile; you can watch and log in).
    #[command(subcommand)]
    Browser(BrowserCmd),
    /// What works right now (read-only check).
    Doctor,
    /// Print the agent skill (SKILL.md), or install it for Claude Code.
    Skill {
        /// Write it to ~/.claude/skills/spyglass/SKILL.md.
        #[arg(long)]
        install: bool,
    },
    /// Manage API keys in the OS keyring (values are never printed).
    #[command(subcommand)]
    Secrets(SecretsCmd),
}

#[derive(Subcommand, Debug)]
enum GhCmd {
    /// Repository overview (owner/repo or URL).
    Repo { repo: String },
    /// README as Markdown.
    Readme { repo: String },
    /// A file from the repository.
    File {
        repo: String,
        path: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Search repositories.
    SearchRepos {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Search code (needs github-token).
    SearchCode {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// List issues.
    Issues {
        repo: String,
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// One issue with comments.
    Issue {
        repo: String,
        number: u64,
        #[arg(long, default_value_t = 20)]
        comments: usize,
    },
    /// List pull requests.
    Prs {
        repo: String,
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// One pull request with comments.
    Pr {
        repo: String,
        number: u64,
        #[arg(long, default_value_t = 20)]
        comments: usize,
    },
    /// Latest releases.
    Releases {
        repo: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
enum YtCmd {
    /// Title, channel, stats, chapters, description.
    Video { video: String },
    /// Subtitles as text (uploaded first, then auto-generated).
    Transcript {
        video: String,
        /// Preferred languages, comma-separated (default: video language, then en).
        #[arg(long, value_delimiter = ',')]
        lang: Vec<String>,
    },
    /// Search videos.
    Search {
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
enum RedditCmd {
    /// Search posts.
    Search {
        query: String,
        /// Restrict to a subreddit.
        #[arg(long)]
        sub: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Posts of a subreddit.
    Sub {
        name: String,
        /// hot, new, top (week) or rising.
        #[arg(long, default_value = "hot")]
        sort: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// A post with its comments (URL or id).
    Post {
        post: String,
        #[arg(long, default_value_t = 30)]
        comments: usize,
    },
}

#[derive(Subcommand, Debug)]
enum XCmd {
    /// Search posts.
    Search {
        query: String,
        /// Latest instead of top.
        #[arg(long)]
        latest: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// A post with its replies (URL or id).
    Post {
        post: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Recent posts of a user.
    User {
        handle: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
enum BrowserCmd {
    /// Start the agent browser window.
    Start {
        #[arg(long)]
        headless: bool,
    },
    /// Is it running?
    Status,
    /// Close the agent browser.
    Stop,
    /// Open a login page (x or reddit) in the agent browser; you log in yourself.
    Login { platform: String },
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        headless: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SecretsCmd {
    /// Store a key (prompted without echo, or read from stdin with --stdin).
    Set {
        name: String,
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a key.
    Rm { name: String },
    /// Show which keys are configured.
    List,
}

/// Output of a command: fetched content (untrusted) or our own status text.
enum Out {
    Doc(Doc),
    Plain(String),
}

/// Clamp a user-supplied list size to a sane range.
fn lim(n: usize) -> usize {
    n.clamp(1, 50)
}

#[derive(Subcommand, Debug)]
enum WebCmd {
    /// Read a page as clean Markdown.
    Read {
        url: String,
        /// Render JavaScript in the agent browser (cookie-less context).
        #[arg(long)]
        render: bool,
        /// Append the page's links (up to 50).
        #[arg(long)]
        links: bool,
        /// Last resort, only with the user's consent: fetch through the free Jina Reader
        /// (r.jina.ai, a third party that sees the URL). Private-looking links are refused.
        #[arg(long, conflicts_with = "render")]
        via_jina: bool,
    },
}

impl Cmd {
    /// Short name for the audit log, e.g. "web read".
    fn label(&self) -> &'static str {
        match self {
            Cmd::Web(WebCmd::Read { via_jina: true, .. }) => "web read --via-jina",
            Cmd::Web(WebCmd::Read { render: false, .. }) => "web read",
            Cmd::Web(WebCmd::Read { render: true, .. }) => "web read --render",
            Cmd::Reddit(_) => "reddit",
            Cmd::X(_) => "x",
            Cmd::Browser(_) => "browser",
            Cmd::Doctor => "doctor",
            Cmd::Skill { .. } => "skill",
            Cmd::Rss { .. } => "rss",
            Cmd::Github(_) => "github",
            Cmd::Youtube(_) => "youtube",
            Cmd::Search { .. } => "search",
            Cmd::Secrets(_) => "secrets",
        }
    }
}

async fn run(cmd: &Cmd) -> Result<Out> {
    match cmd {
        Cmd::Secrets(sc) => return secrets_cmd(sc).map(Out::Plain),
        Cmd::Browser(bc) => return browser_cmd(bc).await.map(Out::Plain),
        Cmd::Doctor => return Ok(Out::Plain(doctor::run().await)),
        Cmd::Skill { install } => return skill::run(*install).map(Out::Plain),
        Cmd::Reddit(rc) => return reddit_cmd(rc).await.map(Out::Doc),
        Cmd::X(xc) => return x_cmd(xc).await.map(Out::Doc),
        Cmd::Web(WebCmd::Read { url, render: true, links, .. }) => {
            return browser::client::doc("web", "render", serde_json::json!({ "url": url, "links": links })).await.map(Out::Doc);
        }
        _ => {}
    }
    let net = net::Net::new()?;
    let doc = match cmd {
        Cmd::Web(WebCmd::Read { url, via_jina: true, .. }) => platforms::jina::read(&net, url).await?,
        Cmd::Web(WebCmd::Read { url, links, .. }) => platforms::web::read(&net, url, *links).await?,
        Cmd::Rss { url, limit } => platforms::rss::read(&net, url, lim(*limit)).await?,
        Cmd::Github(g) => github_cmd(&net, g).await?,
        Cmd::Search { query, limit, provider } => platforms::search::search(&net, query, lim(*limit), *provider).await?,
        Cmd::Youtube(YtCmd::Video { video }) => platforms::youtube::video(video).await?,
        Cmd::Youtube(YtCmd::Transcript { video, lang }) => platforms::youtube::transcript(&net, video, lang).await?,
        Cmd::Youtube(YtCmd::Search { query, limit }) => platforms::youtube::search(query, lim(*limit)).await?,
        Cmd::Secrets(_) | Cmd::Browser(_) | Cmd::Reddit(_) | Cmd::X(_) | Cmd::Doctor | Cmd::Skill { .. } => {
            unreachable!("handled above")
        }
    };
    Ok(Out::Doc(doc))
}

async fn github_cmd(net: &net::Net, cmd: &GhCmd) -> Result<Doc> {
    let gh = platforms::github::GitHub::new(net);
    match cmd {
        GhCmd::Repo { repo } => gh.repo(repo).await,
        GhCmd::Readme { repo } => gh.readme(repo).await,
        GhCmd::File { repo, path, git_ref } => gh.file(repo, path, git_ref.as_deref()).await,
        GhCmd::SearchRepos { query, limit } => gh.search_repos(query, lim(*limit)).await,
        GhCmd::SearchCode { query, limit } => gh.search_code(query, lim(*limit)).await,
        GhCmd::Issues { repo, state, limit } => gh.list(repo, false, state, lim(*limit)).await,
        GhCmd::Prs { repo, state, limit } => gh.list(repo, true, state, lim(*limit)).await,
        GhCmd::Issue { repo, number, comments } => gh.thread(repo, *number, false, lim(*comments)).await,
        GhCmd::Pr { repo, number, comments } => gh.thread(repo, *number, true, lim(*comments)).await,
        GhCmd::Releases { repo, limit } => gh.releases(repo, lim(*limit)).await,
    }
}

async fn reddit_cmd(cmd: &RedditCmd) -> Result<Doc> {
    use serde_json::json;
    let (verb, args) = match cmd {
        RedditCmd::Search { query, sub, limit } => ("search", json!({ "query": query, "sub": sub, "limit": lim(*limit) })),
        RedditCmd::Sub { name, sort, limit } => ("sub", json!({ "name": name, "sort": sort, "limit": lim(*limit) })),
        RedditCmd::Post { post, comments } => ("post", json!({ "post": post, "comments": (*comments).clamp(1, 100) })),
    };
    browser::client::doc("reddit", verb, args).await
}

async fn x_cmd(cmd: &XCmd) -> Result<Doc> {
    use serde_json::json;
    let (verb, args) = match cmd {
        XCmd::Search { query, latest, limit } => ("search", json!({ "query": query, "latest": latest, "limit": lim(*limit) })),
        XCmd::Post { post, limit } => ("post", json!({ "post": post, "limit": lim(*limit) })),
        XCmd::User { handle, limit } => ("user", json!({ "handle": handle, "limit": lim(*limit) })),
    };
    browser::client::doc("x", verb, args).await
}

async fn browser_cmd(cmd: &BrowserCmd) -> Result<String> {
    use browser::{client, proto::Reply, proto::Request};
    let text = |r: Reply| match r {
        Reply::Done { markdown, .. } => Ok(markdown),
        Reply::Error { message } | Reply::Waiting { message } => Err(anyhow::anyhow!(message)),
    };
    match cmd {
        BrowserCmd::Daemon { headless } => browser::daemon::serve(*headless).await.map(|_| String::new()),
        BrowserCmd::Start { headless } => {
            client::start(*headless).await?;
            text(client::request(&Request::Ping).await?)
        }
        BrowserCmd::Status => match client::request(&Request::Ping).await {
            Ok(r) => text(r),
            Err(_) => Ok("agent browser is not running".into()),
        },
        BrowserCmd::Stop => match client::request(&Request::Stop).await {
            Ok(r) => text(r),
            Err(_) => Ok("agent browser is not running".into()),
        },
        BrowserCmd::Login { platform } => {
            client::start(false).await?;
            text(client::request(&Request::Login { platform: platform.clone() }).await?)
        }
    }
}

fn secrets_cmd(cmd: &SecretsCmd) -> Result<String> {
    use secrets::Secret;
    match cmd {
        SecretsCmd::Set { name, stdin } => {
            let secret = Secret::parse(name)?;
            let value = if *stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line
            } else {
                rpassword::prompt_password(format!("{name}: "))?
            };
            secrets::set(secret, &value)?;
            Ok(format!("{name} saved to the OS keyring"))
        }
        SecretsCmd::Rm { name } => {
            secrets::remove(Secret::parse(name)?)?;
            Ok(format!("{name} removed"))
        }
        SecretsCmd::List => Ok(Secret::ALL
            .iter()
            .map(|s| format!("{:<14} {}", s.name(), if secrets::get(*s).is_some() { "set" } else { "not set" }))
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

/// Write the final output; a closed pipe (`spyglass … | head`) is not an error.
fn emit(out: &mut impl std::io::Write, text: &str) {
    let _ = writeln!(out, "{text}");
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(&cli.cmd).await;
    let (ok, text, host) = match &result {
        Ok(Out::Plain(text)) => (true, text.clone(), None),
        Ok(Out::Doc(doc)) => {
            let host = doc.url.as_deref().and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string));
            (true, output::render(doc, Render { max_chars: cli.max_chars, json: cli.json }), host)
        }
        Err(e) => (false, output::render_error(&format!("{e:#}")), None),
    };
    if let Some(path) = audit::default_path() {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        audit::record(&path, &audit::Entry { ts, command: cli.cmd.label(), host, ok, output_chars: text.len() });
    }
    if ok {
        emit(&mut std::io::stdout(), &text);
        ExitCode::SUCCESS
    } else {
        emit(&mut std::io::stderr(), &text);
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("spyglass").chain(args.iter().copied()))
    }

    #[test]
    fn parses_read_commands() {
        assert!(parse(&["web", "read", "https://example.com"]).is_ok());
        assert!(parse(&["rss", "https://e.com/feed", "--limit", "3", "--json"]).is_ok());
        assert!(parse(&["--max-chars", "100", "web", "read", "x.com"]).is_ok());
        assert!(parse(&["github", "issue", "rust-lang/rust", "1", "--comments", "5"]).is_ok());
        assert!(parse(&["github", "file", "a/b", "src/x.rs", "--ref", "main"]).is_ok());
        assert!(parse(&["secrets", "set", "github-token", "--stdin"]).is_ok());
        assert!(parse(&["reddit", "search", "tokio", "--sub", "rust"]).is_ok());
        assert!(parse(&["x", "user", "@rustlang", "--limit", "5"]).is_ok());
        assert!(parse(&["web", "read", "--render", "https://example.com"]).is_ok());
        assert!(parse(&["browser", "login", "x"]).is_ok());
        assert!(parse(&["web", "read", "--via-jina", "https://example.com"]).is_ok());
        assert!(parse(&["web", "read", "--via-jina", "--render", "https://example.com"]).is_err(), "pick one way to fetch");
    }

    #[test]
    fn write_verbs_do_not_exist() {
        for args in [
            &["x", "post-tweet", "hi"][..],
            &["x", "tweet", "hi"],
            &["x", "like", "1"],
            &["x", "reply", "1", "hi"],
            &["x", "dm", "jack", "hi"],
            &["reddit", "comment", "1", "hi"],
            &["reddit", "vote", "1"],
            &["github", "pr-create"],
            &["github", "issue-create"],
            &["github", "repo-create", "x"],
            &["web", "post", "https://e.com"],
        ] {
            assert!(parse(args).is_err(), "{args:?} must not parse");
        }
    }

    #[test]
    fn values_that_look_like_flags_need_separator() {
        // An option-like value cannot sneak in as a flag; after `--` it is taken literally.
        assert!(parse(&["web", "read", "--exec=evil"]).is_err());
        assert!(
            matches!(parse(&["web", "read", "--", "--exec=evil"]).unwrap().cmd, Cmd::Web(WebCmd::Read { ref url, .. }) if url == "--exec=evil")
        );
    }
}
