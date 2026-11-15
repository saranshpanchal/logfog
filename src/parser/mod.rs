//! Auto-detecting multi-format parser and multi-line stack folding.
//!
//! Each line is classified as JSON (including pino/bunyan numeric levels and
//! journald), logfmt, an nginx/apache access line, or plain text. Continuation
//! lines that belong to a traceback are folded into the parent event. ANSI
//! sequences are stripped for indexing; the raw bytes stay in the source buffer
//! for display.

mod access;
mod ansi;
mod jsonlog;
mod logfmt;

use crate::timeutil::leading_timestamp;
use crate::types::{FacetValue, Level, Origin};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use memchr::memchr;
use std::sync::OnceLock;

pub use ansi::strip_ansi;

/// One folded event, described by a byte range in the source buffer.
#[derive(Debug, Clone)]
pub struct RawGroup {
    pub offset: u64,
    pub len: u32,
    pub line: u32,
    pub span_lines: u16,
    pub first: String,
    pub rest: String,
}

#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub level: Level,
    pub ts_ns: i64,
    pub message: String,
    pub facets: Vec<(String, FacetValue)>,
    pub format: &'static str,
}

struct LineRef {
    offset: u64,
    len: u32,
    line_no: u32,
    stripped: String,
}

/// Split `data` (an unindexed suffix starting at `base_offset`) into folded events.
///
/// When `streaming` is set, a trailing partial line and an unfinished stack are
/// left unconsumed so a later chunk can complete them. The returned offset is
/// the absolute byte position up to which events were committed.
pub fn extract_groups(data: &[u8], base_offset: u64, streaming: bool) -> (Vec<RawGroup>, u64) {
    if data.is_empty() {
        return (Vec::new(), base_offset);
    }
    let lines = split_lines(data, base_offset);
    if lines.is_empty() {
        return (Vec::new(), base_offset + data.len() as u64);
    }
    let partial = streaming && !data.ends_with(b"\n");
    let usable = if partial && !lines.is_empty() {
        lines.len() - 1
    } else {
        lines.len()
    };
    if usable == 0 {
        return (Vec::new(), base_offset);
    }
    let mut ranges = fold_ranges(&lines[..usable]);
    if streaming {
        if let Some(last) = ranges.last().copied() {
            if group_is_open(&lines[last.0..last.1]) {
                ranges.pop();
            }
        }
    }
    if ranges.is_empty() {
        return (Vec::new(), base_offset);
    }
    let groups = ranges
        .into_iter()
        .map(|(start, end)| make_group(&lines[start..end]))
        .collect::<Vec<_>>();
    let last = groups.last().expect("ranges non-empty");
    let new_off = last.offset + u64::from(last.len);
    (groups, new_off)
}

fn make_group(lines: &[LineRef]) -> RawGroup {
    let first = &lines[0];
    let last = lines.last().expect("group has a line");
    let end_off = last.offset + u64::from(last.len);
    let mut rest = String::new();
    for line in lines.iter().skip(1) {
        if !rest.is_empty() {
            rest.push('\n');
        }
        rest.push_str(line.stripped.trim_end_matches(['\r', '\n']));
    }
    let span = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    RawGroup {
        offset: first.offset,
        len: u32::try_from(end_off - first.offset).unwrap_or(u32::MAX),
        line: first.line_no,
        span_lines: span,
        first: first.stripped.trim_end_matches(['\r', '\n']).to_string(),
        rest,
    }
}

fn split_lines(data: &[u8], base_offset: u64) -> Vec<LineRef> {
    let mut out = Vec::new();
    let mut start = 0usize;
    // Line numbers are 1-based within this suffix. The engine adds the count of
    // newlines before the suffix to make them absolute.
    let mut line_no = 1u32;
    while start <= data.len() {
        if start == data.len() {
            break;
        }
        let rel = memchr(b'\n', &data[start..]);
        let (end, next) = if let Some(rel) = rel {
            (start + rel + 1, start + rel + 1)
        } else {
            (data.len(), data.len() + 1)
        };
        let raw = &data[start..end.min(data.len())];
        let text_end = if raw.last() == Some(&b'\n') {
            raw.len() - 1
        } else {
            raw.len()
        };
        let text_end = if text_end > 0 && raw[text_end - 1] == b'\r' {
            text_end - 1
        } else {
            text_end
        };
        let text = String::from_utf8_lossy(&raw[..text_end]);
        let stripped = if memchr(0x1b, raw).is_some() {
            strip_ansi(&text)
        } else {
            text.into_owned()
        };
        out.push(LineRef {
            offset: base_offset + start as u64,
            len: (end - start) as u32,
            line_no,
            stripped,
        });
        line_no = line_no.saturating_add(1);
        if next > data.len() {
            break;
        }
        start = next;
    }
    out
}

