//! `spyglass youtube …` — metadata, transcripts and search through the system `yt-dlp`.
//! Never downloads media; the user's yt-dlp config is ignored (it could contain `--exec`).

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::net::Net;
use crate::output::{Doc, oneline, strip_tags, unescape};
use crate::{tools, validate};

const DOMAINS: &[&str] = &["youtube.com", "youtu.be"];
const BASE_ARGS: &[&str] = &["--ignore-config", "--no-cache-dir", "--no-warnings", "--skip-download", "--no-playlist"];
const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_JSON: usize = 20 * 1024 * 1024;

/// Accept a YouTube URL or a bare 11-character video id.
pub fn video_url(input: &str) -> Result<String> {
    let id = input.trim();
    if id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Ok(format!("https://www.youtube.com/watch?v={id}"));
    }
    Ok(validate::url_on(id, DOMAINS)?.to_string())
}

async fn metadata(url: &str) -> Result<Value> {
    let mut args = BASE_ARGS.to_vec();
    args.extend(["--dump-single-json", "--", url]);
    Ok(serde_json::from_str(&tools::run("yt-dlp", &args, TIMEOUT, MAX_JSON).await?)?)
}

pub async fn video(input: &str) -> Result<Doc> {
    let url = video_url(input)?;
    let v = metadata(&url).await?;
    Ok(Doc::new("youtube", Some(url), render_video(&v), slim(&v)))
}

pub async fn transcript(net: &Net, input: &str, langs: &[String]) -> Result<Doc> {
    let url = video_url(input)?;
    let v = metadata(&url).await?;
    // Default: the video's own language, then English.
    let langs: Vec<String> = if langs.is_empty() {
        v["language"].as_str().into_iter().map(str::to_string).chain(["en".to_string()]).collect()
    } else {
        langs.to_vec()
    };
    let Some(track) = pick_track(&v, &langs) else {
        bail!("no subtitles in {:?} for this video (available: {})", langs, available_langs(&v));
    };
    let raw = net.get(&track.url, &[], 10 * 1024 * 1024).await?.body;
    let lines = if track.ext == "json3" { parse_json3(&raw)? } else { parse_vtt(&raw) };
    let text = paragraphs(&lines);
    let title = v["title"].as_str().unwrap_or("");
    let kind = if track.auto { "auto-generated" } else { "uploaded" };
    let md = format!("# {title}\n\nTranscript ({}, {kind})\n\n{text}", track.lang);
    Ok(Doc::new("youtube", Some(url), md, json!({ "title": title, "lang": track.lang, "auto": track.auto, "transcript": text })))
}

pub async fn search(query: &str, limit: usize) -> Result<Doc> {
    let q = validate::query(query)?;
    let target = format!("ytsearch{limit}:{q}");
    let args = ["--ignore-config", "--no-cache-dir", "--no-warnings", "--flat-playlist", "--dump-single-json", "--", target.as_str()];
    let v: Value = serde_json::from_str(&tools::run("yt-dlp", &args, TIMEOUT, MAX_JSON).await?)?;
    Ok(Doc::new("youtube", None, render_search(&v), v["entries"].clone()))
}

#[derive(Debug, PartialEq)]
pub struct Track {
    pub lang: String,
    pub ext: String,
    pub url: String,
    pub auto: bool,
}

/// Prefer uploaded subtitles over automatic captions, then the caller's language order,
/// then json3 over vtt. HLS playlists are skipped.
pub fn pick_track(v: &Value, langs: &[String]) -> Option<Track> {
    for (key, auto) in [("subtitles", false), ("automatic_captions", true)] {
        let Some(map) = v[key].as_object() else { continue };
        for lang in langs {
            let prefix = format!("{lang}-");
            let mut keys: Vec<&String> = map.keys().filter(|k| *k == lang || k.starts_with(&prefix)).collect();
            keys.sort_by_key(|k| *k != lang); // exact match first
            for k in keys {
                let tracks = map[k].as_array().into_iter().flatten().filter(|t| !t["url"].as_str().unwrap_or("").contains("/manifest/"));
                let best = tracks
                    .filter(|t| matches!(t["ext"].as_str(), Some("json3" | "vtt")))
                    .min_by_key(|t| t["ext"].as_str() != Some("json3"));
                if let Some(t) = best {
                    return Some(Track {
                        lang: k.clone(),
                        ext: t["ext"].as_str().unwrap_or("").to_string(),
                        url: t["url"].as_str().unwrap_or("").to_string(),
                        auto,
                    });
                }
            }
        }
    }
    None
}

