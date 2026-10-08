//! `spyglass github …` — GitHub REST API directly (no `gh` CLI). Read-only.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::net::Net;
use crate::output::{Doc, oneline, str_at as s};
use crate::secrets::{self, Secret};
use crate::validate::{self, enc};

const API: &str = "https://api.github.com";
const MAX_FILE_BYTES: usize = 1024 * 1024;

pub struct GitHub<'a> {
    net: &'a Net,
    token: Option<String>,
}

impl<'a> GitHub<'a> {
    pub fn new(net: &'a Net) -> Self {
        Self { net, token: secrets::get(Secret::GithubToken) }
    }

    fn headers(&self, accept: &'static str) -> Vec<(&'static str, String)> {
        let mut h = vec![("Accept", accept.to_string()), ("X-GitHub-Api-Version", "2022-11-28".to_string())];
        if let Some(t) = &self.token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        h
    }

    async fn get_raw(&self, path: &str, accept: &'static str, max: usize) -> Result<String> {
        let headers = self.headers(accept);
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Ok(self.net.get(&format!("{API}{path}"), &refs, max).await?.body)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let body = self.get_raw(path, "application/vnd.github+json", crate::net::DEFAULT_MAX_BYTES).await?;
        Ok(serde_json::from_str(&body)?)
    }

    pub async fn repo(&self, repo: &str) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let v = self.get(&format!("/repos/{o}/{r}")).await?;
        Ok(doc(&o, &r, render_repo(&v), v))
    }

    pub async fn readme(&self, repo: &str) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let text = self.get_raw(&format!("/repos/{o}/{r}/readme"), "application/vnd.github.raw+json", MAX_FILE_BYTES).await?;
        Ok(doc(&o, &r, text.clone(), json!({ "readme": text })))
    }

    pub async fn file(&self, repo: &str, path: &str, git_ref: Option<&str>) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let path = validate::repo_path(path)?;
        let mut api = url::Url::parse(&format!("{API}/repos/{o}/{r}/contents/"))?;
        api.path_segments_mut().map_err(|_| anyhow::anyhow!("bad URL"))?.pop_if_empty().extend(path.split('/'));
        if let Some(rf) = git_ref {
            api.query_pairs_mut().append_pair("ref", &validate::repo_path(rf)?);
        }
        let rel = &api.as_str()[API.len()..];
        let text = self.get_raw(rel, "application/vnd.github.raw+json", MAX_FILE_BYTES).await?;
        Ok(doc(&o, &r, format!("`{path}`\n\n```\n{text}\n```"), json!({ "path": path, "content": text })))
    }

    pub async fn search_repos(&self, query: &str, limit: usize) -> Result<Doc> {
        let q = validate::query(query)?;
        let v = self.get(&format!("/search/repositories?q={}&per_page={limit}", enc(&q))).await?;
        let md = render_search_repos(&v);
        Ok(Doc::new("github", Some(format!("https://github.com/search?q={}&type=repositories", enc(&q))), md, v))
    }

    pub async fn search_code(&self, query: &str, limit: usize) -> Result<Doc> {
        if self.token.is_none() {
            bail!("GitHub code search requires a token: run `spyglass secrets set github-token`");
        }
        let q = validate::query(query)?;
        let v = self.get(&format!("/search/code?q={}&per_page={limit}", enc(&q))).await?;
        let md = render_search_code(&v);
        Ok(Doc::new("github", Some(format!("https://github.com/search?q={}&type=code", enc(&q))), md, v))
    }

    /// Issues (`prs == false`) or pull requests (`prs == true`) of a repository.
    pub async fn list(&self, repo: &str, prs: bool, state: &str, limit: usize) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let v = self.get(&list_path(&o, &r, prs, state, limit)?).await?;
        Ok(doc(&o, &r, render_list(&v, prs, limit), v))
    }

    /// One issue or PR with its comments.
    pub async fn thread(&self, repo: &str, number: u64, prs: bool, comments: usize) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let kind = if prs { "pulls" } else { "issues" };
        let item = self.get(&format!("/repos/{o}/{r}/{kind}/{number}")).await?;
        let list = self.get(&format!("/repos/{o}/{r}/issues/{number}/comments?per_page={comments}")).await?;
        let md = render_thread(&item, list.as_array().map(Vec::as_slice).unwrap_or_default());
        Ok(doc(&o, &r, md, json!({ "item": item, "comments": list })))
    }

    pub async fn releases(&self, repo: &str, limit: usize) -> Result<Doc> {
        let (o, r) = validate::github_repo(repo)?;
        let v = self.get(&format!("/repos/{o}/{r}/releases?per_page={limit}")).await?;
        Ok(doc(&o, &r, render_releases(&v), v))
    }
}

