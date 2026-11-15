//! Shared value types: levels, facets, and source identity.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::OnceLock;

/// Best-effort local host tag. Reads `/etc/hostname` once.
pub fn local_hostname() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "localhost".into())
        })
        .clone()
}

/// Severity stored in the sparse index as a single byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Level {
    Unknown = 0,
    Trace = 1,
    Debug = 2,
    Info = 3,
    Warn = 4,
    Error = 5,
    Fatal = 6,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }

    pub fn rank(self) -> u8 {
        self as u8
    }

    pub fn is_error(self) -> bool {
        matches!(self, Self::Error | Self::Fatal)
    }

    /// Histogram band: trace/debug/info/unknown share the INFO stack.
    pub fn band(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Error | Self::Fatal => "error",
            _ => "info",
        }
    }

    pub fn parse_name(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_lowercase();
        let n = n.trim_matches(|c: char| !c.is_ascii_alphabetic());
        Some(match n {
            "trace" | "trc" => Self::Trace,
            "debug" | "dbg" => Self::Debug,
            "info" | "inf" | "information" | "notice" => Self::Info,
            "warn" | "warning" | "wrn" => Self::Warn,
            "error" | "err" => Self::Error,
            "fatal" | "critical" | "crit" | "alert" | "emerg" | "emergency" | "panic" => {
                Self::Fatal
            }
            "dpanic" => Self::Error,
            _ => return None,
        })
    }

    /// Pino/Bunyan 10..60 and syslog priorities 0..7.
    pub fn from_number(n: i64) -> Self {
        if (10..=60).contains(&n) && n % 10 == 0 {
            return match n {
                10 => Self::Trace,
                20 => Self::Debug,
                30 => Self::Info,
                40 => Self::Warn,
                50 => Self::Error,
                60 => Self::Fatal,
                _ => Self::Info,
            };
        }
        match n {
            0..=2 => Self::Fatal,
            3 => Self::Error,
            4 => Self::Warn,
            5 | 6 => Self::Info,
            7 => Self::Debug,
            _ => Self::Info,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed facet value. Numbers stay numeric so `status:>=500` works.
#[derive(Debug, Clone, PartialEq)]
pub enum FacetValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl FacetValue {
    pub fn from_token(raw: &str) -> Self {
        let raw = unquote(raw);
        if raw.eq_ignore_ascii_case("true") {
            return Self::Bool(true);
        }
        if raw.eq_ignore_ascii_case("false") {
            return Self::Bool(false);
        }
        if looks_like_int(&raw) {
            if let Ok(i) = raw.parse::<i64>() {
                return Self::Int(i);
            }
        }
        if raw.contains('.') {
            if let Ok(f) = raw.parse::<f64>() {
                if f.is_finite() {
                    return Self::Float(f);
                }
            }
        }
        Self::Str(raw)
    }

    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Str("null".into()),
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else if let Some(u) = n.as_u64() {
                    if u <= i64::MAX as u64 {
                        Self::Int(u as i64)
                    } else {
                        Self::Str(n.to_string())
                    }
                } else if let Some(f) = n.as_f64() {
                    Self::Float(f)
                } else {
                    Self::Str(n.to_string())
                }
            }
            serde_json::Value::String(s) => Self::from_token(s),
            other => {
                let s = other.to_string();
                if s.len() > 2_000 {
                    Self::Str(format!("{}…", &s[..2_000]))
                } else {
                    Self::Str(s)
                }
            }
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{f:.0}")
                } else {
                    let s = format!("{f}");
                    s
                }
            }
            Self::Bool(b) => {
                if *b {
                    "true".into()
                } else {
                    "false".into()
                }
            }
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) if f.is_finite() => Some(*f),
            Self::Float(_) => None,
            Self::Str(s) => s.parse::<f64>().ok(),
            Self::Bool(_) => None,
        }
    }

    pub fn eq_token(&self, expected: &str) -> bool {
        match self {
            Self::Str(s) => s == expected,
            Self::Int(i) => expected.parse::<i64>().ok() == Some(*i) || i.to_string() == expected,
            Self::Float(f) => {
                expected
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|n| (n - f).abs() < 1e-9)
                    || self.display() == expected
            }
            Self::Bool(b) => expected.eq_ignore_ascii_case(if *b { "true" } else { "false" }),
        }
    }
}

fn unquote(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2 {
        let b = t.as_bytes();
        if (b[0] == b'"' && b[t.len() - 1] == b'"') || (b[0] == b'\'' && b[t.len() - 1] == b'\'') {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

fn looks_like_int(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let start = usize::from(bytes[0] == b'-' || bytes[0] == b'+');
    start < bytes.len() && bytes[start..].iter().all(|c| c.is_ascii_digit())
}

/// Where a buffer came from. Facets `source` / `host` / `service` default to this.
#[derive(Debug, Clone)]
pub struct Origin {
    pub name: String,
    pub host: String,
    pub service: String,
    pub kind: SourceKind,
    /// Docker CLI timestamps are a prefix on each line (`docker logs --timestamps`).
    pub docker_timestamps: bool,
}

impl Origin {
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: local_hostname(),
            service: String::new(),
            kind: SourceKind::File,
            docker_timestamps: false,
        }
    }

    pub fn named(kind: SourceKind, name: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: local_hostname(),
            service: service.into(),
            kind,
            docker_timestamps: kind == SourceKind::Docker,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    File,
    Stdin,
    Ssh,
    Docker,
    Journald,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Stdin => "stdin",
            Self::Ssh => "ssh",
            Self::Docker => "docker",
            Self::Journald => "journald",
        }
    }
}

/// Rough token estimate used by the MCP tools (about 4 characters per token).
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.len().div_ceil(4).max(1)
    }
}

pub fn is_correlation_key(key: &str) -> bool {
    let mut norm = String::with_capacity(key.len());
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            norm.push(c.to_ascii_lowercase());
        }
    }
    matches!(
        norm.as_str(),
        "reqid"
            | "requestid"
            | "traceid"
            | "spanid"
            | "correlationid"
            | "xrequestid"
            | "traceparent"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumOp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl NumOp {
    pub fn test(self, left: f64, right: f64) -> bool {
        match self {
            Self::Eq => (left - right).abs() < 1e-9,
            Self::Gt => left > right,
            Self::Gte => left >= right,
            Self::Lt => left < right,
            Self::Lte => left <= right,
        }
    }
}
