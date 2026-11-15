//! Sparse block-offset index.
//!
//! Each event is a 32-byte packed record inside a block:
//! `(timestamp delta, file-relative offset, length, line, template id, level)`.
//! Raw payloads are not copied. Facet values live in a side store so numeric
//! filters do not have to re-parse the file. A per-file posting list supports
//! offset and line lookup when follow-mode appends are interleaved with other
//! sources.

use crate::parser::{ParsedEvent, RawGroup};
use crate::types::{is_correlation_key, FacetValue, Level};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::PathBuf;

pub const BLOCK_LINES: usize = 128;

/// On-disk-equivalent size of one sparse index record.
pub fn sparse_record_size() -> usize {
    std::mem::size_of::<Packed>()
}

const _: () = assert!(std::mem::size_of::<Packed>() == 32);

#[repr(C)]
#[derive(Clone, Copy)]
struct Packed {
    ts_delta_ns: i64,
    offset_delta: u32,
    byte_len: u32,
    line_delta: u32,
    template_id: u32,
    span_lines: u16,
    level: u8,
    _pad: u8,
}

struct Block {
    file_id: u32,
    base_offset: u64,
    base_line: u32,
    base_ts_ns: i64,
    entries: Vec<Packed>,
}

#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub id: u32,
    pub file_id: u32,
    pub offset: u64,
    pub len: u32,
    pub line: u32,
    pub span_lines: u16,
    pub ts_ns: i64,
    pub level: Level,
    pub template_id: u32,
}

enum PackedVal {
    Str(u32),
    Int(i64),
    Float(u64),
    Bool(bool),
}

struct FacetStore {
    keys: Vec<u16>,
    vals: Vec<PackedVal>,
    /// `ranges[id]` is the start index; the end is `ranges[id + 1]`.
    ranges: Vec<u32>,
}

impl FacetStore {
    fn new() -> Self {
        Self {
            keys: Vec::new(),
            vals: Vec::new(),
            ranges: vec![0],
        }
    }

    fn push(&mut self, pairs: &[(u16, PackedVal)]) {
        for (k, v) in pairs {
            self.keys.push(*k);
            self.vals.push(match v {
                PackedVal::Str(s) => PackedVal::Str(*s),
                PackedVal::Int(i) => PackedVal::Int(*i),
                PackedVal::Float(f) => PackedVal::Float(*f),
                PackedVal::Bool(b) => PackedVal::Bool(*b),
            });
        }
        self.ranges.push(self.keys.len() as u32);
    }
}

pub struct Interner {
    values: Vec<String>,
    to_id: HashMap<String, u32>,
}

impl Interner {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            to_id: HashMap::new(),
        }
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(id) = self.to_id.get(s) {
            return *id;
        }
        let id = self.values.len() as u32;
        self.values.push(s.to_string());
        self.to_id.insert(s.to_string(), id);
        id
    }

    pub fn resolve(&self, id: u32) -> &str {
        &self.values[id as usize]
    }
}

pub enum FileBytes {
    Map(Mmap),
    Buf(Vec<u8>),
}

impl FileBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Map(m) => m,
            Self::Buf(b) => b,
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn extend_buf(&mut self, extra: &[u8]) -> bool {
        match self {
            Self::Buf(b) => {
                b.extend_from_slice(extra);
                true
            }
            Self::Map(_) => false,
        }
    }
}

pub struct IndexedFile {
    pub origin: crate::types::Origin,
    pub bytes: FileBytes,
    pub path: Option<PathBuf>,
    pub inode: Option<u64>,
    pub indexed_upto: u64,
    /// Next 1-based line number, counting every physical line already consumed.
    pub next_line: u32,
    pub has_ansi: bool,
    pub events: Vec<u32>,
}

pub struct LogIndex {
    files: Vec<IndexedFile>,
    blocks: Vec<Block>,
    block_first: Vec<u32>,
    facets: FacetStore,
    intern: Interner,
    key_ids: HashMap<String, u16>,
    /// key id -> display value -> count. Capped per key.
    facet_counts: HashMap<u16, HashMap<String, u64>>,
    correlation: HashMap<String, Vec<u32>>,
    pub level_counts: [u64; 7],
    pub min_ts: i64,
    pub max_ts: i64,
    pub events_with_ts: u64,
}

