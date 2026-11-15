//! Nginx and Apache combined/common access logs.

use crate::parser::ParsedEvent;
use crate::types::{FacetValue, Level};
use regex::Regex;
use std::sync::OnceLock;

fn access_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^(\S+) \S+ \S+ \[([^\]]+)\] "([A-Z]+) (\S+) [^"]*" (\d{3}) (\S+)(?: "([^"]*)" "([^"]*)")?$"#,
        )
        .expect("access log regex")
    })
}

pub fn parse_access(line: &str) -> Option<ParsedEvent> {
    let caps = access_re().captures(line)?;
    let ip = caps.get(1)?.as_str();
    let ts = crate::timeutil::parse_time_str(caps.get(2)?.as_str()).unwrap_or(0);
    let method = caps.get(3)?.as_str();
    let path = caps.get(4)?.as_str();
    let status: i64 = caps.get(5)?.as_str().parse().ok()?;
    let bytes = caps.get(6)?.as_str();
    let referer = caps.get(7).map(|m| m.as_str()).unwrap_or("-");
    let ua = caps.get(8).map(|m| m.as_str()).unwrap_or("-");
    let level = if status >= 500 {
        Level::Error
    } else if status >= 400 {
        Level::Warn
    } else {
        Level::Info
    };
    let message = format!("{method} {path} {status}");
    let mut facets = vec![
        ("client".into(), FacetValue::Str(ip.to_string())),
        ("method".into(), FacetValue::Str(method.to_string())),
        ("path".into(), FacetValue::Str(path.to_string())),
        ("status".into(), FacetValue::Int(status)),
        ("bytes".into(), FacetValue::from_token(bytes)),
    ];
    if referer != "-" {
        facets.push(("referer".into(), FacetValue::Str(referer.to_string())));
    }
    if ua != "-" {
        facets.push(("user_agent".into(), FacetValue::Str(ua.to_string())));
    }
    Some(ParsedEvent {
        level,
        ts_ns: ts,
        message,
        facets,
        format: "access",
    })
}