/// Fold stack continuations. Returns half-open ranges into `lines`.
fn fold_ranges(lines: &[LineRef]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].stripped.trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        let mut stack = StackFold::from_line(&lines[i].stripped);
        i += 1;
        while i < lines.len() {
            if !stack.absorb(&lines[i].stripped) {
                break;
            }
            i += 1;
            if stack.finished {
                break;
            }
        }
        groups.push((start, i));
    }
    groups
}

struct StackFold {
    active: bool,
    saw_frame: bool,
    soft_left: u8,
    finished: bool,
    /// Python tracebacks end at `RuntimeError: ...`. Other stacks must not
    /// swallow a following `Error: ...` line from Node.
    python: bool,
}

impl StackFold {
    fn from_line(line: &str) -> Self {
        Self {
            active: is_stack_header(line),
            saw_frame: false,
            soft_left: 4,
            finished: false,
            python: line.contains("Traceback"),
        }
    }

    fn absorb(&mut self, line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return self.active;
        }
        if line.contains("Traceback") {
            self.python = true;
        }
        if is_stack_header(line) {
            // A traceback continues the log line above it. A panic or a
            // goroutine header starts its own event unless a stack is already open.
            let continues_log = trimmed.starts_with("Traceback")
                || trimmed.starts_with("stack backtrace:")
                || trimmed.starts_with("Stack trace:");
            if self.active || continues_log {
                self.active = true;
                return true;
            }
            return false;
        }
        if is_hard_continuation(line) {
            self.active = true;
            if trimmed.starts_with("File \"") || trimmed.starts_with("File '") {
                self.python = true;
            }
            self.saw_frame = true;
            return true;
        }
        if self.active && self.python && is_python_exception_line(line) {
            self.finished = true;
            return true;
        }
        if self.active && (line.starts_with(' ') || line.starts_with('\t')) {
            self.saw_frame = true;
            return true;
        }
        if self.active && !self.saw_frame && self.soft_left > 0 && !is_fresh_event(line) {
            self.soft_left -= 1;
            return true;
        }
        false
    }
}

fn group_is_open(lines: &[LineRef]) -> bool {
    let Some(last) = lines.last() else {
        return false;
    };
    let text = last.stripped.trim();
    if text.is_empty() {
        return false;
    }
    if is_stack_header(text) || is_hard_continuation(&last.stripped) {
        return true;
    }
    // `Error: ...` / `RuntimeError: ...` often gains frames on the next line.
    // Follow mode flushes the hold after a short quiet period.
    if is_python_exception_line(text) {
        return true;
    }
    if last.stripped.starts_with(' ') || last.stripped.starts_with('\t') {
        return true;
    }
    let saw_header = lines.iter().any(|l| is_stack_header(&l.stripped));
    let saw_end = lines.iter().any(|l| is_python_exception_line(&l.stripped));
    saw_header && !saw_end
}

fn is_fresh_event(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('{') {
        return true;
    }
    if is_access_line(t) {
        return true;
    }
    if logfmt::is_full_logfmt(t) {
        return true;
    }
    if leading_timestamp(t).is_some() {
        return true;
    }
    // A level token only counts at the start. "explicit panic" is a panic
    // message, not a new ERROR/PANIC log line.
    level_token_at_start(t).is_some()
}

pub fn interpret(first: &str, rest: &str, origin: &Origin) -> ParsedEvent {
    let first = if first.as_bytes().contains(&0x1b) {
        strip_ansi(first)
    } else {
        first.to_string()
    };
    let rest = if rest.as_bytes().contains(&0x1b) {
        strip_ansi(rest)
    } else {
        rest.to_string()
    };
    let mut line = first;
    let mut compose: Option<String> = None;
    if let Some((svc, remainder)) = split_compose(&line) {
        compose = Some(svc.to_string());
        line = remainder.to_string();
    }
    let mut ts_hint = None;
    if origin.docker_timestamps {
        if let Some((ts, remainder)) = split_docker_ts(&line) {
            ts_hint = Some(ts);
            line = remainder.to_string();
        }
    }
    let mut parsed = parse_one(&line);
    if parsed.ts_ns == 0 {
        if let Some(ts) = ts_hint {
            parsed.ts_ns = ts;
        }
    }
    if let Some(svc) = compose {
        parsed
            .facets
            .push(("compose_service".into(), FacetValue::Str(svc.clone())));
        if !has_key(&parsed.facets, "service") {
            parsed.facets.push(("service".into(), FacetValue::Str(svc)));
        }
    }
    refine_stack_message(&mut parsed, &rest);
    if parsed.level == Level::Unknown && failure_text(&line, &rest) {
        parsed.level = Level::Error;
    }
    apply_origin(&mut parsed, origin);
    if parsed.message.len() > 4_000 {
        parsed.message.truncate(4_000);
    }
    parsed
}

