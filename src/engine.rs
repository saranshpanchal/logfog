//! The in-memory engine: files + sparse index + Drain miner.

use crate::drain::{self, Miner};
use crate::index::{FileBytes, IndexedFile, LogIndex, Record};
use crate::parser;
use crate::query::{self, FieldOp, Pred, Query};
use crate::timeutil::format_ns;
use crate::types::{estimate_tokens, FacetValue, Level, Origin, SourceKind};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub struct Engine {
    pub index: LogIndex,
    pub drain: Miner,
    stdin_id: Option<u32>,
    pub hold_since: HashMap<u32, Instant>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            index: LogIndex::new(),
            drain: Miner::new(),
            stdin_id: None,
            hold_since: HashMap::new(),
        }
    }

    pub fn ingest_bytes(&mut self, origin: Origin, data: Vec<u8>) -> Result<u32> {
        let has_ansi = data.contains(&0x1b);
        let id = self.index.add_file(IndexedFile {
            origin,
            bytes: FileBytes::Buf(data),
            path: None,
            inode: None,
            indexed_upto: 0,
            next_line: 1,
            has_ansi,
            events: Vec::new(),
        });
        self.consume(id, false);
        Ok(id)
    }

    pub fn ingest_path(&mut self, path: &Path, followable: bool) -> Result<u32> {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let compressed = is_compressed(path);
        let (bytes, inode) = if compressed {
            (read_compressed(path)?, None)
        } else if followable {
            let meta =
                std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
            let inode = file_inode(&meta);
            let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            (FileBytes::Buf(data), inode)
        } else {
            let meta =
                std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
            let inode = file_inode(&meta);
            (map_file(path, meta.len())?, inode)
        };
        let has_ansi = bytes.as_slice().contains(&0x1b);
        let id = self.index.add_file(IndexedFile {
            origin: Origin::file(name),
            bytes,
            path: Some(path.to_path_buf()),
            inode,
            indexed_upto: 0,
            next_line: 1,
            has_ansi,
            events: Vec::new(),
        });
        self.consume(id, false);
        Ok(id)
    }

    pub fn ingest_paths(&mut self, paths: &[String], followable: bool) -> Result<usize> {
        let mut n = 0;
        for p in paths {
            if p == "-" {
                n += self.ingest_stdin()?;
                continue;
            }
            let expanded = crate::sources::expand_input(p)?;
            for path in expanded {
                self.ingest_path(&path, followable)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn ingest_stdin(&mut self) -> Result<usize> {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        self.ingest_bytes(Origin::named(SourceKind::Stdin, "stdin", "stdin"), buf)?;
        Ok(1)
    }

    pub fn ensure_stdin(&mut self) -> u32 {
        if let Some(id) = self.stdin_id {
            return id;
        }
        let id = self.index.add_file(IndexedFile {
            origin: Origin::named(SourceKind::Stdin, "stdin", "stdin"),
            bytes: FileBytes::Buf(Vec::new()),
            path: None,
            inode: None,
            indexed_upto: 0,
            next_line: 1,
            has_ansi: false,
            events: Vec::new(),
        });
        self.stdin_id = Some(id);
        id
    }

    pub fn append_bytes(&mut self, file_id: u32, extra: &[u8], streaming: bool) -> Vec<u32> {
        if extra.is_empty() && !streaming {
            return self.consume(file_id, false);
        }
        if !extra.is_empty() {
            if extra.contains(&0x1b) {
                self.index.file_mut(file_id).has_ansi = true;
            }
            if !self.index.file_mut(file_id).bytes.extend_buf(extra) {
                return Vec::new();
            }
        }
        self.consume(file_id, streaming)
    }

    /// Re-read a followed file. Rotation (shorter file or new inode) starts a
    /// new buffer so the old lines stay searchable.
    pub fn poll_path(&mut self, path: &Path) -> Result<Vec<u32>> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(Vec::new()),
        };
        let inode = file_inode(&meta);
        let disk_len = meta.len();
        let found = self
            .index
            .files()
            .iter()
            .enumerate()
            .rev()
            .find(|(_, f)| f.path.as_deref() == Some(path))
            .map(|(idx, f)| (idx as u32, f.bytes.len() as u64, f.inode, f.origin.clone()));
        let Some((file_id, cur_len, cur_inode, origin)) = found else {
            return Ok(Vec::new());
        };
        let rotated = cur_inode.zip(inode).is_some_and(|(a, b)| a != b) || disk_len < cur_len;
        if rotated {
            let data = std::fs::read(path).unwrap_or_default();
            let mut origin = origin;
            origin.name = format!("{}#rotated", origin.name.trim_end_matches("#rotated"));
            let id = self.ingest_bytes(origin, data)?;
            let f = self.index.file_mut(id);
            f.path = Some(path.to_path_buf());
            f.inode = inode;
            return Ok(self.index.file(id).events.clone());
        }
        if disk_len == cur_len {
            return self.flush_if_stale(file_id);
        }
        let mut fh = File::open(path)?;
        fh.seek(SeekFrom::Start(cur_len))?;
        let mut extra = Vec::new();
        fh.read_to_end(&mut extra)?;
        let mut ids = self.append_bytes(file_id, &extra, true);
        ids.extend(self.flush_if_stale(file_id)?);
        Ok(ids)
    }

    fn flush_if_stale(&mut self, file_id: u32) -> Result<Vec<u32>> {
        let file = self.index.file(file_id);
        let pending = file.bytes.len() as u64 > file.indexed_upto;
        if !pending {
            self.hold_since.remove(&file_id);
            return Ok(Vec::new());
        }
        let since = self.hold_since.entry(file_id).or_insert_with(Instant::now);
        if since.elapsed().as_millis() > 400 {
            self.hold_since.remove(&file_id);
            return Ok(self.consume(file_id, false));
        }
        Ok(Vec::new())
    }

    pub fn flush_all(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = (0..self.index.files().len() as u32).collect();
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.consume(id, false));
        }
        out
    }

    fn consume(&mut self, file_id: u32, streaming: bool) -> Vec<u32> {
        let before = self.index.event_count();
        let start = self.index.file(file_id).indexed_upto;
        let (groups, new_upto) = {
            let file = self.index.file(file_id);
            let bytes = file.bytes.as_slice();
            let from = start as usize;
            if from > bytes.len() {
                (Vec::new(), start)
            } else {
                parser::extract_groups(&bytes[from..], start, streaming)
            }
        };
        let origin = self.index.file(file_id).origin.clone();
        let base_line = {
            let bytes = self.index.file(file_id).bytes.as_slice();
            let end = (start as usize).min(bytes.len());
            bytes[..end].iter().filter(|b| **b == b'\n').count() as u32
        };
        for mut group in groups {
            group.line += base_line;
            let parsed = parser::interpret(&group.first, &group.rest, &origin);
            let tid = self
                .drain
                .observe(&parsed.message, parsed.level, parsed.ts_ns);
            let eid = self.index.append(file_id, &group, &parsed, tid);
            self.drain.note_event(tid, eid);
        }
        self.index.set_progress(file_id, new_upto);
        if streaming
            && self.index.file(file_id).indexed_upto < self.index.file(file_id).bytes.len() as u64
        {
            self.hold_since.entry(file_id).or_insert_with(Instant::now);
        } else {
            self.hold_since.remove(&file_id);
        }
        self.drain
            .refresh_spikes(self.index.min_ts, self.index.max_ts);
        (before..self.index.event_count()).collect()
    }

    pub fn len(&self) -> u32 {
        self.index.event_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn record(&self, id: u32) -> Option<Record> {
        self.index.record(id)
    }

    pub fn query(&self, q: &str) -> Result<Vec<u32>> {
        let parsed = query::parse(q, self.index.max_ts)?;
        Ok(self.search(&parsed, None, None))
    }

    pub fn search(&self, q: &Query, from_ns: Option<i64>, to_ns: Option<i64>) -> Vec<u32> {
        let mut out = Vec::new();
        let n = self.index.event_count();
        for id in 0..n {
            if self.matches(id, q, from_ns, to_ns) {
                out.push(id);
            }
        }
        out.sort_by(|&a, &b| self.cmp_time(a, b));
        out
    }

    fn matches(&self, id: u32, q: &Query, from_ns: Option<i64>, to_ns: Option<i64>) -> bool {
        let Some(rec) = self.index.record(id) else {
            return false;
        };
        if let Some(from) = from_ns {
            if rec.ts_ns <= 0 || rec.ts_ns < from {
                return false;
            }
        }
        if let Some(to) = to_ns {
            if rec.ts_ns <= 0 || rec.ts_ns > to {
                return false;
            }
        }
        for pred in &q.preds {
            if !self.pred_ok(id, &rec, pred) {
                return false;
            }
        }
        true
    }

    fn pred_ok(&self, id: u32, rec: &Record, pred: &Pred) -> bool {
        match pred {
            Pred::Level { cmp, level, neg } => {
                let ok = level_cmp(rec.level, *cmp, *level);
                if *neg {
                    !ok
                } else {
                    ok
                }
            }
            Pred::Field { key, op, neg } => {
                let ok = self.index.facet_match(id, key, |v| field_ok(v, op));
                if *neg {
                    !ok
                } else {
                    ok
                }
            }
            Pred::Text { needle, neg } => {
                let ok = self.text_contains(id, needle);
                if *neg {
                    !ok
                } else {
                    ok
                }
            }
            Pred::Regex { re, neg } => {
                let ok = self.text_regex(id, re);
                if *neg {
                    !ok
                } else {
                    ok
                }
            }
            Pred::Template { id: tid, neg } => {
                let ok = rec.template_id == *tid;
                if *neg {
                    !ok
                } else {
                    ok
                }
            }
            Pred::TimeGte(ns) => rec.ts_ns > 0 && rec.ts_ns >= *ns,
            Pred::TimeLte(ns) => rec.ts_ns > 0 && rec.ts_ns <= *ns,
        }
    }

    fn text_contains(&self, id: u32, needle: &str) -> bool {
        let Some(text) = self.event_text(id) else {
            return false;
        };
        contains_ignore_ascii(text.as_ref(), needle)
    }

    fn text_regex(&self, id: u32, re: &regex::Regex) -> bool {
        let Some(text) = self.event_text(id) else {
            return false;
        };
        re.is_match(text.as_ref())
    }

    fn event_text(&self, id: u32) -> Option<std::borrow::Cow<'_, str>> {
        let raw = self.index.raw_bytes(id)?;
        let text = String::from_utf8_lossy(raw);
        if raw.contains(&0x1b) {
            Some(std::borrow::Cow::Owned(parser::strip_ansi(&text)))
        } else {
            Some(text)
        }
    }

    fn cmp_time(&self, a: u32, b: u32) -> Ordering {
        let ra = self.index.record(a);
        let rb = self.index.record(b);
        match (ra, rb) {
            (Some(a), Some(b)) => match (a.ts_ns > 0, b.ts_ns > 0) {
                (true, true) => a
                    .ts_ns
                    .cmp(&b.ts_ns)
                    .then(a.file_id.cmp(&b.file_id))
                    .then(a.offset.cmp(&b.offset)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => a.file_id.cmp(&b.file_id).then(a.offset.cmp(&b.offset)),
            },
            _ => Ordering::Equal,
        }
    }

    pub fn preview(&self, id: u32) -> Option<EventPreview> {
        let rec = self.index.record(id)?;
        let file = self.index.file(rec.file_id);
        let facets = self.index.facets_of(id);
        let message = self.display_message(id, &rec);
        let template = self
            .drain
            .clusters
            .get(rec.template_id as usize)
            .map(|c| c.template.clone())
            .unwrap_or_default();
        let mut map = BTreeMap::new();
        for (k, v) in &facets {
            if k == "stack" {
                continue;
            }
            map.insert(k.clone(), v.display());
        }
        Some(EventPreview {
            id,
            file_id: rec.file_id,
            offset: rec.offset,
            len: rec.len,
            line: rec.line,
            span_lines: rec.span_lines,
            ts_ns: rec.ts_ns,
            ts: format_ns(rec.ts_ns),
            level: rec.level.as_str().to_string(),
            source: map
                .get("source")
                .cloned()
                .unwrap_or_else(|| file.origin.name.clone()),
            host: map
                .get("host")
                .cloned()
                .unwrap_or_else(|| file.origin.host.clone()),
            service: map.get("service").cloned().unwrap_or_default(),
            message,
            template_id: rec.template_id,
            template,
            facets: map,
            format: file.origin.kind.as_str().to_string(),
        })
    }

    fn display_message(&self, id: u32, rec: &Record) -> String {
        let Some(raw) = self.index.raw_bytes(id) else {
            return String::new();
        };
        let text = String::from_utf8_lossy(raw);
        let stripped = if raw.contains(&0x1b) {
            parser::strip_ansi(&text)
        } else {
            text.into_owned()
        };
        let mut lines = stripped.split('\n');
        let first = lines
            .next()
            .unwrap_or("")
            .trim_end_matches('\r')
            .to_string();
        let rest = lines.collect::<Vec<_>>().join("\n");
        let origin = &self.index.file(rec.file_id).origin;
        parser::interpret(&first, &rest, origin).message
    }

    pub fn summary(&self, q: &str) -> Result<Summary> {
        let query = if q.trim().is_empty() {
            None
        } else {
            Some(query::parse(q, self.index.max_ts)?)
        };
        Ok(self.summary_filtered(query.as_ref()))
    }

    pub fn summary_filtered(&self, q: Option<&Query>) -> Summary {
        let ids = if let Some(q) = q {
            self.search(q, None, None)
        } else {
            (0..self.index.event_count()).collect()
        };
        self.summarize(&ids)
    }

    fn summarize(&self, ids: &[u32]) -> Summary {
        let mut levels = [0u64; 7];
        let mut tmin = 0i64;
        let mut tmax = 0i64;
        let mut by_tpl: HashMap<u32, Acc> = HashMap::new();
        for &id in ids {
            let Some(rec) = self.index.record(id) else {
                continue;
            };
            levels[rec.level as usize] += 1;
            if rec.ts_ns > 0 {
                if tmin == 0 || rec.ts_ns < tmin {
                    tmin = rec.ts_ns;
                }
                if rec.ts_ns > tmax {
                    tmax = rec.ts_ns;
                }
            }
            let acc = by_tpl.entry(rec.template_id).or_default();
            acc.count += 1;
            acc.levels[rec.level as usize] += 1;
            if rec.level.is_error() {
                acc.errors += 1;
            }
        }
        let mut top: Vec<TemplateBrief> = by_tpl
            .into_iter()
            .filter_map(|(tid, acc)| {
                if acc.errors == 0 {
                    return None;
                }
                let template = self
                    .drain
                    .clusters
                    .get(tid as usize)
                    .map(|c| c.template.clone())
                    .unwrap_or_else(|| format!("#{tid}"));
                Some(TemplateBrief {
                    id: tid,
                    template,
                    count: acc.count,
                    error_count: acc.errors,
                })
            })
            .collect();
        top.sort_by(|a, b| {
            b.error_count
                .cmp(&a.error_count)
                .then(a.template.cmp(&b.template))
        });
        top.truncate(10);
        let anomalies = self.anomaly_lines();
        let sources = self
            .index
            .files()
            .iter()
            .map(|f| f.origin.name.clone())
            .collect();
        Summary {
            total: ids.len(),
            files: self.index.files().len(),
            payload_bytes: self.index.payload_bytes(),
            index_bytes: self.index.sparse_index_bytes(),
            tmin_ns: (tmin > 0).then_some(tmin),
            tmax_ns: (tmax > 0).then_some(tmax),
            levels: level_map(&levels),
            top_error_templates: top,
            anomalies,
            sources,
        }
    }

    fn anomaly_lines(&self) -> Vec<String> {
        self.drain
            .clusters
            .iter()
            .filter_map(|c| c.spike.as_ref().map(|s| format!("{} — {s}", c.template)))
            .take(8)
            .collect()
    }

    pub fn summary_text(&self, q: &str, max_tokens: usize) -> Result<String> {
        let summary = self.summary(q)?;
        Ok(render_summary(&summary, max_tokens))
    }

    pub fn patterns(&self, q: &str, sort: &str) -> Result<Vec<PatternRow>> {
        let ids = if q.trim().is_empty() {
            (0..self.index.event_count()).collect::<Vec<_>>()
        } else {
            self.query(q)?
        };
        let mut acc: HashMap<u32, PatAcc> = HashMap::new();
        for id in ids {
            let Some(rec) = self.index.record(id) else {
                continue;
            };
            let row = acc.entry(rec.template_id).or_default();
            row.count += 1;
            row.levels[rec.level as usize] += 1;
            if rec.ts_ns > 0 {
                if row.first_ns == 0 || rec.ts_ns < row.first_ns {
                    row.first_ns = rec.ts_ns;
                }
                if rec.ts_ns > row.last_ns {
                    row.last_ns = rec.ts_ns;
                }
                let bucket = rec.ts_ns - rec.ts_ns.rem_euclid(60_000_000_000);
                if let Some(last) = row.buckets.last_mut() {
                    if *last == bucket {
                        // counted below
                    }
                }
                if row.buckets.last().copied() != Some(bucket) {
                    row.buckets.push(bucket);
                    row.bucket_n.push(0);
                }
                if let Some(n) = row.bucket_n.last_mut() {
                    *n += 1;
                }
            }
        }
        let mut rows = Vec::new();
        for (tid, row) in acc {
            let template = self
                .drain
                .clusters
                .get(tid as usize)
                .map(|c| c.template.clone())
                .unwrap_or_else(|| format!("#{tid}"));
            let spike = self
                .drain
                .clusters
                .get(tid as usize)
                .and_then(|c| c.spike.clone());
            let buckets: Vec<(i64, u32, u32)> = row
                .buckets
                .iter()
                .zip(row.bucket_n.iter())
                .map(|(b, n)| (*b, *n, 0))
                .collect();
            rows.push(PatternRow {
                id: tid,
                template,
                count: row.count,
                levels: level_map(&row.levels),
                first: format_ns(row.first_ns),
                last: format_ns(row.last_ns),
                spike,
                spark: drain::sparkline(&buckets, 16),
            });
        }
        match sort {
            "spike" => rows.sort_by(|a, b| {
                let as_ = u8::from(a.spike.is_some());
                let bs = u8::from(b.spike.is_some());
                bs.cmp(&as_)
                    .then(b.count.cmp(&a.count))
                    .then(a.template.cmp(&b.template))
            }),
            _ => rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.template.cmp(&b.template))),
        }
        Ok(rows)
    }

    pub fn context(
        &self,
        id: Option<u32>,
        file_id: Option<u32>,
        offset: Option<u64>,
        line: Option<u32>,
        n: u32,
    ) -> Result<ContextView> {
        let id = if let Some(id) = id {
            id
        } else {
            let file_id = file_id.ok_or_else(|| anyhow!("file_id is required"))?;
            if let Some(off) = offset {
                self.index
                    .find_by_offset(file_id, off)
                    .ok_or_else(|| anyhow!("no event at offset {off}"))?
            } else if let Some(line) = line {
                self.index
                    .find_by_line(file_id, line)
                    .ok_or_else(|| anyhow!("no event at line {line}"))?
            } else {
                return Err(anyhow!("pass id, or file_id plus offset or line"));
            }
        };
        let rec = self
            .index
            .record(id)
            .ok_or_else(|| anyhow!("unknown event {id}"))?;
        let file = self.index.file(rec.file_id);
        let lines = physical_lines(file.bytes.as_slice());
        let start = rec.line.saturating_sub(1) as usize;
        let span = rec.span_lines.max(1) as usize;
        let end = (start + span).min(lines.len());
        let n = n as usize;
        let before_from = start.saturating_sub(n);
        let after_to = (end + n).min(lines.len());
        let before = lines[before_from..start].to_vec();
        let after = lines[end..after_to].to_vec();
        let event_lines = if start < lines.len() {
            lines[start..end.min(lines.len())].join("\n")
        } else {
            String::new()
        };
        let stack = if event_lines.contains('\n') {
            event_lines
                .split_once('\n')
                .map(|(_, r)| r.to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let preview = self.preview(id).ok_or_else(|| anyhow!("missing event"))?;
        Ok(ContextView {
            event: preview,
            before,
            after,
            stack,
            event_text: event_lines,
        })
    }

    pub fn trace(&self, id: &str) -> Vec<EventPreview> {
        let mut ids: Vec<u32> = self.index.correlation(id).to_vec();
        if ids.is_empty() {
            // Fall back to a facet equality scan across the well-known keys.
            for key in [
                "req_id",
                "request_id",
                "trace_id",
                "span_id",
                "correlation_id",
                "requestId",
                "traceId",
            ] {
                let q = Query {
                    preds: vec![Pred::Field {
                        key: key.into(),
                        op: FieldOp::Eq(id.to_string()),
                        neg: false,
                    }],
                };
                ids.extend(self.search(&q, None, None));
            }
            ids.sort_unstable();
            ids.dedup();
        }
        ids.sort_by(|&a, &b| self.cmp_time(a, b));
        ids.into_iter().filter_map(|id| self.preview(id)).collect()
    }

    pub fn histogram(&self, q: &str, buckets: usize) -> Result<Histogram> {
        let ids = if q.trim().is_empty() {
            (0..self.index.event_count()).collect::<Vec<_>>()
        } else {
            self.query(q)?
        };
        Ok(self.histogram_ids(&ids, buckets.max(1)))
    }

    pub fn histogram_ids(&self, ids: &[u32], buckets: usize) -> Histogram {
        let mut tmin = 0i64;
        let mut tmax = 0i64;
        let mut timed = Vec::new();
        let mut info = 0u64;
        let mut warn = 0u64;
        let mut error = 0u64;
        for &id in ids {
            let Some(rec) = self.index.record(id) else {
                continue;
            };
            match rec.level.band() {
                "warn" => warn += 1,
                "error" => error += 1,
                _ => info += 1,
            }
            if rec.ts_ns > 0 {
                if tmin == 0 || rec.ts_ns < tmin {
                    tmin = rec.ts_ns;
                }
                if rec.ts_ns > tmax {
                    tmax = rec.ts_ns;
                }
                timed.push(rec);
            }
        }
        if timed.is_empty() || tmax <= tmin {
            return Histogram {
                tmin_ns: (tmin > 0).then_some(tmin),
                tmax_ns: (tmax > 0).then_some(tmax),
                buckets: vec![HistBucket {
                    start_ns: tmin,
                    end_ns: tmax,
                    info,
                    warn,
                    error,
                }],
            };
        }
        let buckets = buckets.max(1);
        let span = (tmax - tmin).max(1);
        let mut rows = vec![
            HistBucket {
                start_ns: 0,
                end_ns: 0,
                info: 0,
                warn: 0,
                error: 0
            };
            buckets
        ];
        for (i, row) in rows.iter_mut().enumerate() {
            let a = tmin + span * i as i64 / buckets as i64;
            let b = if i + 1 == buckets {
                tmax
            } else {
                tmin + span * (i as i64 + 1) / buckets as i64
            };
            row.start_ns = a;
            row.end_ns = b;
        }
        for rec in timed {
            let mut idx = ((rec.ts_ns - tmin) as i128 * buckets as i128 / span as i128) as usize;
            if idx >= buckets {
                idx = buckets - 1;
            }
            match rec.level.band() {
                "warn" => rows[idx].warn += 1,
                "error" => rows[idx].error += 1,
                _ => rows[idx].info += 1,
            }
        }
        Histogram {
            tmin_ns: Some(tmin),
            tmax_ns: Some(tmax),
            buckets: rows,
        }
    }

    pub fn query_page(&self, req: PageQuery<'_>) -> Result<QueryPage> {
        let parsed = if req.query.trim().is_empty() {
            Query { preds: Vec::new() }
        } else {
            query::parse(req.query, self.index.max_ts)?
        };
        let ids = self.search(&parsed, req.from_ns, req.to_ns);
        let total = ids.len();
        let mut events = Vec::new();
        let mut seen = HashSet::new();
        let mut tokens = 0usize;
        let mut skipped = 0usize;
        let mut truncated = false;
        for id in ids {
            let Some(preview) = self.preview(id) else {
                continue;
            };
            if req.dedup {
                let key = (preview.template_id, preview.message.clone());
                if !seen.insert(key) {
                    continue;
                }
            }
            let cost = estimate_tokens(&preview.message) + 8;
            if skipped < req.offset {
                skipped += 1;
                continue;
            }
            if events.len() >= req.limit {
                truncated = true;
                break;
            }
            if let Some(budget) = req.token_budget {
                if !events.is_empty() && tokens + cost > budget {
                    truncated = true;
                    break;
                }
            }
            tokens += cost;
            events.push(preview);
        }
        if !req.dedup && req.offset + events.len() < total {
            truncated = truncated || events.len() >= req.limit;
        }
        Ok(QueryPage {
            total,
            offset: req.offset,
            limit: req.limit,
            truncated,
            estimated_tokens: tokens,
            events,
        })
    }

    pub fn facets_for(&self, q: &str, limit: usize) -> Result<Vec<FacetCount>> {
        if q.trim().is_empty() {
            return Ok(self
                .index
                .global_facets(limit)
                .into_iter()
                .map(|(key, values)| FacetCount { key, values })
                .collect());
        }
        let ids = self.query(q)?;
        let mut counts: HashMap<String, HashMap<String, u64>> = HashMap::new();
        for id in ids {
            for (k, v) in self.index.facets_of(id) {
                if k == "stack" || k == "format_detail" {
                    continue;
                }
                let display = v.display();
                let bucket = counts.entry(k).or_default();
                if let Some(n) = bucket.get_mut(&display) {
                    *n += 1;
                } else if bucket.len() < 200 {
                    bucket.insert(display, 1);
                }
            }
        }
        let mut keys: Vec<String> = counts.keys().cloned().collect();
        keys.sort_by(|a, b| facet_rank(a).cmp(&facet_rank(b)).then(a.cmp(b)));
        let mut out = Vec::new();
        for key in keys {
            let mut values: Vec<(String, u64)> = counts
                .remove(&key)
                .unwrap_or_default()
                .into_iter()
                .collect();
            values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            values.truncate(limit);
            out.push(FacetCount { key, values });
        }
        Ok(out)
    }
}

fn facet_rank(name: &str) -> u8 {
    match name {
        "level" => 0,
        "service" => 1,
        "source" => 2,
        "status" => 3,
        "host" => 4,
        _ => 9,
    }
}

fn field_ok(v: &FacetValue, op: &FieldOp) -> bool {
    match op {
        FieldOp::Eq(expected) => v.eq_token(expected),
        FieldOp::Num(nop, rhs) => v.as_f64().is_some_and(|n| nop.test(n, *rhs)),
        FieldOp::Range(lo, hi) => v.as_f64().is_some_and(|n| n >= *lo && n <= *hi),
    }
}

fn level_cmp(actual: Level, cmp: query::Cmp, expected: Level) -> bool {
    match cmp {
        query::Cmp::Eq => actual == expected,
        query::Cmp::Gt => actual != Level::Unknown && actual.rank() > expected.rank(),
        query::Cmp::Gte => actual != Level::Unknown && actual.rank() >= expected.rank(),
        query::Cmp::Lt => actual != Level::Unknown && actual.rank() < expected.rank(),
        query::Cmp::Lte => actual != Level::Unknown && actual.rank() <= expected.rank(),
    }
}

fn contains_ignore_ascii(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    let last = h.len() - n.len();
    for i in 0..=last {
        if h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
    }
    false
}

fn physical_lines(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let stripped = if data.contains(&0x1b) {
        parser::strip_ansi(&text)
    } else {
        text.into_owned()
    };
    stripped
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect()
}

fn level_map(levels: &[u64; 7]) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for (i, n) in levels.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let name = level_from_u8(i as u8).as_str().to_string();
        map.insert(name, *n);
    }
    map
}