fn available_langs(v: &Value) -> String {
    let mut langs: Vec<&str> =
        ["subtitles", "automatic_captions"].iter().filter_map(|k| v[k].as_object()).flat_map(|m| m.keys().map(String::as_str)).collect();
    langs.sort_unstable();
    langs.dedup();
    if langs.len() > 20 {
        langs.truncate(20);
    }
    if langs.is_empty() { "none".into() } else { langs.join(", ") }
}

/// (start_ms, text) pairs from YouTube's json3 caption format.
pub fn parse_json3(raw: &str) -> Result<Vec<(u64, String)>> {
    let v: Value = serde_json::from_str(raw)?;
    let lines = v["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let text: String = e["segs"].as_array()?.iter().filter_map(|s| s["utf8"].as_str()).collect();
            let text = oneline(&text, usize::MAX);
            (!text.is_empty()).then(|| (e["tStartMs"].as_u64().unwrap_or(0), text))
        })
        .collect();
    Ok(lines)
}

/// (start_ms, text) pairs from WebVTT; inline tags removed, rolling duplicates dropped.
pub fn parse_vtt(raw: &str) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for block in raw.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines();
        let Some(start) = lines.by_ref().find(|l| l.contains("-->")).and_then(|l| vtt_ms(l.split("-->").next()?.trim())) else {
            continue;
        };
        for line in lines {
            let text = oneline(&unescape(&strip_tags(line)), usize::MAX);
            let recent = out.iter().rev().take(3).any(|(_, t)| *t == text);
            if !text.is_empty() && !recent {
                out.push((start, text));
            }
        }
    }
    out
}

fn vtt_ms(ts: &str) -> Option<u64> {
    let (hms, ms) = ts.split_once('.')?;
    let parts: Vec<u64> = hms.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let secs = parts.iter().fold(0, |acc, p| acc * 60 + p);
    Some(secs * 1000 + ms.get(..3)?.parse::<u64>().ok()?)
}

pub fn paragraphs(lines: &[(u64, String)]) -> String {
    let mut paras: Vec<String> = Vec::new();
    let mut para_start: Option<u64> = None;
    for (ms, text) in lines {
        match para_start {
            Some(start) if ms.saturating_sub(start) < 30_000 => {
                let last = paras.last_mut().expect("paragraph exists");
                last.push(' ');
                last.push_str(text);
            }
            _ => {
                para_start = Some(*ms);
                paras.push(format!("[{}] {text}", mmss(ms / 1000)));
            }
        }
    }
    paras.join("\n\n")
}

/// Minutes may exceed 59 (e.g. 61:40) — compact and unambiguous for transcripts.
fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn duration(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

pub fn render_video(v: &Value) -> String {
    let date =
        v["upload_date"].as_str().filter(|d| d.len() == 8).map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..])).unwrap_or_default();
    let mut md = format!(
        "# {}\n\n{} · {} · {} · {} views · {} likes\n",
        v["title"].as_str().unwrap_or(""),
        v["channel"].as_str().or(v["uploader"].as_str()).unwrap_or(""),
        date,
        duration(v["duration"].as_f64().unwrap_or(0.0)),
        v["view_count"],
        v["like_count"],
    );
    if let Some(chapters) = v["chapters"].as_array().filter(|c| !c.is_empty()) {
        md.push_str("\n## Chapters\n");
        for c in chapters {
            md.push_str(&format!("- [{}] {}\n", mmss(c["start_time"].as_f64().unwrap_or(0.0) as u64), c["title"].as_str().unwrap_or("")));
        }
    }
    md.push_str(&format!("\n## Description\n\n{}", v["description"].as_str().unwrap_or("").trim()));
    md
}

pub fn render_search(v: &Value) -> String {
    let mut md = String::new();
    for e in v["entries"].as_array().into_iter().flatten() {
        md.push_str(&format!(
            "- [{}]({}) — {} · {} · {} views\n",
            oneline(e["title"].as_str().unwrap_or(""), 160),
            e["url"].as_str().unwrap_or(""),
            e["channel"].as_str().unwrap_or(""),
            duration(e["duration"].as_f64().unwrap_or(0.0)),
            e["view_count"],
        ));
    }
    if md.is_empty() { "No results.".into() } else { md }
}