fn parse_one(line: &str) -> ParsedEvent {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Some(ev) = jsonlog::parse_json_line(trimmed) {
            return ev;
        }
    }
    if let Some(ev) = access::parse_access(trimmed) {
        return ev;
    }
    if let Some(ev) = logfmt::parse_logfmt_line(trimmed) {
        return ev;
    }
    parse_plain(trimmed)
}

fn parse_plain(line: &str) -> ParsedEvent {
    let (ts_ns, consumed) = match leading_timestamp(line) {
        Some((ns, n)) => (ns, n),
        None => (0, 0),
    };
    let body = line.get(consumed..).unwrap_or(line);
    let level = level_word(line, 64).unwrap_or(Level::Unknown);
    let message = strip_level_token(body);
    let mut facets = logfmt::inline_pairs(line);
    // A leading level word is not a facet; inline pairs still are.
    facets.retain(|(k, _)| !k.eq_ignore_ascii_case("level"));
    ParsedEvent {
        level,
        ts_ns,
        message: {
            let m = message.trim();
            if m.is_empty() {
                line.to_string()
            } else {
                m.to_string()
            }
        },
        facets,
        format: "text",
    }
}

fn strip_level_token(body: &str) -> String {
    let t = body.trim_start();
    if let Some((level, len)) = level_token_at_start(t) {
        let _ = level;
        let rest = t[len..].trim_start();
        let rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
        return rest.to_string();
    }
    // [ERROR] or <INFO>
    let bytes = t.as_bytes();
    if bytes.first() == Some(&b'[') || bytes.first() == Some(&b'<') {
        if let Some(end) = t.find([']', '>']) {
            let inside = &t[1..end];
            if Level::parse_name(inside).is_some() {
                return t[end + 1..].trim_start().to_string();
            }
        }
    }
    t.to_string()
}

fn level_token_at_start(t: &str) -> Option<(Level, usize)> {
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let word = &t[..i];
    let level = Level::parse_name(word)?;
    // Require a boundary so "information" can match fully via parse_name,
    // but "errors" does not become error.
    let boundary = bytes.get(i).is_none_or(|c| !c.is_ascii_alphanumeric());
    if !boundary {
        return None;
    }
    Some((level, i))
}

fn refine_stack_message(parsed: &mut ParsedEvent, rest: &str) {
    if rest.is_empty() {
        return;
    }
    let head = parsed.message.trim().to_string();
    let replace = head.starts_with("Traceback (most recent call last)")
        || head.starts_with("stack backtrace:")
        || head.starts_with("panic:");
    if let Some(line) = exception_summary(rest).filter(|_| replace) {
        parsed.message = line;
    }
    let panic_like = (head.starts_with("thread '") && head.contains(" panicked at"))
        || head.starts_with("panic:")
        || head.starts_with("Traceback");
    if panic_like && matches!(parsed.level, Level::Unknown | Level::Info) {
        parsed.level = Level::Error;
    }
}

fn exception_summary(rest: &str) -> Option<String> {
    let mut last_exc = None;
    for line in rest.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if is_python_exception_line(t) || t.starts_with("panic:") {
            last_exc = Some(t.to_string());
        }
    }
    if last_exc.is_some() {
        return last_exc;
    }
    rest.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !is_hard_continuation(l) && !is_stack_header(l))
        .map(str::to_string)
}

fn failure_text(first: &str, rest: &str) -> bool {
    is_stack_header(first)
        || is_stack_header(rest)
        || rest
            .lines()
            .any(|l| is_stack_header(l) || is_hard_continuation(l))
}

