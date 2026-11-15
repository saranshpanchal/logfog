//! In-process fixed-depth Drain template miner.
//!
//! Tokens that look like UUIDs, IPs, numbers, hex, paths, or durations are
//! masked to `<*>` before clustering. A depth-4 prefix tree (the length root
//! plus three token levels, matching the usual Drain depth) finds candidate
//! clusters. A bounded same-length similarity fallback merges templates whose
//! variable token diverges before the leaf, which is how `alice` and `bob`
//! collapse when they were not masked up front.

use crate::types::Level;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

const DEPTH_TOKENS: usize = 3;
const SIM_THRESHOLD: f64 = 0.5;
const BUCKET_NS: i64 = 60_000_000_000;

#[derive(Debug, Clone)]
pub struct Cluster {
    pub id: u32,
    pub template: String,
    pub tokens: Vec<String>,
    pub count: u64,
    pub levels: [u64; 7],
    pub first_ns: i64,
    pub last_ns: i64,
    pub first_event: u32,
    pub last_event: u32,
    /// `(bucket_start_ns, total, errors)`
    pub buckets: Vec<(i64, u32, u32)>,
    pub spike: Option<String>,
}

struct Node {
    children: HashMap<String, usize>,
    clusters: Vec<u32>,
}

pub struct Miner {
    nodes: Vec<Node>,
    by_len: HashMap<usize, usize>,
    pub clusters: Vec<Cluster>,
}

impl Default for Miner {
    fn default() -> Self {
        Self::new()
    }
}

impl Miner {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                children: HashMap::new(),
                clusters: Vec::new(),
            }],
            by_len: HashMap::new(),
            clusters: Vec::new(),
        }
    }

    pub fn observe(&mut self, message: &str, level: Level, ts_ns: i64) -> u32 {
        let tokens = tokenize(message);
        let len = tokens.len();
        if let Some(id) = self.find_match(len, &tokens) {
            self.merge(id, &tokens, level, ts_ns);
            return id;
        }
        let id = self.clusters.len() as u32;
        self.clusters.push(Cluster {
            id,
            template: tokens.join(" "),
            tokens: tokens.clone(),
            count: 0,
            levels: [0; 7],
            first_ns: 0,
            last_ns: 0,
            first_event: 0,
            last_event: 0,
            buckets: Vec::new(),
            spike: None,
        });
        self.insert_tree(len, &tokens, id);
        self.merge(id, &tokens, level, ts_ns);
        id
    }

    pub fn note_event(&mut self, id: u32, event_id: u32) {
        let Some(c) = self.clusters.get_mut(id as usize) else {
            return;
        };
        if c.count == 1 {
            c.first_event = event_id;
        }
        c.last_event = event_id;
    }

    pub fn refresh_spikes(&mut self, min_ts: i64, max_ts: i64) {
        let span = max_ts.saturating_sub(min_ts);
        for c in &mut self.clusters {
            c.spike = detect_spike(c, span);
        }
    }

    fn find_match(&self, len: usize, tokens: &[String]) -> Option<u32> {
        let cands = self.candidates(len, tokens).unwrap_or_default();
        best_of(self, cands, tokens, None)
            .filter(|(_, score)| *score >= SIM_THRESHOLD)
            .map(|(id, _)| id)
    }

    fn candidates(&self, len: usize, tokens: &[String]) -> Option<Vec<u32>> {
        let root = *self.by_len.get(&len)?;
        let mut node = root;
        let limit = DEPTH_TOKENS.min(tokens.len());
        for token in tokens.iter().take(limit) {
            let children = &self.nodes[node].children;
            if let Some(next) = children.get(token).or_else(|| children.get("<*>")) {
                node = *next;
            } else {
                return Some(Vec::new());
            }
        }
        Some(self.nodes[node].clusters.clone())
    }

    fn insert_tree(&mut self, len: usize, tokens: &[String], id: u32) {
        if !self.by_len.contains_key(&len) {
            let idx = self.alloc_node();
            self.by_len.insert(len, idx);
        }
        let mut node = self.by_len[&len];
        let limit = DEPTH_TOKENS.min(tokens.len());
        for token in tokens.iter().take(limit) {
            if let Some(next) = self.nodes[node].children.get(token).copied() {
                node = next;
            } else {
                let next = self.alloc_node();
                self.nodes[node].children.insert(token.clone(), next);
                node = next;
            }
        }
        self.nodes[node].clusters.push(id);
    }

    fn alloc_node(&mut self) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node {
            children: HashMap::new(),
            clusters: Vec::new(),
        });
        idx
    }

    fn merge(&mut self, id: u32, tokens: &[String], level: Level, ts_ns: i64) {
        let c = &mut self.clusters[id as usize];
        if c.tokens.len() == tokens.len() {
            let mut changed = false;
            for (slot, incoming) in c.tokens.iter_mut().zip(tokens) {
                if slot != incoming {
                    *slot = "<*>".to_string();
                    changed = true;
                }
            }
            if changed {
                c.template = c.tokens.join(" ");
            }
        }
        c.count += 1;
        c.levels[level as usize] += 1;
        if ts_ns > 0 {
            if c.first_ns == 0 || ts_ns < c.first_ns {
                c.first_ns = ts_ns;
            }
            if ts_ns > c.last_ns {
                c.last_ns = ts_ns;
            }
            let bucket = ts_ns - ts_ns.rem_euclid(BUCKET_NS);
            let err = u32::from(level.is_error());
            if let Some(last) = c.buckets.last_mut() {
                if last.0 == bucket {
                    last.1 = last.1.saturating_add(1);
                    last.2 = last.2.saturating_add(err);
                } else {
                    c.buckets.push((bucket, 1, err));
                }
            } else {
                c.buckets.push((bucket, 1, err));
            }
        }
    }
}