impl LogIndex {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            blocks: Vec::new(),
            block_first: Vec::new(),
            facets: FacetStore::new(),
            intern: Interner::new(),
            key_ids: HashMap::new(),
            facet_counts: HashMap::new(),
            correlation: HashMap::new(),
            level_counts: [0; 7],
            min_ts: 0,
            max_ts: 0,
            events_with_ts: 0,
        }
    }

    pub fn add_file(&mut self, file: IndexedFile) -> u32 {
        let id = self.files.len() as u32;
        self.files.push(file);
        id
    }

    pub fn file(&self, id: u32) -> &IndexedFile {
        &self.files[id as usize]
    }

    pub fn file_mut(&mut self, id: u32) -> &mut IndexedFile {
        &mut self.files[id as usize]
    }

    pub fn files(&self) -> &[IndexedFile] {
        &self.files
    }

    pub fn len(&self) -> u32 {
        self.block_first
            .last()
            .map(|_| self.event_count())
            .unwrap_or(0)
    }

    pub fn event_count(&self) -> u32 {
        self.blocks.iter().map(|b| b.entries.len() as u32).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.event_count() == 0
    }

    pub fn append(
        &mut self,
        file_id: u32,
        group: &RawGroup,
        parsed: &ParsedEvent,
        template_id: u32,
    ) -> u32 {
        let ts = parsed.ts_ns;
        let level = parsed.level;
        let id = self.event_count();
        self.push_packed(file_id, group, ts, level, template_id);
        let pairs = self.pack_facets(&parsed.facets);
        self.observe_facets(id, &parsed.facets);
        self.facets.push(&pairs);
        self.files[file_id as usize].events.push(id);
        self.level_counts[level as usize] += 1;
        if ts > 0 {
            if self.min_ts == 0 || ts < self.min_ts {
                self.min_ts = ts;
            }
            if ts > self.max_ts {
                self.max_ts = ts;
            }
            self.events_with_ts += 1;
        }
        id
    }

    /// Set the next line number from how many newlines were consumed.
    pub fn set_progress(&mut self, file_id: u32, indexed_upto: u64) {
        let file = &mut self.files[file_id as usize];
        let bytes = file.bytes.as_slice();
        let end = (indexed_upto as usize).min(bytes.len());
        let newlines = bytes[..end].iter().filter(|b| **b == b'\n').count() as u32;
        file.next_line = newlines.saturating_add(1);
        file.indexed_upto = indexed_upto;
    }

    fn push_packed(
        &mut self,
        file_id: u32,
        group: &RawGroup,
        ts_ns: i64,
        level: Level,
        template_id: u32,
    ) {
        let offset = group.offset;
        let line = group.line;
        let need_new = match self.blocks.last() {
            None => true,
            Some(b) => {
                b.file_id != file_id
                    || b.entries.len() >= BLOCK_LINES
                    || offset < b.base_offset
                    || offset - b.base_offset > u32::MAX as u64
                    || line < b.base_line
            }
        };
        if need_new {
            self.block_first.push(self.event_count());
            self.blocks.push(Block {
                file_id,
                base_offset: offset,
                base_line: line,
                base_ts_ns: ts_ns,
                entries: Vec::with_capacity(BLOCK_LINES.min(32)),
            });
        }
        let block = self.blocks.last_mut().expect("block");
        let packed = Packed {
            ts_delta_ns: ts_ns.saturating_sub(block.base_ts_ns),
            offset_delta: (offset - block.base_offset) as u32,
            byte_len: group.len,
            line_delta: line.saturating_sub(block.base_line),
            template_id,
            span_lines: group.span_lines.max(1),
            level: level as u8,
            _pad: 0,
        };
        block.entries.push(packed);
    }

    fn pack_facets(&mut self, facets: &[(String, FacetValue)]) -> Vec<(u16, PackedVal)> {
        let mut out = Vec::with_capacity(facets.len());
        for (k, v) in facets {
            let kid = self.key_id(k);
            let pv = match v {
                FacetValue::Str(s) => PackedVal::Str(self.intern.intern(s)),
                FacetValue::Int(i) => PackedVal::Int(*i),
                FacetValue::Float(f) => PackedVal::Float(f.to_bits()),
                FacetValue::Bool(b) => PackedVal::Bool(*b),
            };
            out.push((kid, pv));
        }
        out
    }

    fn key_id(&mut self, key: &str) -> u16 {
        if let Some(id) = self.key_ids.get(key) {
            return *id;
        }
        let id = self.key_ids.len() as u16;
        self.key_ids.insert(key.to_string(), id);
        id
    }

    fn key_name(&self, id: u16) -> Option<&str> {
        self.key_ids
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.as_str())
    }

    fn observe_facets(&mut self, event_id: u32, facets: &[(String, FacetValue)]) {
        for (k, v) in facets {
            if is_correlation_key(k) {
                let display = v.display();
                if !display.is_empty() && display != "null" {
                    self.correlation.entry(display).or_default().push(event_id);
                }
            }
            let kid = *self.key_ids.get(k).expect("key interned");
            let display = v.display();
            let bucket = self.facet_counts.entry(kid).or_default();
            if let Some(n) = bucket.get_mut(&display) {
                *n += 1;
            } else if bucket.len() < 200 {
                bucket.insert(display, 1);
            }
        }
    }

    pub fn record(&self, id: u32) -> Option<Record> {
        let (bi, slot) = self.locate(id)?;
        let block = &self.blocks[bi];
        let p = &block.entries[slot];
        Some(Record {
            id,
            file_id: block.file_id,
            offset: block.base_offset + u64::from(p.offset_delta),
            len: p.byte_len,
            line: block.base_line + p.line_delta,
            span_lines: p.span_lines,
            ts_ns: block.base_ts_ns.saturating_add(p.ts_delta_ns),
            level: level_from_u8(p.level),
            template_id: p.template_id,
        })
    }

    fn locate(&self, id: u32) -> Option<(usize, usize)> {
        if self.block_first.is_empty() || id >= self.event_count() {
            return None;
        }
        let bi = self.block_first.partition_point(|start| *start <= id);
        let bi = bi.checked_sub(1)?;
        let slot = (id - self.block_first[bi]) as usize;
        if slot < self.blocks[bi].entries.len() {
            Some((bi, slot))
        } else {
            None
        }
    }

    pub fn raw_bytes(&self, id: u32) -> Option<&[u8]> {
        let rec = self.record(id)?;
        let bytes = self.files.get(rec.file_id as usize)?.bytes.as_slice();
        let start = rec.offset as usize;
        let end = start.saturating_add(rec.len as usize).min(bytes.len());
        if start > bytes.len() || start > end {
            return None;
        }
        Some(&bytes[start..end])
    }

    pub fn facets_of(&self, id: u32) -> Vec<(String, FacetValue)> {
        let Some((start, end)) = self.facet_range(id) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(end - start);
        for i in start..end {
            let key = self.key_name(self.facets.keys[i]).unwrap_or("").to_string();
            let value = match &self.facets.vals[i] {
                PackedVal::Str(sid) => FacetValue::Str(self.intern.resolve(*sid).to_string()),
                PackedVal::Int(n) => FacetValue::Int(*n),
                PackedVal::Float(bits) => FacetValue::Float(f64::from_bits(*bits)),
                PackedVal::Bool(b) => FacetValue::Bool(*b),
            };
            out.push((key, value));
        }
        out
    }

    pub fn facet_match(&self, id: u32, key: &str, test: impl Fn(&FacetValue) -> bool) -> bool {
        let Some(kid) = self.key_ids.get(key).copied() else {
            return false;
        };
        let Some((start, end)) = self.facet_range(id) else {
            return false;
        };
        for i in start..end {
            if self.facets.keys[i] != kid {
                continue;
            }
            let hit = match &self.facets.vals[i] {
                PackedVal::Str(sid) => {
                    test(&FacetValue::Str(self.intern.resolve(*sid).to_string()))
                }
                PackedVal::Int(n) => test(&FacetValue::Int(*n)),
                PackedVal::Float(bits) => test(&FacetValue::Float(f64::from_bits(*bits))),
                PackedVal::Bool(b) => test(&FacetValue::Bool(*b)),
            };
            if hit {
                return true;
            }
        }
        false
    }

    fn facet_range(&self, id: u32) -> Option<(usize, usize)> {
        let start = *self.facets.ranges.get(id as usize)? as usize;
        let end = *self.facets.ranges.get(id as usize + 1)? as usize;
        Some((start, end))
    }

    pub fn correlation(&self, id: &str) -> &[u32] {
        self.correlation.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn find_by_offset(&self, file_id: u32, offset: u64) -> Option<u32> {
        let events = &self.files.get(file_id as usize)?.events;
        let idx = events.partition_point(|&id| {
            self.record(id)
                .map(|r| r.offset + u64::from(r.len) <= offset)
                .unwrap_or(false)
        });
        let id = *events.get(idx)?;
        let rec = self.record(id)?;
        if offset >= rec.offset && offset < rec.offset + u64::from(rec.len) {
            Some(id)
        } else if idx > 0 {
            let prev = events[idx - 1];
            let rec = self.record(prev)?;
            if offset >= rec.offset && offset < rec.offset + u64::from(rec.len) {
                Some(prev)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn find_by_line(&self, file_id: u32, line: u32) -> Option<u32> {
        let events = &self.files.get(file_id as usize)?.events;
        for &id in events {
            let Some(rec) = self.record(id) else { continue };
            let end = rec.line.saturating_add(u32::from(rec.span_lines.max(1)));
            if line >= rec.line && line < end {
                return Some(id);
            }
        }
        None
    }

    pub fn global_facets(&self, limit: usize) -> Vec<(String, Vec<(String, u64)>)> {
        let mut keys: Vec<(u16, String)> = self
            .key_ids
            .iter()
            .map(|(k, id)| (*id, k.clone()))
            .collect();
        keys.sort_by(|a, b| facet_rank(&a.1).cmp(&facet_rank(&b.1)).then(a.1.cmp(&b.1)));
        let mut out = Vec::new();
        for (id, name) in keys {
            if name == "format_detail" || name == "stack" {
                continue;
            }
            let Some(map) = self.facet_counts.get(&id) else {
                continue;
            };
            let mut vals: Vec<(String, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
            vals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            vals.truncate(limit);
            if !vals.is_empty() {
                out.push((name, vals));
            }
        }
        out
    }

    pub fn sparse_index_bytes(&self) -> u64 {
        self.blocks
            .iter()
            .map(|b| b.entries.len() as u64 * std::mem::size_of::<Packed>() as u64)
            .sum()
    }

    pub fn payload_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes.len() as u64).sum()
    }
}

impl Default for LogIndex {
    fn default() -> Self {
        Self::new()
    }
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

fn facet_rank(name: &str) -> u8 {
    match name {
        "level" => 0,
        "service" => 1,
        "source" => 2,
        "status" => 3,
        "host" => 4,
        "method" => 5,
        _ => 9,
    }
}