fn apply_origin(parsed: &mut ParsedEvent, origin: &Origin) {
    if !has_key(&parsed.facets, "source") {
        parsed
            .facets
            .push(("source".into(), FacetValue::Str(origin.name.clone())));
    } else if facet_str(parsed, "source") != Some(origin.name.as_str()) {
        if let Some(existing) = take_facet(parsed, "source") {
            parsed.facets.push(("log_source".into(), existing));
        }
        parsed
            .facets
            .push(("source".into(), FacetValue::Str(origin.name.clone())));
    }
    if !has_key(&parsed.facets, "host") {
        parsed
            .facets
            .push(("host".into(), FacetValue::Str(origin.host.clone())));
    }
    if !has_key(&parsed.facets, "service") && !origin.service.is_empty() {
        parsed
            .facets
            .push(("service".into(), FacetValue::Str(origin.service.clone())));
    }
    // Drop duplicate keys, keeping the last write (origin overrides above were pushes).
    dedup_facets(&mut parsed.facets);
}

fn dedup_facets(facets: &mut Vec<(String, FacetValue)>) {
    let mut seen = Vec::new();
    let mut out = Vec::with_capacity(facets.len());
    for (k, v) in facets.drain(..).rev() {
        if seen.iter().any(|s: &String| s == &k) {
            continue;
        }
        seen.push(k.clone());
        out.push((k, v));
    }
    out.reverse();
    *facets = out;
}

fn has_key(facets: &[(String, FacetValue)], key: &str) -> bool {
    facets.iter().any(|(k, _)| k == key)
}

fn facet_str<'a>(parsed: &'a ParsedEvent, key: &str) -> Option<&'a str> {
    parsed
        .facets
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| {
            if let FacetValue::Str(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        })
}

fn take_facet(parsed: &mut ParsedEvent, key: &str) -> Option<FacetValue> {
    if let Some(pos) = parsed.facets.iter().position(|(k, _)| k == key) {
        Some(parsed.facets.remove(pos).1)
    } else {
        None
    }
}

/// Docker Compose prefix: `api_1 | message`. Requires a digit so ordinary
/// `WARN | note` lines are not treated as service names.
pub fn split_compose(line: &str) -> Option<(&str, &str)> {
    let pipe = line.find('|')?;
    if pipe == 0
        || !line
            .as_bytes()
            .get(pipe - 1)
            .is_some_and(|c| c.is_ascii_whitespace())
    {
        return None;
    }
    let name = line[..pipe].trim();
    if name.is_empty() || name.len() > 80 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return None;
    }
    if !name.chars().any(|c| c.is_ascii_digit()) || !name.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest = line[pipe + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((name, rest))
}

pub fn split_docker_ts(line: &str) -> Option<(i64, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() < 22 || bytes.get(4) != Some(&b'-') {
        return None;
    }
    let end = line.find(char::is_whitespace)?;
    let token = &line[..end];
    let ns = crate::timeutil::parse_time_str(token)?;
    let rest = line[end..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((ns, rest))
}

fn is_access_line(line: &str) -> bool {
    access::parse_access(line).is_some()
}

pub fn is_stack_header(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("Traceback (most recent call last)")
        || t.starts_with("stack backtrace:")
        || t.starts_with("Stack trace:")
        || t.starts_with("panic:")
        || t.starts_with("goroutine ")
        || (t.starts_with("thread '") && t.contains(" panicked at"))
        || t.contains("Exception in thread")
}

pub fn is_hard_continuation(s: &str) -> bool {
    let t = s.trim_start();
    if t.starts_with("at ") {
        return true;
    }
    if t.starts_with("...") && t.contains(" more") {
        return true;
    }
    if t.starts_with("Caused by:") || t.starts_with("Suppressed:") {
        return true;
    }
    if t.starts_with("File \"") || t.starts_with("File '") {
        return true;
    }
    if t.starts_with("created by ") || t.starts_with("goroutine ") {
        return true;
    }
    if t.starts_with("During handling of the above exception")
        || t.starts_with("The above exception was the direct cause")
    {
        return true;
    }
    if t.starts_with("note: ") && (t.contains("backtrace") || t.contains("omitted")) {
        return true;
    }
    if is_rust_frame(t) {
        return true;
    }
    if is_go_func_line(t.trim()) {
        return true;
    }
    let indented = s.starts_with([' ', '\t']);
    indented
        && (t.contains(".go:")
            || t.contains(".rs:")
            || t.contains(".java:")
            || t.contains(".py:")
            || t.contains(" +0x"))
}

fn is_rust_frame(t: &str) -> bool {
    let t = t.trim_start();
    let mut digits = 0usize;
    let bytes = t.as_bytes();
    while digits < bytes.len() && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    digits > 0 && bytes.get(digits) == Some(&b':') && bytes.get(digits + 1) == Some(&b' ')
}

fn is_go_func_line(t: &str) -> bool {
    if !t.ends_with(')') {
        return false;
    }
    let Some(open) = t.rfind('(') else {
        return false;
    };
    if open == 0 {
        return false;
    }
    let name = &t[..open];
    if !name.contains('.') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '*' | '_' | '-'))
}

