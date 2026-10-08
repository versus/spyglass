//! CLI ↔ daemon protocol: one JSON request line, then response lines
//! (zero or more `waiting` notices, then one final reply).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    Stop,
    Login { platform: String },
    Job { platform: String, verb: String, args: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// The user has to do something in the browser window; the job keeps waiting.
    Waiting {
        message: String,
    },
    Done {
        url: Option<String>,
        markdown: String,
        data: Value,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_round_trip_as_single_lines() {
        let job = Request::Job { platform: "reddit".into(), verb: "search".into(), args: json!({"query": "a\nb"}) };
        let line = serde_json::to_string(&job).unwrap();
        assert!(!line.contains('\n'), "newlines inside values must be escaped");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), job);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"ping"}"#).unwrap(), Request::Ping);
    }

    #[test]
    fn unknown_operations_and_fields_are_rejected() {
        assert!(serde_json::from_str::<Request>(r#"{"op":"eval","js":"alert(1)"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"login","platform":"x","url":"https://evil"}"#).is_err());
    }

    #[test]
    fn replies_are_tagged() {
        let r = Reply::Waiting { message: "log in".into() };
        assert_eq!(serde_json::to_value(&r).unwrap()["kind"], "waiting");
    }
}
