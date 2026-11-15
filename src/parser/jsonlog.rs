//! JSON / NDJSON objects: pino, bunyan, zap, slog, winston, structlog, journald.

use crate::parser::ParsedEvent;
use crate::types::{FacetValue, Level};
use serde_json::Value;

const LEVEL_KEYS: &[&str] = &[
    "level",
    "severity",
    "lvl",
    "log.level",
    "priority",
    "PRIORITY",
    "severity_text",
];
const TIME_KEYS: &[&str] = &[
    "time",
    "timestamp",
    "ts",
    "@timestamp",
    "datetime",
    "date",
    "__REALTIME_TIMESTAMP",
];
const MSG_KEYS: &[&str] = &["msg", "message", "MESSAGE", "log", "text", "short_message"];

pub fn parse_json_line(line: &str) -> Option<ParsedEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;
    let journal = obj.contains_key("MESSAGE")
        || obj.contains_key("__REALTIME_TIMESTAMP")
        || obj.contains_key("_SYSTEMD_UNIT");

    let mut level = Level::Unknown;
    for key in LEVEL_KEYS {
        if let Some(v) = obj.get(*key) {
            if let Some(lvl) = level_from_value(v) {
                level = lvl;
                break;
            }
        }
    }
    let mut ts_ns = 0i64;
    for key in TIME_KEYS {
        if let Some(v) = obj.get(*key) {
            if let Some(ns) = time_from_value(v) {
                ts_ns = ns;
                break;
            }
        }
    }
    let mut message = String::new();
    for key in MSG_KEYS {
        if let Some(v) = obj.get(*key) {
            let text = value_message(v);
            if !text.is_empty() {
                message = text;
                break;
            }
        }
    }
    let mut stack = String::new();
    if let Some(text) = error_bits(obj.get("err")) {
        stack = text;
    } else if let Some(text) = error_bits(obj.get("error")) {
        stack = text;
    } else if let Some(Value::String(s)) = obj.get("stack") {
        stack = s.clone();
    }
    if message.is_empty() && !stack.is_empty() {
        message = stack.lines().next().unwrap_or("").to_string();
    }
    if message.is_empty() {
        message = line.chars().take(300).collect();
    }

    let mut facets = Vec::new();
    flatten("", obj, &mut facets, 0);
    if journal {
        if let Some(Value::String(unit)) = obj.get("_SYSTEMD_UNIT") {
            if !facets.iter().any(|(k, _)| k == "service") {
                let svc = unit.trim_end_matches(".service").to_string();
                facets.push(("service".into(), FacetValue::Str(svc)));
            }
        }
        if let Some(Value::String(id)) = obj.get("SYSLOG_IDENTIFIER") {
            if !facets.iter().any(|(k, _)| k == "service") {
                facets.push(("service".into(), FacetValue::Str(id.clone())));
            }
        }
        if let Some(Value::String(host)) = obj.get("_HOSTNAME") {
            facets.push(("host".into(), FacetValue::Str(host.clone())));
        }
        facets.push(("format_detail".into(), FacetValue::Str("journald".into())));
    }
    if !stack.is_empty() && !facets.iter().any(|(k, _)| k == "stack") {
        let clipped = if stack.len() > 4_000 {
            stack[..4_000].to_string()
        } else {
            stack
        };
        facets.push(("stack".into(), FacetValue::Str(clipped)));
    }
    Some(ParsedEvent {
        level,
        ts_ns,
        message,
        facets,
        format: if journal { "journald" } else { "json" },
    })
}

fn flatten(
    prefix: &str,
    obj: &serde_json::Map<String, Value>,
    out: &mut Vec<(String, FacetValue)>,
    depth: u8,
) {
    for (k, v) in obj {
        if matches!(
            k.as_str(),
            "msg" | "message" | "MESSAGE" | "stack" | "msg_template"
        ) {
            continue;
        }
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Object(child) if depth < 1 && k != "err" && k != "error" => {
                flatten(&key, child, out, depth + 1);
            }
            Value::Object(_) | Value::Array(_) => {
                if k == "err" || k == "error" {
                    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
                        out.push(("error".into(), FacetValue::Str(msg.to_string())));
                    }
                } else {
                    out.push((key, FacetValue::from_json(v)));
                }
            }
            other => out.push((key, FacetValue::from_json(other))),
        }
    }
}

fn error_bits(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => {
            let stack = map.get("stack").and_then(|v| v.as_str()).unwrap_or("");
            if !stack.is_empty() {
                return Some(stack.to_string());
            }
            map.get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
        _ => None,
    }
}

fn value_message(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn level_from_value(v: &Value) -> Option<Level> {
    match v {
        Value::Number(n) => n.as_i64().map(Level::from_number),
        Value::String(s) => {
            if let Ok(i) = s.parse::<i64>() {
                if (0..=7).contains(&i) || ((10..=60).contains(&i) && i % 10 == 0) {
                    return Some(Level::from_number(i));
                }
            }
            Level::parse_name(s)
        }
        _ => None,
    }
}

fn time_from_value(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().and_then(crate::timeutil::epoch_to_ns),
        Value::String(s) => crate::timeutil::parse_time_str(s),
        _ => None,
    }
}
