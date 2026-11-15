//! Query language.
//!
//! ```text
//! level:error service:api status:>=500 -service:healthcheck "timeout" /disk/ since:1h
//! ```
//!
//! Relative durations (`since:1h`) are measured from the newest timestamp in the
//! index, so historical files still match.

use crate::timeutil::parse_time_str;
use crate::types::{Level, NumOp};
use anyhow::{anyhow, Result};
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Cmp {
    fn from_prefix(value: &str) -> (Self, &str) {
        if let Some(rest) = value.strip_prefix(">=") {
            (Self::Gte, rest)
        } else if let Some(rest) = value.strip_prefix("<=") {
            (Self::Lte, rest)
        } else if let Some(rest) = value.strip_prefix('>') {
            (Self::Gt, rest)
        } else if let Some(rest) = value.strip_prefix('<') {
            (Self::Lt, rest)
        } else {
            (Self::Eq, value)
        }
    }
}

#[derive(Debug)]
pub enum FieldOp {
    Eq(String),
    Num(NumOp, f64),
    Range(f64, f64),
}

#[derive(Debug)]
pub enum Pred {
    Level { cmp: Cmp, level: Level, neg: bool },
    Field { key: String, op: FieldOp, neg: bool },
    Text { needle: String, neg: bool },
    Regex { re: Regex, neg: bool },
    Template { id: u32, neg: bool },
    TimeGte(i64),
    TimeLte(i64),
}

#[derive(Debug)]
pub struct Query {
    pub preds: Vec<Pred>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.preds.is_empty()
    }
}

pub fn parse(input: &str, newest_ns: i64) -> Result<Query> {
    let tokens = tokenize(input)?;
    let mut preds = Vec::new();
    for tok in tokens {
        preds.push(pred_from(tok, newest_ns)?);
    }
    Ok(Query { preds })
}

struct Tok {
    neg: bool,
    kind: TokKind,
}

enum TokKind {
    Quoted(String),
    Regex { pat: String, flags: String },
    Bare(String),
}

fn pred_from(tok: Tok, newest_ns: i64) -> Result<Pred> {
    match tok.kind {
        TokKind::Quoted(s) => Ok(Pred::Text {
            needle: s,
            neg: tok.neg,
        }),
        TokKind::Regex { pat, flags } => {
            let mut builder = regex::RegexBuilder::new(&pat);
            if flags.contains('i') {
                builder.case_insensitive(true);
            }
            let re = builder
                .build()
                .map_err(|e| anyhow!("bad regex /{pat}/: {e}"))?;
            Ok(Pred::Regex { re, neg: tok.neg })
        }
        TokKind::Bare(s) => bare_pred(&s, tok.neg, newest_ns),
    }
}

fn bare_pred(s: &str, neg: bool, newest_ns: i64) -> Result<Pred> {
    let Some((key, value)) = split_field(s) else {
        return Ok(Pred::Text {
            needle: s.to_string(),
            neg,
        });
    };
    let key_l = key.to_ascii_lowercase();
    if key_l == "since" || key_l == "from" {
        let ns = bound(value, newest_ns)?;
        return Ok(Pred::TimeGte(ns));
    }
    if key_l == "until" || key_l == "to" {
        let ns = bound(value, newest_ns)?;
        return Ok(Pred::TimeLte(ns));
    }
    if key_l == "level" || key_l == "severity" {
        let (cmp, name) = Cmp::from_prefix(value);
        let level = Level::parse_name(name).ok_or_else(|| anyhow!("unknown level `{name}`"))?;
        return Ok(Pred::Level { cmp, level, neg });
    }
    if key_l == "template" {
        let id: u32 = value
            .parse()
            .map_err(|_| anyhow!("template id must be an integer"))?;
        return Ok(Pred::Template { id, neg });
    }
    if key_l == "ts" || key_l == "time" || key_l == "timestamp" {
        let (cmp, rest) = Cmp::from_prefix(value);
        let ns = parse_time_str(rest)
            .or_else(|| bound(rest, newest_ns).ok())
            .ok_or_else(|| anyhow!("bad timestamp `{rest}`"))?;
        return match cmp {
            Cmp::Eq => Ok(Pred::TimeGte(ns)),
            Cmp::Gt | Cmp::Gte => Ok(Pred::TimeGte(ns)),
            Cmp::Lt | Cmp::Lte => Ok(Pred::TimeLte(ns)),
        };
    }
    if let Some((lo, hi)) = value.split_once("..") {
        if let (Ok(lo), Ok(hi)) = (lo.parse::<f64>(), hi.parse::<f64>()) {
            return Ok(Pred::Field {
                key: key.to_string(),
                op: FieldOp::Range(lo, hi),
                neg,
            });
        }
    }
    let (cmp, rest) = Cmp::from_prefix(value);
    if cmp == Cmp::Eq {
        return Ok(Pred::Field {
            key: key.to_string(),
            op: FieldOp::Eq(rest.to_string()),
            neg,
        });
    }
    let n: f64 = rest
        .parse()
        .map_err(|_| anyhow!("`{key}:{value}` needs a number"))?;
    let op = match cmp {
        Cmp::Gt => NumOp::Gt,
        Cmp::Gte => NumOp::Gte,
        Cmp::Lt => NumOp::Lt,
        Cmp::Lte => NumOp::Lte,
        Cmp::Eq => NumOp::Eq,
    };
    Ok(Pred::Field {
        key: key.to_string(),
        op: FieldOp::Num(op, n),
        neg,
    })
}