fn doc(owner: &str, repo: &str, markdown: String, data: Value) -> Doc {
    Doc::new("github", Some(format!("https://github.com/{owner}/{repo}")), markdown, data)
}

fn date<'v>(v: &'v Value, key: &str) -> &'v str {
    s(v, key).get(..10).unwrap_or("")
}

/// API path listing a repository's issues or pull requests, newest first.
/// Issues go through search: the `/issues` endpoint mixes in PRs, so busy repos return too few issues.
pub fn list_path(owner: &str, repo: &str, prs: bool, state: &str, limit: usize) -> Result<String> {
    if !matches!(state, "open" | "closed" | "all") {
        bail!("state must be open, closed or all");
    }
    if prs {
        return Ok(format!("/repos/{owner}/{repo}/pulls?state={state}&sort=created&direction=desc&per_page={limit}"));
    }
    let state_filter = if state == "all" { String::new() } else { format!(" state:{state}") };
    let q = enc(&format!("repo:{owner}/{repo} is:issue{state_filter}"));
    Ok(format!("/search/issues?q={q}&sort=created&order=desc&per_page={limit}"))
}

pub fn render_repo(v: &Value) -> String {
    let topics: Vec<&str> = v["topics"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut md = format!("# {}\n\n{}\n\n", s(v, "full_name"), s(v, "description"));
    md.push_str(&format!(
        "★{} · forks {} · open issues {} · {} · license {} · default branch `{}` · last push {}",
        v["stargazers_count"],
        v["forks_count"],
        v["open_issues_count"],
        s(v, "language"),
        v["license"]["spdx_id"].as_str().unwrap_or("none"),
        s(v, "default_branch"),
        date(v, "pushed_at"),
    ));
    if !topics.is_empty() {
        md.push_str(&format!("\nTopics: {}", topics.join(", ")));
    }
    if v["archived"].as_bool() == Some(true) {
        md.push_str("\n**Archived**");
    }
    md
}

pub fn render_search_repos(v: &Value) -> String {
    let mut md = format!("{} repositories found\n", v["total_count"]);
    for it in v["items"].as_array().into_iter().flatten() {
        md.push_str(&format!(
            "\n- [{}]({}) ★{} {} — {}",
            s(it, "full_name"),
            s(it, "html_url"),
            it["stargazers_count"],
            s(it, "language"),
            oneline(s(it, "description"), 160)
        ));
    }
    md
}

pub fn render_search_code(v: &Value) -> String {
    let mut md = format!("{} code results\n", v["total_count"]);
    for it in v["items"].as_array().into_iter().flatten() {
        md.push_str(&format!("\n- {}: {} — {}", s(&it["repository"], "full_name"), s(it, "path"), s(it, "html_url")));
    }
    md
}

pub fn render_list(v: &Value, prs: bool, limit: usize) -> String {
    // Plain arrays come from /pulls; search results wrap them in `items`.
    let list = v.as_array().or_else(|| v["items"].as_array());
    let items = list.into_iter().flatten().filter(|it| prs || it.get("pull_request").is_none()).take(limit);
    let mut md = String::new();
    for it in items {
        md.push_str(&format!(
            "- #{} {} [{}] by @{} · {} · {} comments\n",
            it["number"],
            oneline(s(it, "title"), 160),
            s(it, "state"),
            s(&it["user"], "login"),
            date(it, "created_at"),
            it["comments"].as_u64().unwrap_or(0)
        ));
    }
    if md.is_empty() { "No results.".into() } else { md }
}

pub fn render_thread(item: &Value, comments: &[Value]) -> String {
    let mut md = format!(
        "# #{} {}\n\n[{}] by @{} · {}\n\n{}\n",
        item["number"],
        s(item, "title"),
        s(item, "state"),
        s(&item["user"], "login"),
        date(item, "created_at"),
        s(item, "body").trim()
    );
    for c in comments {
        md.push_str(&format!("\n---\n**@{}** · {}\n\n{}\n", s(&c["user"], "login"), date(c, "created_at"), s(c, "body").trim()));
    }
    md
}

pub fn render_releases(v: &Value) -> String {
    let mut md = String::new();
    for r in v.as_array().into_iter().flatten() {
        let pre = if r["prerelease"].as_bool() == Some(true) { " (pre-release)" } else { "" };
        md.push_str(&format!(
            "## {} — {}{} · {}\n\n{}\n\n",
            s(r, "tag_name"),
            s(r, "name"),
            pre,
            date(r, "published_at"),
            s(r, "body").trim()
        ));
    }
    if md.is_empty() { "No releases.".into() } else { md }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_are_listed_via_search_and_prs_via_pulls() {
        assert_eq!(
            list_path("tokio-rs", "tokio", false, "open", 5).unwrap(),
            "/search/issues?q=repo%3Atokio-rs%2Ftokio+is%3Aissue+state%3Aopen&sort=created&order=desc&per_page=5"
        );
        assert_eq!(
            list_path("tokio-rs", "tokio", false, "all", 5).unwrap(),
            "/search/issues?q=repo%3Atokio-rs%2Ftokio+is%3Aissue&sort=created&order=desc&per_page=5"
        );
        assert_eq!(list_path("a", "b", true, "closed", 3).unwrap(), "/repos/a/b/pulls?state=closed&sort=created&direction=desc&per_page=3");
        assert!(list_path("a", "b", false, "merged; DROP", 3).is_err());
    }

    #[test]
    fn renders_search_items_as_issue_list() {
        let search = json!({"total_count": 1, "items": [{"number": 9, "title": "Bug", "state": "closed", "user": {"login": "ann"}, "created_at": "2026-10-08T00:00:00Z", "comments": 1}]});
        assert!(render_list(&search, false, 10).contains("#9 Bug [closed] by @ann"));
    }

    #[test]
    fn repo_summary() {
        let v = json!({"full_name":"tokio-rs/tokio","description":"Async runtime","stargazers_count":30000,
            "forks_count":2500,"open_issues_count":300,"language":"Rust","license":{"spdx_id":"MIT"},
            "topics":["async","rust"],"default_branch":"master","pushed_at":"2026-10-01T10:00:00Z",
            "html_url":"https://github.com/tokio-rs/tokio","archived":false});
        let md = render_repo(&v);
        assert!(md.starts_with("# tokio-rs/tokio"));
        for part in ["Async runtime", "30000", "Rust", "MIT", "async, rust", "2026-10-01"] {
            assert!(md.contains(part), "missing {part} in {md}");
        }
    }

    #[test]
    fn issue_list_skips_pull_requests_and_respects_limit() {
        let v = json!([
            {"number":1,"title":"Bug A","state":"open","user":{"login":"ann"},"comments":2,"created_at":"2026-09-01T00:00:00Z"},
            {"number":2,"title":"PR B","state":"open","user":{"login":"bob"},"pull_request":{},"created_at":"2026-09-02T00:00:00Z"},
            {"number":3,"title":"Bug C","state":"closed","user":{"login":"cid"},"comments":0,"created_at":"2026-09-03T00:00:00Z"},
            {"number":4,"title":"Bug D","state":"open","user":{"login":"dan"},"comments":0,"created_at":"2026-09-04T00:00:00Z"}
        ]);
        let md = render_list(&v, false, 2);
        assert!(md.contains("#1 Bug A") && md.contains("#3 Bug C"));
        assert!(!md.contains("PR B") && !md.contains("Bug D"));
        assert!(md.contains("@ann"));
        let prs = render_list(&v, true, 10);
        assert!(prs.contains("PR B"));
    }

    #[test]
    fn thread_with_comments() {
        let item = json!({"number":7,"title":"Crash on start","state":"open","user":{"login":"ann"},
            "body":"Steps to reproduce","created_at":"2026-09-01T00:00:00Z","html_url":"https://github.com/o/r/issues/7"});
        let comments = vec![json!({"user":{"login":"bob"},"body":"Same here","created_at":"2026-09-02T00:00:00Z"})];
        let md = render_thread(&item, &comments);
        assert!(md.starts_with("# #7 Crash on start"));
        assert!(md.contains("Steps to reproduce"));
        assert!(md.contains("@bob") && md.contains("Same here"));
    }

    #[test]
    fn search_and_releases() {
        let v = json!({"total_count":2,"items":[{"full_name":"a/b","description":"desc","stargazers_count":5,"language":"Go","html_url":"https://github.com/a/b"}]});
        let md = render_search_repos(&v);
        assert!(md.contains("[a/b](https://github.com/a/b)") && md.contains("★5"));
        let code = json!({"total_count":1,"items":[{"path":"src/x.rs","repository":{"full_name":"a/b"},"html_url":"https://github.com/a/b/blob/x"}]});
        assert!(render_search_code(&code).contains("a/b: src/x.rs"));
        let rel = json!([{"tag_name":"v1.2.0","name":"Big release","published_at":"2026-08-01T00:00:00Z","body":"Changes here","prerelease":false}]);
        let md = render_releases(&rel);
        assert!(md.contains("v1.2.0") && md.contains("2026-08-01") && md.contains("Changes here"));
    }
}
