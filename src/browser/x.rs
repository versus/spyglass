//! X/Twitter through the agent browser. Requires the user to be logged in once
//! (`spyglass browser login x`); otherwise the job hands off to the user and waits.
//! Data is taken from the site's own GraphQL responses, not from the DOM.

use std::collections::HashSet;
use std::time::Duration;

use super::scenario::{Ctx, Found, Site, arg_str, arg_usize, capture_graphql, ensure_session};
use super::tab::Allow;
use crate::output::oneline;
use crate::validate;
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub const LOGIN_URL: &str = "https://x.com/i/flow/login";
pub const SITE: Site = Site { allow: Allow::Domains(&["x.com", "twitter.com", "twimg.com"]), scratch: false, visible: true };
const SESSION_COOKIE: (&str, &str) = ("https://x.com", "auth_token");
const LOGIN_WAIT: Duration = Duration::from_secs(300);
const DATA_WAIT: Duration = Duration::from_secs(25);

/// `x search|post|user` in the agent tab, after making sure the user is logged in.
pub async fn job(ctx: &Ctx<'_>, verb: &str, args: &Value) -> Result<Found> {
    let limit = arg_usize(args, "limit", 10);
    let (url, op) = match verb {
        "search" => (search_url(arg_str(args, "query")?, args["latest"].as_bool().unwrap_or(false))?, "SearchTimeline"),
        "post" => (format!("https://x.com/i/status/{}", status_id(arg_str(args, "post")?)?), "TweetDetail"),
        "user" => (format!("https://x.com/{}", validate::x_handle(arg_str(args, "handle")?)?), "UserTweets"),
        _ => bail!("unknown x command {verb}"),
    };
    ensure_session(ctx, "X", SESSION_COOKIE, LOGIN_URL, LOGIN_WAIT).await?;
    let body = capture_graphql(ctx.tab, &url, op, DATA_WAIT).await?;
    let tweets = extract_tweets(&serde_json::from_str(&body)?);
    let shown: Vec<&Tweet> = tweets.iter().take(limit).collect();
    Ok(Found { url: Some(url), markdown: render(&tweets, limit), data: json!(shown) })
}

pub fn search_url(query: &str, latest: bool) -> Result<String> {
    let q = validate::enc(&validate::query(query)?);
    Ok(format!("https://x.com/search?q={q}&src=typed_query{}", if latest { "&f=live" } else { "" }))
}

fn is_id(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

pub fn status_id(input: &str) -> Result<String> {
    let input = input.trim();
    if is_id(input) {
        return Ok(input.to_string());
    }
    let url = validate::url_on(input, &["x.com", "twitter.com"])?;
    let segs: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    match segs.windows(2).find(|w| w[0] == "status" && is_id(w[1])) {
        Some(w) => Ok(w[1].to_string()),
        None => bail!("not a post URL"),
    }
}

/// A tweet flattened from X's GraphQL shapes.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Tweet {
    pub id: String,
    pub author: String,
    pub name: String,
    pub text: String,
    pub created: String,
    pub likes: u64,
    pub retweets: u64,
    pub replies: u64,
    pub views: Option<String>,
    pub reply_to: Option<String>,
}

/// All tweets in a GraphQL response, in document order, de-duplicated.
pub fn extract_tweets(v: &Value) -> Vec<Tweet> {
    let mut out = Vec::new();
    walk(v, &mut out, &mut HashSet::new());
    out
}

fn walk(v: &Value, out: &mut Vec<Tweet>, seen: &mut HashSet<String>) {
    match v {
        Value::Object(map) if map.get("__typename").and_then(Value::as_str) == Some("Tweet") => {
            // A tweet's own subtree only holds quoted/retweeted tweets: not part of the list.
            if let Some(t) = parse_tweet(v).filter(|t| seen.insert(t.id.clone())) {
                out.push(t);
            }
        }
        Value::Object(map) => map.values().for_each(|c| walk(c, out, seen)),
        Value::Array(items) => items.iter().for_each(|c| walk(c, out, seen)),
        _ => {}
    }
}

fn parse_tweet(t: &Value) -> Option<Tweet> {
    let legacy = &t["legacy"];
    let user = &t["core"]["user_results"]["result"];
    let pick = |k: &str| user["core"][k].as_str().or(user["legacy"][k].as_str()).unwrap_or("").to_string();
    let text = t["note_tweet"]["note_tweet_results"]["result"]["text"].as_str().or(legacy["full_text"].as_str())?;
    Some(Tweet {
        id: t["rest_id"].as_str()?.to_string(),
        author: pick("screen_name"),
        name: pick("name"),
        text: text.to_string(),
        created: twitter_date(legacy["created_at"].as_str().unwrap_or("")),
        likes: legacy["favorite_count"].as_u64().unwrap_or(0),
        retweets: legacy["retweet_count"].as_u64().unwrap_or(0),
        replies: legacy["reply_count"].as_u64().unwrap_or(0),
        views: t["views"]["count"].as_str().map(str::to_string),
        reply_to: legacy["in_reply_to_status_id_str"].as_str().map(str::to_string),
    })
}