fn bound(value: &str, newest_ns: i64) -> Result<i64> {
    if let Some(ns) = parse_time_str(value) {
        return Ok(ns);
    }
    let dur = humantime::parse_duration(value)
        .map_err(|_| anyhow!("bad duration or timestamp `{value}`"))?;
    let sub = i64::try_from(dur.as_nanos()).unwrap_or(i64::MAX);
    Ok(newest_ns.saturating_sub(sub))
}

fn split_field(s: &str) -> Option<(&str, &str)> {
    let (key, value) = s.split_once(':')?;
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '@')
    {
        return None;
    }
    Some((key, value))
}

fn tokenize(input: &str) -> Result<Vec<Tok>> {
    let b = input.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let mut neg = false;
        if b[i] == b'-' && i + 1 < b.len() {
            let n = b[i + 1];
            if n == b'"' || n == b'/' || n.is_ascii_alphabetic() || n == b'_' {
                neg = true;
                i += 1;
            }
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'"' {
            let (s, ni) = parse_quoted(input, i)?;
            out.push(Tok {
                neg,
                kind: TokKind::Quoted(s),
            });
            i = ni;
            continue;
        }
        if b[i] == b'/' {
            let (pat, flags, ni) = parse_regex_lit(input, i)?;
            out.push(Tok {
                neg,
                kind: TokKind::Regex { pat, flags },
            });
            i = ni;
            continue;
        }
        let start = i;
        i += 1;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            if b[i] == b':' && i + 1 < b.len() && b[i + 1] == b'"' {
                let (_s, ni) = parse_quoted(input, i + 1)?;
                i = ni;
                break;
            }
            i += 1;
        }
        let mut bare = input[start..i].to_string();
        // Collapse msg:"hello world" into msg:hello world without the quotes.
        if let Some((key, value)) = bare.split_once(':') {
            if let Some(inner) = unquote_full(value) {
                bare = format!("{key}:{inner}");
            }
        }
        out.push(Tok {
            neg,
            kind: TokKind::Bare(bare),
        });
    }
    Ok(out)
}

fn unquote_full(value: &str) -> Option<String> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        Some(value[1..value.len() - 1].to_string())
    } else {
        None
    }
}

fn parse_quoted(input: &str, start: usize) -> Result<(String, usize)> {
    let b = input.as_bytes();
    debug_assert_eq!(b[start], b'"');
    let mut i = start + 1;
    let mut out = String::new();
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            out.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        if b[i] == b'"' {
            return Ok((out, i + 1));
        }
        out.push(b[i] as char);
        i += 1;
    }
    Err(anyhow!("unterminated quote"))
}

fn parse_regex_lit(input: &str, start: usize) -> Result<(String, String, usize)> {
    let b = input.as_bytes();
    let mut i = start + 1;
    let mut pat = String::new();
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            if n == b'/' {
                pat.push('/');
            } else {
                pat.push('\\');
                pat.push(n as char);
            }
            i += 2;
            continue;
        }
        if b[i] == b'/' {
            i += 1;
            let flag_start = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            return Ok((pat, input[flag_start..i].to_string(), i));
        }
        pat.push(b[i] as char);
        i += 1;
    }
    Err(anyhow!("unterminated regex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_showcase_query() {
        let q = parse(
            r#"level:error service:api status:>=500 -service:healthcheck "timeout" /disk/i since:1h"#,
            1_000_000_000_000,
        )
        .unwrap();
        assert!(q.preds.len() >= 6);
    }
}
