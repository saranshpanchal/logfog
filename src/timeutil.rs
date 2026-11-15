//! Timestamp parsing shared by the ingest parser and the query language.

use chrono::{DateTime, NaiveDateTime, Utc};

/// Parse a timestamp into UTC nanoseconds.
///
/// Accepts RFC3339, `YYYY-MM-DD HH:MM:SS`, nginx/apache `[15/Nov/2026:12:00:01 +0000]`,
/// and unix epoch in seconds, milliseconds, microseconds, or nanoseconds.
pub fn parse_time_str(raw: &str) -> Option<i64> {
    let s = raw
        .trim()
        .trim_matches(|c| c == '[' || c == ']' || c == '"' || c == '\'');
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<f64>() {
        if s.len() >= 10 {
            if let Some(ns) = epoch_to_ns(n) {
                return Some(ns);
            }
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_nanos_opt();
    }
    if let Ok(dt) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z") {
        return dt.timestamp_nanos_opt();
    }
    if let Ok(dt) = DateTime::parse_from_str(s, "%d/%b/%Y:%H:%M:%S %z") {
        return dt.timestamp_nanos_opt();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    parse_syslog(s)
}

fn parse_syslog(s: &str) -> Option<i64> {
    let year = Utc::now().format("%Y");
    let with_year = format!("{year} {s}");
    if let Ok(dt) = NaiveDateTime::parse_from_str(&with_year, "%Y %b %e %H:%M:%S") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(&with_year, "%Y %b %d %H:%M:%S") {
        return dt.and_utc().timestamp_nanos_opt();
    }
    None
}

pub fn epoch_to_ns(n: f64) -> Option<i64> {
    if !n.is_finite() || n < 1e9 {
        return None;
    }
    let ns = if n >= 1e17 {
        n
    } else if n >= 1e14 {
        n * 1e3
    } else if n >= 1e11 {
        n * 1e6
    } else {
        n * 1e9
    };
    if !ns.is_finite() || ns > i64::MAX as f64 || ns < 0.0 {
        return None;
    }
    Some(ns as i64)
}

pub fn format_ns(ns: i64) -> Option<String> {
    if ns <= 0 {
        return None;
    }
    let secs = ns.div_euclid(1_000_000_000);
    let nsec = ns.rem_euclid(1_000_000_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nsec)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Leading timestamp on a plain-text line, plus the byte length consumed
/// (including one trailing space when present).
pub fn leading_timestamp(line: &str) -> Option<(i64, usize)> {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let start = usize::from(bytes[0] == b'[');
    // RFC3339 / ISO date at column 0.
    if bytes.len() >= start + 20 && bytes.get(start + 4) == Some(&b'-') {
        let end = scan_ts_token(line, start);
        let token = &line[start..end];
        if let Some(ns) = parse_time_str(token) {
            let mut consumed = end;
            if bytes.get(consumed) == Some(&b']') {
                consumed += 1;
            }
            if bytes.get(consumed) == Some(&b' ') {
                consumed += 1;
            }
            return Some((ns, consumed));
        }
    }
    // "Nov 15 12:00:01"
    if bytes.len() >= 15 && bytes[0].is_ascii_alphabetic() {
        let token = &line[..line.len().min(15)];
        if let Some(ns) = parse_syslog(token) {
            let mut consumed = 15.min(line.len());
            if bytes.get(consumed) == Some(&b' ') {
                consumed += 1;
            }
            return Some((ns, consumed));
        }
    }
    None
}

fn scan_ts_token(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b':' | b'.' | b'+' | b'Z' | b'z' | b'T')
        {
            i += 1;
        } else {
            break;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_and_epoch() {
        let ns = parse_time_str("2026-11-15T12:00:00Z").unwrap();
        assert!(ns > 0);
        let ms = parse_time_str("1731679445123").unwrap();
        assert!(ms > 1_000_000_000_000_000);
        let nginx = parse_time_str("15/Nov/2026:12:00:01 +0000").unwrap();
        assert!(nginx > 0);
    }
}