pub fn is_python_exception_line(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || s.starts_with(' ') || s.starts_with('\t') {
        return false;
    }
    let Some((head, _msg)) = t.split_once(':') else {
        return false;
    };
    if head.contains(' ') {
        return false;
    }
    // "ERROR:" / "WARN:" are log prefixes, not Python exceptions.
    if head.chars().all(|c| c.is_ascii_uppercase() || c == '_') && head.len() <= 12 {
        return false;
    }
    let base = head.rsplit('.').next().unwrap_or(head);
    base.ends_with("Error") || base.ends_with("Exception") || base == "Panic"
}

fn level_ac() -> &'static AhoCorasick {
    static AC: OnceLock<AhoCorasick> = OnceLock::new();
    AC.get_or_init(|| {
        AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostLongest)
            .build([
                "trace",
                "debug",
                "information",
                "info",
                "warning",
                "warn",
                "error",
                "fatal",
                "critical",
                "panic",
            ])
            .expect("level automaton")
    })
}

fn level_word(line: &str, window: usize) -> Option<Level> {
    let slice = if line.len() > window {
        // Cut on a char boundary.
        let mut end = window.min(line.len());
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    } else {
        line
    };
    for mat in level_ac().find_iter(slice) {
        let start = mat.start();
        let end = mat.end();
        if !word_boundary(slice, start, end) {
            continue;
        }
        let word = &slice[start..end];
        if let Some(level) = Level::parse_name(word) {
            return Some(level);
        }
    }
    None
}

fn word_boundary(s: &str, start: usize, end: usize) -> bool {
    let b = s.as_bytes();
    let before = start == 0 || !b[start - 1].is_ascii_alphanumeric();
    let after = end >= b.len() || !b[end].is_ascii_alphanumeric();
    before && after
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::file("fixture.log")
    }

    #[test]
    fn folds_python_rust_and_node() {
        let raw = "\
ERROR something broke in the handler
Traceback (most recent call last):
  File \"app.py\", line 10, in <module>
    main()
RuntimeError: boom
thread 'worker' panicked at src/main.rs:42:5:
explicit panic
stack backtrace:
   0: rust_begin_unwind
             at /rustc/library/std/src/panicking.rs:1:1
Error: connection reset
    at Socket.emit (node:net:123:1)
    at TCP.<anonymous> (node:net:456:7)
INFO next line
";
        let (groups, off) = extract_groups(raw.as_bytes(), 0, false);
        assert_eq!(off, raw.len() as u64);
        assert_eq!(groups.len(), 4, "groups: {groups:?}");
        assert!(groups[0].rest.contains("RuntimeError: boom"));
        assert!(groups[0].rest.contains("File \"app.py\""));
        assert!(groups[1].first.contains("panicked at"));
        assert!(groups[1].rest.contains("rust_begin_unwind"));
        assert!(groups[2].rest.contains("Socket.emit"));
        assert!(groups[3].first.starts_with("INFO"));
    }

    #[test]
    fn pino_numeric_levels() {
        for (n, level) in [
            (10, Level::Trace),
            (20, Level::Debug),
            (30, Level::Info),
            (40, Level::Warn),
            (50, Level::Error),
            (60, Level::Fatal),
        ] {
            let line = format!(r#"{{"level":{n},"msg":"m","service":"api"}}"#);
            let ev = interpret(&line, "", &origin());
            assert_eq!(ev.level, level, "pino {n}");
            assert_eq!(ev.message, "m");
        }
    }

    #[test]
    fn ansi_is_stripped_for_the_message() {
        let line = "\u{1b}[31mERROR\u{1b}[0m colored failure code=E42";
        let ev = interpret(line, "", &origin());
        assert_eq!(ev.level, Level::Error);
        assert!(ev.message.contains("colored failure"));
        assert!(!ev.message.contains('\u{1b}'));
    }
}
