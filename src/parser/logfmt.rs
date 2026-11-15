//! Small logfmt parser.
//!
//! A hand-written scanner is used instead of the `logfmt` crate so values keep
//! their type (`status=503` stays an integer for range queries) and so a line
//! is accepted only when the whole line is key=value pairs. The crate's
//! string-only API would force every facet through a second parse.

use crate::parser::ParsedEvent;
use crate::types::{FacetValue, Level};

pub fn parse_logfmt_line(line: &str) -> Option<ParsedEvent> {
    let pairs = parse_pairs(line)?;
    if pairs.is_empty() {
        return None;
    }
    let mut level = Level::Unknown;
    let mut ts_ns = 0i64;
    let mut message = String::new();
    let mut facets = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if is_level_key(&k) && level == Level::Unknown {
            level = level_of(&v).unwrap_or(Level::Unknown);
        }
        if is_time_key(&k) && ts_ns == 0 {
            if let Some(ns) = time_of(&v) {
                ts_ns = ns;
            }
        }
        if is_msg_key(&k) && message.is_empty() {
            message = v.display();
        }
        facets.push((k, v));
    }
    if message.is_empty() {
        message = line.to_string();
    }
    Some(ParsedEvent {
        level,
        ts_ns,
        message,
        facets,
        format: "logfmt",
    })
}

pub fn is_full_logfmt(line: &str) -> bool {
    parse_pairs(line).is_some()
}

/// Pull unquoted `key=value` tokens out of a plain-text line.
pub fn inline_pairs(line: &str) -> Vec<(String, FacetValue)> {
    let mut out = Vec::new();
    for token in line.split_whitespace() {
        let Some((k, v)) = token.split_once('=') else {
            continue;
        };
        if !is_ident(k) || v.is_empty() || v.contains('=') && v.matches('=').count() > 1 {
            continue;
        }
        // Skip tokens that look like a comparison in prose.
        if !v.chars().next().is_some_and(|c| {
            c.is_ascii_alphanumeric() || c == '"' || c == '\'' || c == '-' || c == '/'
        }) {
            continue;
        }
        out.push((k.to_string(), FacetValue::from_token(v)));
    }
    out
}

fn parse_pairs(line: &str) -> Option<Vec<(String, FacetValue)>> {
    let b = line.as_bytes();
    let mut i = 0;
    let mut pairs = Vec::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if !is_key_start(b[i]) {
            return None;
        }
        let key_start = i;
        i += 1;
        while i < b.len() && is_key_cont(b[i]) {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            return None;
        }
        let key = line[key_start..i].to_string();
        i += 1;
        let value = if i < b.len() && b[i] == b'"' {
            i += 1;
            let mut val = String::new();
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    val.push(b[i + 1] as char);
                    i += 2;
                } else {
                    val.push(b[i] as char);
                    i += 1;
                }
            }
            if i >= b.len() || b[i] != b'"' {
                return None;
            }
            i += 1;
            FacetValue::from_token(&val)
        } else {
            let vs = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            FacetValue::from_token(&line[vs..i])
        };
        pairs.push((key, value));
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs)
    }
}

fn is_key_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'@'
}

fn is_key_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@' | b'/')
}

fn is_ident(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn is_level_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    matches!(k.as_str(), "level" | "lvl" | "severity" | "priority")
}

fn is_time_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "time" | "timestamp" | "ts" | "@timestamp" | "datetime" | "date"
    )
}

fn is_msg_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    matches!(k.as_str(), "msg" | "message" | "log" | "text")
}

fn level_of(v: &FacetValue) -> Option<Level> {
    match v {
        FacetValue::Int(i) => Some(Level::from_number(*i)),
        FacetValue::Str(s) => {
            if let Ok(i) = s.parse::<i64>() {
                if (0..=7).contains(&i) || (10..=60).contains(&i) {
                    return Some(Level::from_number(i));
                }
            }
            Level::parse_name(s)
        }
        FacetValue::Float(f) => Some(Level::from_number(*f as i64)),
        FacetValue::Bool(_) => None,
    }
}

fn time_of(v: &FacetValue) -> Option<i64> {
    match v {
        FacetValue::Int(i) => crate::timeutil::epoch_to_ns(*i as f64),
        FacetValue::Float(f) => crate::timeutil::epoch_to_ns(*f),
        FacetValue::Str(s) => crate::timeutil::parse_time_str(s),
        FacetValue::Bool(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_message_and_numeric_status() {
        let ev = parse_logfmt_line(
            r#"level=error service=worker status=500 msg="timeout on upstream" duration=12ms"#,
        )
        .unwrap();
        assert_eq!(ev.level, Level::Error);
        assert_eq!(ev.message, "timeout on upstream");
        assert!(ev
            .facets
            .iter()
            .any(|(k, v)| k == "status" && matches!(v, FacetValue::Int(500))));
    }
}
