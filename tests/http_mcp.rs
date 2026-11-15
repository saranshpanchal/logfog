use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use logfog::engine::Engine;
use logfog::mcp::{self, encode_frame};
use logfog::session::SessionStore;
use logfog::types::estimate_tokens;
use logfog::web::{router, AppState};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use tempfile::tempdir;
use tokio::sync::broadcast;
use tower::ServiceExt;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn app() -> axum::Router {
    let mut engine = Engine::new();
    engine.ingest_path(&fixture("query.log"), false).unwrap();
    let dir = tempdir().unwrap();
    let session = SessionStore::open(&dir.path().join("session.sqlite")).unwrap();
    // Leak the tempdir for the process lifetime of the test router. The
    // connection keeps the file open; dropping the dir would delete it on
    // some platforms while the handle is live. Keep the path alive via meta.
    session.set_meta("fixture", "query.log").unwrap();
    std::mem::forget(dir);
    let (tx, _rx) = broadcast::channel(16);
    let state = AppState {
        engine: Arc::new(RwLock::new(engine)),
        session: Arc::new(session),
        tx,
    };
    router(state)
}

async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri}");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn http_endpoints() {
    let app = app();
    let summary = get_json(&app, "/api/summary").await;
    assert!(summary["total"].as_u64().unwrap() >= 5);
    assert!(summary["levels"]["error"].as_u64().unwrap() >= 1);

    let query = get_json(&app, "/api/query?q=level%3Aerror").await;
    assert_eq!(query["total"].as_u64().unwrap(), 3);

    let ranged = get_json(&app, "/api/query?q=status%3A%3E%3D500").await;
    assert_eq!(ranged["total"].as_u64().unwrap(), 2);

    let patterns = get_json(&app, "/api/patterns").await;
    assert!(patterns["patterns"].as_array().unwrap().len() >= 2);

    let facets = get_json(&app, "/api/facets").await;
    let keys: Vec<&str> = facets["facets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["key"].as_str())
        .collect();
    assert!(keys.contains(&"service"), "{keys:?}");
    assert!(keys.contains(&"level"), "{keys:?}");

    let ctx = get_json(&app, "/api/context?file_id=0&offset=0&n=20").await;
    assert!(ctx["event"]["message"]
        .as_str()
        .unwrap()
        .contains("timeout"));
    assert!(ctx["event_text"].as_str().is_some());

    let hist = get_json(&app, "/api/histogram?buckets=8").await;
    assert!(!hist["buckets"].as_array().unwrap().is_empty());

    let trace = get_json(&app, "/api/trace?id=abc").await;
    assert_eq!(trace["events"].as_array().unwrap().len(), 1);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ctype = res.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ctype.starts_with("text/event-stream"), "{ctype}");

    let bookmark = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/bookmarks")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"label":"errors","query":"level:error"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bookmark.status(), StatusCode::CREATED);
    let listed = get_json(&app, "/api/bookmarks").await;
    assert_eq!(listed.as_array().unwrap()[0]["label"], "errors");
}

#[test]
fn mcp_jsonrpc_round_trip() {
    let mut engine = Engine::new();
    engine.ingest_path(&fixture("query.log"), false).unwrap();

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
    });
    let init_resp = mcp::handle_message(&engine, &init).unwrap();
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "logfog");

    let listed = mcp::handle_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for tool in [
        "logfog_summary",
        "logfog_patterns",
        "logfog_query",
        "logfog_trace",
        "logfog_context",
    ] {
        assert!(names.contains(&tool), "{names:?}");
    }

    let summary = call(&engine, "logfog_summary", json!({}));
    let text = summary["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        estimate_tokens(text) < 500,
        "tokens {} text:\n{text}",
        estimate_tokens(text)
    );
    assert!(text.contains("events"));
    assert!(text.contains("top_error_templates"));

    let query = call(
        &engine,
        "logfog_query",
        json!({"q": "level:error status:>=500", "limit": 20, "token_budget": 800}),
    );
    let qtext = query["result"]["content"][0]["text"].as_str().unwrap();
    assert!(qtext.contains("total=2"));
    assert!(qtext.contains("disk full"));

    let trace = call(&engine, "logfog_trace", json!({"id": "r1"}));
    let ttext = trace["result"]["content"][0]["text"].as_str().unwrap();
    assert!(ttext.contains("timeout talking"));

    let ctx = call(
        &engine,
        "logfog_context",
        json!({"file_id": 0, "offset": 0, "lines": 5}),
    );
    let ctext = ctx["result"]["content"][0]["text"].as_str().unwrap();
    assert!(ctext.contains("message"));
    assert!(ctext.contains("--- event"));

    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    assert!(mcp::handle_message(&engine, &note).is_none());

    let frame = encode_frame(&init);
    let text = String::from_utf8(frame.clone()).unwrap();
    assert!(text.starts_with("Content-Length:"));
    let mut cursor = std::io::Cursor::new(frame);
    let body = mcp::read_frame(&mut cursor).unwrap().unwrap();
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["method"], "initialize");
}

fn call(engine: &Engine, name: &str, arguments: Value) -> Value {
    mcp::handle_message(
        engine,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .unwrap()
}