/// Keep only useful metadata for --json (the raw dump is ~100 KB of formats).
fn slim(v: &Value) -> Value {
    let keys = ["id", "title", "channel", "upload_date", "duration", "view_count", "like_count", "description", "chapters", "webpage_url"];
    Value::Object(keys.iter().filter_map(|k| v.get(*k).map(|x| (k.to_string(), x.clone()))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn video_url_rules() {
        assert_eq!(video_url("jNQXAC9IVRw").unwrap(), "https://www.youtube.com/watch?v=jNQXAC9IVRw");
        assert_eq!(video_url("https://youtu.be/jNQXAC9IVRw").unwrap(), "https://youtu.be/jNQXAC9IVRw");
        assert!(video_url("https://www.youtube.com/watch?v=jNQXAC9IVRw&t=5").is_ok());
        for bad in ["--exec=x", "short", "https://evil.com/watch?v=jNQXAC9IVRw", "ytsearch5:x", "file:///etc/passwd"] {
            assert!(video_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn picks_uploaded_json3_in_preferred_language() {
        let v = json!({
            "subtitles": {"de": [{"ext":"json3","url":"https://www.youtube.com/de.j3"}],
                          "en": [{"ext":"vtt","url":"https://www.youtube.com/en.vtt"},{"ext":"json3","url":"https://www.youtube.com/en.j3"}]},
            "automatic_captions": {"ru": [{"ext":"json3","url":"https://www.youtube.com/ru.j3"}]}
        });
        let t = pick_track(&v, &langs(&["en", "de"])).unwrap();
        assert_eq!((t.lang.as_str(), t.ext.as_str(), t.auto), ("en", "json3", false));
        let t = pick_track(&v, &langs(&["ru"])).unwrap();
        assert_eq!((t.lang.as_str(), t.auto), ("ru", true));
        assert!(pick_track(&v, &langs(&["fr"])).is_none());
    }

    #[test]
    fn auto_caption_variants_match_language_prefix_and_skip_hls() {
        let v = json!({"automatic_captions": {"en-orig": [
            {"ext":"vtt","url":"https://manifest.googlevideo.com/api/manifest/hls_timedtext_playlist/x"},
            {"ext":"vtt","url":"https://www.youtube.com/api/timedtext?v=x&fmt=vtt"}]}});
        let t = pick_track(&v, &langs(&["en"])).unwrap();
        assert_eq!(t.url, "https://www.youtube.com/api/timedtext?v=x&fmt=vtt");
    }

    #[test]
    fn parses_json3() {
        let raw = r#"{"events":[{"tStartMs":1200,"segs":[{"utf8":"All right, so here\nwe are"}]},
            {"tStartMs":5000},{"tStartMs":7000,"segs":[{"utf8":"really "},{"utf8":"long trunks"}]},{"tStartMs":8000,"segs":[{"utf8":"\n"}]}]}"#;
        assert_eq!(
            parse_json3(raw).unwrap(),
            vec![(1200, "All right, so here we are".to_string()), (7000, "really long trunks".to_string())]
        );
    }

    #[test]
    fn parses_vtt_dropping_tags_and_rolling_duplicates() {
        let raw = "WEBVTT\nKind: captions\nLanguage: en\n\n00:00:01.000 --> 00:00:03.000 align:start\nhello <c>world</c>\n\n00:00:03.000 --> 00:00:05.000\nhello world\nnext line\n\n01:02:03.500 --> 01:02:04.000\n&gt; quoted &amp; done\n";
        assert_eq!(parse_vtt(raw), vec![(1000, "hello world".into()), (3000, "next line".into()), (3_723_500, "> quoted & done".into())]);
    }

    #[test]
    fn groups_paragraphs_with_timestamps() {
        let lines = vec![(0, "a".to_string()), (10_000, "b".into()), (31_000, "c".into()), (3_700_000, "d".into())];
        assert_eq!(paragraphs(&lines), "[00:00] a b\n\n[00:31] c\n\n[61:40] d");
    }

    #[test]
    fn renders_video_and_search() {
        let v = json!({"title":"Me at the zoo","channel":"jawed","upload_date":"20050424","duration":19,
            "view_count":441282333,"like_count":20018862,"description":"The first video\non YouTube",
            "chapters":[{"start_time":0.0,"title":"Intro"}]});
        let md = render_video(&v);
        assert!(md.starts_with("# Me at the zoo"));
        for part in ["jawed", "2005-04-24", "0:19", "441282333", "The first video", "[00:00] Intro"] {
            assert!(md.contains(part), "missing {part}: {md}");
        }
        let s = json!({"entries":[{"title":"Rust in 100 seconds","url":"https://www.youtube.com/watch?v=5C_HPTJg5ek","channel":"Fireship","duration":149.0,"view_count":2000000}]});
        let md = render_search(&s);
        assert!(
            md.contains("[Rust in 100 seconds](https://www.youtube.com/watch?v=5C_HPTJg5ek)")
                && md.contains("Fireship")
                && md.contains("2:29")
        );
    }
}