fn best_of(
    miner: &Miner,
    ids: impl IntoIterator<Item = u32>,
    tokens: &[String],
    mut best: Option<(u32, f64)>,
) -> Option<(u32, f64)> {
    for id in ids {
        let Some(c) = miner.clusters.get(id as usize) else {
            continue;
        };
        let score = similarity(&c.tokens, tokens);
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((id, score));
        }
    }
    best
}

fn similarity(a: &[String], b: &[String]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut matches = 0usize;
    for (x, y) in a.iter().zip(b) {
        if x == y || x == "<*>" || y == "<*>" {
            matches += 1;
        }
    }
    matches as f64 / a.len() as f64
}

fn detect_spike(c: &Cluster, span_ns: i64) -> Option<String> {
    let errors = c.levels[Level::Error as usize] + c.levels[Level::Fatal as usize];
    if errors == 0 || c.buckets.is_empty() {
        return None;
    }
    if c.buckets.len() >= 2 {
        let last_e = c.buckets.last().map(|b| b.2).unwrap_or(0);
        let prev: Vec<u32> = c.buckets[..c.buckets.len() - 1]
            .iter()
            .map(|b| b.2)
            .collect();
        let avg = prev.iter().map(|n| f64::from(*n)).sum::<f64>() / prev.len() as f64;
        if last_e >= 5 && (avg < 0.5 || f64::from(last_e) > avg * 3.0) {
            return Some(format!(
                "error spike ({last_e} in the latest minute vs baseline {avg:.1})"
            ));
        }
    }
    // A template that shows up only at the end of a long span and is all errors.
    if span_ns > 10 * BUCKET_NS && c.buckets.len() == 1 && errors >= 3 && c.count == errors {
        let first = c.first_ns;
        if first > 0 && c.last_ns.saturating_sub(first) < BUCKET_NS {
            return Some(format!("new error template ({errors} events)"));
        }
    }
    None
}

pub fn mask_message(message: &str) -> String {
    let mut s = message.to_string();
    for re in maskers() {
        s = re.replace_all(&s, "<*>").into_owned();
    }
    s
}

fn tokenize(message: &str) -> Vec<String> {
    let masked = mask_message(message);
    let tokens: Vec<String> = masked.split_whitespace().map(|t| t.to_string()).collect();
    if tokens.is_empty() {
        vec!["(empty)".into()]
    } else {
        tokens
    }
}

fn maskers() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        let pats = [
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
            r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b",
            r"\b\d+(?:\.\d+)?(?:ns|us|µs|ms|s|m|h)\b",
            r"(?:[\w.@+-]+[/\\])+[\w.@+-]+(?:\.\w+)?(?::\d+)?",
            r"\b0x[0-9a-fA-F]+\b",
            r"\b[0-9a-fA-F]{16,}\b",
            r"\b\d+(?:\.\d+)?\b",
        ];
        pats.into_iter()
            .map(|p| Regex::new(p).expect("mask regex"))
            .collect()
    })
}

/// Sparkline of per-bucket totals, resampled to `n` points.
pub fn sparkline(buckets: &[(i64, u32, u32)], n: usize) -> Vec<u32> {
    if n == 0 {
        return Vec::new();
    }
    if buckets.is_empty() {
        return vec![0; n];
    }
    let mut out = vec![0u32; n];
    let first = buckets[0].0;
    let last = buckets[buckets.len() - 1].0.max(first + 1);
    let span = (last - first).max(1);
    for (ts, total, _) in buckets {
        let pos = ((*ts - first) as f64 / span as f64 * (n as f64 - 1.0)).round() as usize;
        let pos = pos.min(n - 1);
        out[pos] = out[pos].saturating_add(*total);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_variables() {
        let m = mask_message("User 10 logged in from 10.1.2.3 after 12ms");
        assert_eq!(m, "User <*> logged in from <*> after <*>");
        let u = mask_message("id 550e8400-e29b-41d4-a716-446655440000");
        assert!(u.contains("<*>"));
        assert!(!u.contains("550e8400"));
    }

    #[test]
    fn clusters_parameterized_lines() {
        let mut miner = Miner::new();
        let lines = [
            "User 10 logged in from 10.1.2.3",
            "User 11 logged in from 10.1.2.4",
            "User 12 logged in from 192.168.0.1",
            "Failed password for alice from 10.0.0.8",
            "Failed password for bob from 10.0.0.9",
        ];
        for (i, line) in lines.iter().enumerate() {
            let id = miner.observe(line, Level::Info, 1_700_000_000_000_000_000 + i as i64);
            miner.note_event(id, i as u32);
        }
        let templates: Vec<&str> = miner.clusters.iter().map(|c| c.template.as_str()).collect();
        assert!(
            templates.contains(&"User <*> logged in from <*>"),
            "{templates:?}"
        );
        assert!(
            templates.contains(&"Failed password for <*> from <*>"),
            "{templates:?}"
        );
        let user = miner
            .clusters
            .iter()
            .find(|c| c.template == "User <*> logged in from <*>")
            .unwrap();
        assert_eq!(user.count, 3);
    }
}