/// "Wed Oct 01 12:00:00 +0000 2026" → "2026-10-01".
fn twitter_date(s: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let p: Vec<&str> = s.split_whitespace().collect();
    match (p.get(1).and_then(|m| MONTHS.iter().position(|x| x == m)), p.get(2), p.get(5)) {
        (Some(m), Some(d), Some(y)) => format!("{y}-{:02}-{d:0>2}", m + 1),
        _ => String::new(),
    }
}

pub fn render(tweets: &[Tweet], limit: usize) -> String {
    let mut md = String::new();
    for t in tweets.iter().take(limit) {
        let views = t.views.as_ref().map(|v| format!(" 👁{v}")).unwrap_or_default();
        let reply = if t.reply_to.is_some() { " · reply" } else { "" };
        md.push_str(&format!(
            "- **@{}** ({}) · {}{reply} · ♥{} ⟲{} 💬{}{views}\n  {}\n  https://x.com/{}/status/{}\n",
            t.author,
            t.name,
            t.created,
            t.likes,
            t.retweets,
            t.replies,
            oneline(&t.text, 1000),
            t.author,
            t.id
        ));
    }
    if md.is_empty() { "No posts.".into() } else { md }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tweet_new_shape(id: &str, text: &str) -> Value {
        json!({"__typename":"Tweet","rest_id":id,
            "core":{"user_results":{"result":{"__typename":"User","core":{"screen_name":"rustlang","name":"Rust Language"},"legacy":{}}}},
            "views":{"count":"12345"},
            "legacy":{"full_text":text,"created_at":"Wed Oct 01 12:00:00 +0000 2026","favorite_count":10,"retweet_count":2,"reply_count":3}})
    }

    #[test]
    fn urls_and_ids() {
        assert_eq!(search_url("rust 1.99", false).unwrap(), "https://x.com/search?q=rust+1.99&src=typed_query");
        assert_eq!(search_url("rust", true).unwrap(), "https://x.com/search?q=rust&src=typed_query&f=live");
        assert_eq!(status_id("1973400000000000000").unwrap(), "1973400000000000000");
        assert_eq!(status_id("https://x.com/rustlang/status/1973400000000000000?s=20").unwrap(), "1973400000000000000");
        assert_eq!(status_id("https://twitter.com/a/status/123").unwrap(), "123");
        for bad in ["abc", "https://evil.com/a/status/1", "https://x.com/rustlang", "1; DROP"] {
            assert!(status_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn extracts_tweets_from_nested_timeline_in_both_user_shapes() {
        let legacy_user = json!({"__typename":"TweetWithVisibilityResults","tweet":{"__typename":"Tweet","rest_id":"2",
            "core":{"user_results":{"result":{"legacy":{"screen_name":"old_style","name":"Old"}}}},
            "note_tweet":{"note_tweet_results":{"result":{"text":"Long note text beyond 280 chars"}}},
            "legacy":{"full_text":"truncated…","created_at":"Thu Oct 02 08:00:00 +0000 2026","favorite_count":1,"retweet_count":0,"reply_count":0,
                      "in_reply_to_status_id_str":"1"}}});
        let response = json!({"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"entries":[
            {"content":{"itemContent":{"tweet_results":{"result": tweet_new_shape("1", "Rust 1.99 is out!")}}}},
            {"content":{"itemContent":{"tweet_results":{"result": legacy_user}}}},
            {"content":{"itemContent":{"tweet_results":{"result": tweet_new_shape("1", "duplicate")}}}}
        ]}]}}}}});
        let tweets = extract_tweets(&response);
        assert_eq!(tweets.len(), 2);
        assert_eq!(tweets[0].author, "rustlang");
        assert_eq!(tweets[0].text, "Rust 1.99 is out!");
        assert_eq!(tweets[0].likes, 10);
        assert_eq!(tweets[0].views.as_deref(), Some("12345"));
        assert_eq!(tweets[1].author, "old_style");
        assert_eq!(tweets[1].text, "Long note text beyond 280 chars", "note_tweet wins over truncated text");
        assert_eq!(tweets[1].reply_to.as_deref(), Some("1"));
    }

    #[test]
    fn quoted_tweets_are_not_flattened_into_the_list() {
        let mut t = tweet_new_shape("1", "look at this");
        t["quoted_status_result"] = json!({"result": tweet_new_shape("99", "quoted")});
        let tweets = extract_tweets(&json!({"data": {"tweet": t}}));
        assert_eq!(tweets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["1"]);
    }

    #[test]
    fn renders_compact_list() {
        let tweets = extract_tweets(&json!([tweet_new_shape("1", "Rust 1.99\nis out!")]));
        let md = render(&tweets, 10);
        assert!(md.contains("**@rustlang** (Rust Language) · 2026-10-01"), "{md}");
        assert!(md.contains("Rust 1.99 is out!"));
        assert!(md.contains("♥10 ⟲2 💬3 👁12345"));
        assert!(md.contains("https://x.com/rustlang/status/1"));
        assert_eq!(render(&[], 10), "No posts.");
    }
}
