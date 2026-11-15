use logfog::{Engine, Level, Origin};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_mixed() -> Engine {
    let mut engine = Engine::new();
    engine
        .ingest_path(&fixture("mixed.log"), false)
        .expect("ingest mixed");
    engine
}

#[test]
fn mixed_formats_and_folded_stacks() {
    let engine = load_mixed();
    let previews: Vec<_> = (0..engine.len())
        .filter_map(|id| engine.preview(id))
        .collect();
    assert!(
        previews.len() < 20,
        "stack folding collapsed continuations, got {} events",
        previews.len()
    );

    let listen = previews.iter().find(|e| e.message == "listening").unwrap();
    assert_eq!(listen.level, "info");
    assert_eq!(
        listen.facets.get("service").map(String::as_str),
        Some("api")
    );

    let failed = previews
        .iter()
        .find(|e| e.message == "request failed")
        .unwrap();
    assert_eq!(failed.level, "error");
    assert_eq!(failed.facets.get("status").map(String::as_str), Some("503"));
    assert_eq!(
        failed.facets.get("trace_id").map(String::as_str),
        Some("abc")
    );
    assert!(failed.ts_ns > 0);

    let levels: Vec<Level> = [10, 20, 30, 40, 50, 60]
        .into_iter()
        .map(|n| {
            let line = format!(r#"{{"level":{n},"msg":"m"}}"#);
            let mut eng = Engine::new();
            eng.ingest_bytes(Origin::file("pino.log"), line.into_bytes())
                .unwrap();
            eng.record(0).unwrap().level
        })
        .collect();
    assert_eq!(
        levels,
        vec![
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::Fatal
        ]
    );

    assert!(previews
        .iter()
        .any(|e| e.message == "job done" && e.level == "info"));
    assert!(previews
        .iter()
        .any(|e| e.message == "timeout on upstream" && e.level == "error"));
    assert!(previews.iter().any(|e| {
        e.facets.get("status").map(String::as_str) == Some("500") && e.message.contains("timeout")
    }));

    let py = previews
        .iter()
        .find(|e| e.message.contains("something broke") || e.message.contains("RuntimeError"))
        .expect("python traceback event");
    let py_ctx = engine.context(Some(py.id), None, None, None, 2).unwrap();
    assert!(py_ctx
        .event_text
        .contains("Traceback (most recent call last)"));
    assert!(py_ctx.event_text.contains("RuntimeError: boom"));
    assert!(py_ctx.event_text.contains("File \"app.py\""));
    assert!(previews
        .iter()
        .all(|e| !e.message.starts_with("File \"app.py\"")));

    let rust = previews
        .iter()
        .find(|e| e.message.contains("panicked at"))
        .expect("rust backtrace");
    let rust_ctx = engine.context(Some(rust.id), None, None, None, 1).unwrap();
    assert!(rust_ctx.event_text.contains("rust_begin_unwind"));
    assert!(rust_ctx.stack.contains("panic_fmt") || rust_ctx.event_text.contains("panic_fmt"));

    let node = previews
        .iter()
        .find(|e| e.message.contains("connection reset"))
        .expect("node stack");
    let node_ctx = engine.context(Some(node.id), None, None, None, 1).unwrap();
    assert!(node_ctx.event_text.contains("Socket.emit"));
    assert!(node_ctx.event_text.contains("node:net"));

    assert!(previews.iter().any(|e| e.message.contains("slow query")));
    assert!(previews
        .iter()
        .any(|e| e.message.contains("POST /v1/charge") && e.level == "error"));
    assert!(previews
        .iter()
        .any(|e| e.message.contains("GET /health") && e.level == "info"));
    assert!(previews
        .iter()
        .any(|e| e.message == "unit started" && e.level == "info"));
    let colored = previews
        .iter()
        .find(|e| e.message.contains("colored failure"))
        .expect("ansi line");
    assert_eq!(colored.level, "error");
    assert!(!colored.message.contains('\u{1b}'));
    assert_eq!(colored.facets.get("code").map(String::as_str), Some("E42"));

    let go = previews
        .iter()
        .find(|e| e.message.contains("nil pointer") || e.message.starts_with("panic:"))
        .expect("go panic");
    let go_ctx = engine.context(Some(go.id), None, None, None, 1).unwrap();
    assert!(go_ctx.event_text.contains("main.go"));
    assert!(go_ctx.event_text.contains("goroutine"));
}

#[test]
fn transparent_gzip_and_zstd() {
    let raw = b"{\"level\":\"error\",\"msg\":\"zipped failure\",\"status\":500}\n";
    let dir = tempfile::tempdir().unwrap();

    let gz_path = dir.path().join("app.log.gz");
    {
        use std::io::Write;
        let f = std::fs::File::create(&gz_path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap();
    }
    let mut engine = Engine::new();
    engine.ingest_path(&gz_path, false).unwrap();
    let ev = engine.preview(0).unwrap();
    assert_eq!(ev.message, "zipped failure");
    assert_eq!(ev.level, "error");

    let zst_path = dir.path().join("app.log.zst");
    {
        use std::io::Write;
        let f = std::fs::File::create(&zst_path).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
        enc.write_all(raw).unwrap();
        enc.finish().unwrap();
    }
    let mut engine = Engine::new();
    engine.ingest_path(&zst_path, false).unwrap();
    assert_eq!(engine.preview(0).unwrap().message, "zipped failure");
}
