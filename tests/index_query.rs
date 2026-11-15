use logfog::Engine;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_query() -> Engine {
    let mut engine = Engine::new();
    engine.ingest_path(&fixture("query.log"), false).unwrap();
    engine
}

#[test]
fn byte_offsets_round_trip() {
    let data = std::fs::read(fixture("query.log")).unwrap();
    let engine = load_query();
    assert!(engine.len() >= 5);
    let mut covered = vec![false; data.len()];
    for id in 0..engine.len() {
        let rec = engine.record(id).unwrap();
        let start = rec.offset as usize;
        let end = start + rec.len as usize;
        assert!(end <= data.len(), "offset {start}..{end} past eof");
        let raw = engine.index.raw_bytes(id).unwrap();
        assert_eq!(raw, &data[start..end]);
        for slot in covered.iter_mut().take(end).skip(start) {
            *slot = true;
        }
        assert!(rec.line >= 1);
    }
    // Every non-newline-only gap should be inside an event. This fixture has
    // no skipped blanks, so the events tile the file.
    assert!(covered.iter().all(|b| *b), "sparse index missed bytes");
    let first = engine.record(0).unwrap();
    assert_eq!(first.offset, 0);
    assert_eq!(first.line, 1);
    assert_eq!(logfog::index::sparse_record_size(), 32);
}

#[test]
fn facet_substring_and_regex_filters() {
    let engine = load_query();
    let msgs = |q: &str| {
        let ids = engine.query(q).unwrap();
        ids.into_iter()
            .map(|id| engine.preview(id).unwrap().message)
            .collect::<Vec<_>>()
    };

    let errors = msgs("level:error");
    assert!(errors.iter().any(|m| m.contains("timeout talking")));
    assert!(errors.iter().any(|m| m.contains("disk full")));
    assert!(errors.iter().any(|m| m.contains("missing widget")));
    assert!(!errors.iter().any(|m| m.contains("health")));
    assert_eq!(errors.len(), 3);

    let ge = msgs("status:>=500");
    assert_eq!(ge.len(), 2, "{ge:?}");
    assert!(ge
        .iter()
        .all(|m| m.contains("timeout") || m.contains("disk")));

    let not_health = msgs("-service:healthcheck");
    assert!(!not_health
        .iter()
        .any(|m| m.contains("health") || m == "ping"));
    assert!(not_health.len() < engine.len() as usize);

    let timeout = msgs("\"timeout\"");
    assert_eq!(timeout.len(), 2, "{timeout:?}");

    let disk = msgs("/disk full/");
    assert_eq!(disk, vec!["disk full".to_string()]);

    let combo = msgs("level:error -service:healthcheck status:>=500");
    assert_eq!(combo.len(), 2, "{combo:?}");
}

#[test]
fn trace_and_context_by_offset() {
    let engine = load_query();
    let traced = engine.trace("abc");
    assert_eq!(traced.len(), 1);
    assert_eq!(traced[0].message, "timeout talking to db");

    let by_req = engine.trace("r9");
    assert!(by_req.iter().any(|e| e.message.contains("missing widget")));

    let rec = engine.record(0).unwrap();
    let ctx = engine
        .context(None, Some(rec.file_id), Some(rec.offset), None, 20)
        .unwrap();
    assert_eq!(ctx.event.id, rec.id);
    assert!(!ctx.event_text.is_empty());
}