fn level_from_u8(v: u8) -> Level {
    match v {
        1 => Level::Trace,
        2 => Level::Debug,
        3 => Level::Info,
        4 => Level::Warn,
        5 => Level::Error,
        6 => Level::Fatal,
        _ => Level::Unknown,
    }
}

pub fn render_summary(summary: &Summary, max_tokens: usize) -> String {
    let span = match (
        summary.tmin_ns.and_then(format_ns),
        summary.tmax_ns.and_then(format_ns),
    ) {
        (Some(a), Some(b)) => format!("{a} .. {b}"),
        _ => "n/a".into(),
    };
    let levels = summary
        .levels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![
        format!("events {}", summary.total),
        format!("span {span}"),
        format!("levels {levels}"),
        format!(
            "sources {}",
            summary
                .sources
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!(
            "index {} bytes / payload {} bytes",
            summary.index_bytes, summary.payload_bytes
        ),
        "top_error_templates:".into(),
    ];
    if summary.top_error_templates.is_empty() {
        lines.push("  (none)".into());
    }
    for (i, t) in summary.top_error_templates.iter().enumerate() {
        lines.push(format!("  {}. ({}) {}", i + 1, t.error_count, t.template));
    }
    lines.push("anomalies:".into());
    if summary.anomalies.is_empty() {
        lines.push("  (none)".into());
    } else {
        for a in &summary.anomalies {
            lines.push(format!("  - {a}"));
        }
    }
    // Drop template/anomaly lines from the bottom until the token budget fits.
    while estimate_tokens(&lines.join("\n")) > max_tokens && lines.len() > 6 {
        lines.pop();
    }
    let mut text = lines.join("\n");
    if estimate_tokens(&text) > max_tokens {
        let bytes = max_tokens.saturating_mul(4);
        let mut end = bytes.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[derive(Debug, Default)]
struct Acc {
    count: u64,
    errors: u64,
    levels: [u64; 7],
}

#[derive(Default)]
struct PatAcc {
    count: u64,
    levels: [u64; 7],
    first_ns: i64,
    last_ns: i64,
    buckets: Vec<i64>,
    bucket_n: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventPreview {
    pub id: u32,
    pub file_id: u32,
    pub offset: u64,
    pub len: u32,
    pub line: u32,
    pub span_lines: u16,
    pub ts_ns: i64,
    pub ts: Option<String>,
    pub level: String,
    pub source: String,
    pub host: String,
    pub service: String,
    pub message: String,
    pub template_id: u32,
    pub template: String,
    pub facets: BTreeMap<String, String>,
    pub format: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub total: usize,
    pub files: usize,
    pub payload_bytes: u64,
    pub index_bytes: u64,
    pub tmin_ns: Option<i64>,
    pub tmax_ns: Option<i64>,
    pub levels: BTreeMap<String, u64>,
    pub top_error_templates: Vec<TemplateBrief>,
    pub anomalies: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateBrief {
    pub id: u32,
    pub template: String,
    pub count: u64,
    pub error_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatternRow {
    pub id: u32,
    pub template: String,
    pub count: u64,
    pub levels: BTreeMap<String, u64>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub spike: Option<String>,
    pub spark: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistBucket {
    pub start_ns: i64,
    pub end_ns: i64,
    pub info: u64,
    pub warn: u64,
    pub error: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Histogram {
    pub tmin_ns: Option<i64>,
    pub tmax_ns: Option<i64>,
    pub buckets: Vec<HistBucket>,
}

/// A bounded page of query hits. MCP sets `token_budget` and `dedup`.
pub struct PageQuery<'a> {
    pub query: &'a str,
    pub from_ns: Option<i64>,
    pub to_ns: Option<i64>,
    pub offset: usize,
    pub limit: usize,
    pub token_budget: Option<usize>,
    pub dedup: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryPage {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub truncated: bool,
    pub estimated_tokens: usize,
    pub events: Vec<EventPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FacetCount {
    pub key: String,
    pub values: Vec<(String, u64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextView {
    pub event: EventPreview,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub stack: String,
    pub event_text: String,
}

fn is_compressed(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".gz") || name.ends_with(".zst") || name.ends_with(".zstd")
}

fn read_compressed(path: &Path) -> Result<FileBytes> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = Vec::new();
    if name.ends_with(".gz") {
        let mut dec = flate2::read::GzDecoder::new(file);
        dec.read_to_end(&mut buf)
            .with_context(|| format!("gunzip {}", path.display()))?;
    } else {
        let mut dec =
            zstd::Decoder::new(file).with_context(|| format!("zstd {}", path.display()))?;
        dec.read_to_end(&mut buf)
            .with_context(|| format!("unzstd {}", path.display()))?;
    }
    Ok(FileBytes::Buf(buf))
}

fn map_file(path: &Path, len: u64) -> Result<FileBytes> {
    if len == 0 {
        return Ok(FileBytes::Buf(Vec::new()));
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: read-only map of a local log. Follow mode uses a Vec instead so
    // appends do not race a live mapping. Callers do not mutate the file
    // through this map.
    let map = unsafe { memmap2::MmapOptions::new().map(&file)? };
    Ok(FileBytes::Map(map))
}

fn file_inode(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Load every source named by the CLI, in batch mode.
pub fn load_batch(opts: &LoadRequest) -> Result<Engine> {
    let mut engine = Engine::new();
    if opts.read_stdin {
        engine.ingest_stdin()?;
    }
    engine.ingest_paths(&opts.paths, false)?;
    for spec in &opts.ssh {
        let host = crate::sources::ssh_host(spec)?;
        for (name, bytes) in crate::sources::read_ssh(spec)? {
            let mut origin = Origin::named(SourceKind::Ssh, name, "");
            origin.host = host.clone();
            engine.ingest_bytes(origin, bytes)?;
        }
    }
    for container in &opts.docker {
        let bytes = crate::sources::read_docker(container, opts.remote_lines)?;
        engine.ingest_bytes(
            Origin::named(SourceKind::Docker, format!("docker:{container}"), container),
            bytes,
        )?;
    }
    for unit in &opts.journald {
        let bytes = crate::sources::read_journal(unit, opts.remote_lines)?;
        engine.ingest_bytes(
            Origin::named(SourceKind::Journald, format!("journal:{unit}"), unit),
            bytes,
        )?;
    }
    Ok(engine)
}

#[derive(Debug, Clone)]
pub struct LoadRequest {
    pub paths: Vec<String>,
    pub ssh: Vec<String>,
    pub docker: Vec<String>,
    pub journald: Vec<String>,
    pub read_stdin: bool,
    pub remote_lines: usize,
}

impl LoadRequest {
    pub fn paths_only(paths: Vec<String>) -> Self {
        Self {
            paths,
            ssh: Vec::new(),
            docker: Vec::new(),
            journald: Vec::new(),
            read_stdin: false,
            remote_lines: 5_000,
        }
    }
}

pub fn followed_paths(paths: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        if p == "-" {
            continue;
        }
        for path in crate::sources::expand_input(p)? {
            if path.is_file() && !is_compressed(&path) {
                out.push(path);
            }
        }
    }
    Ok(out)
}
