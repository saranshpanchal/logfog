//! Loopback browser explorer. Binds to 127.0.0.1 only.

use crate::engine::{Engine, PageQuery};
use crate::follow;
use crate::session::SessionStore;
use crate::sources;
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

#[derive(RustEmbed)]
#[folder = "src/web/static"]
struct Asset;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<RwLock<Engine>>,
    pub session: Arc<SessionStore>,
    pub tx: broadcast::Sender<String>,
}

pub struct ServeOpts {
    pub port: u16,
    pub db_path: PathBuf,
    pub follow_files: Vec<PathBuf>,
    pub follow_stdin: bool,
    pub ssh: Vec<String>,
    pub docker: Vec<String>,
    pub journald: Vec<String>,
    pub remote_lines: usize,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/favicon.ico", get(favicon))
        .route("/api/summary", get(summary))
        .route("/api/query", get(query_events))
        .route("/api/patterns", get(patterns))
        .route("/api/facets", get(facets))
        .route("/api/context", get(context))
        .route("/api/histogram", get(histogram))
        .route("/api/trace", get(trace))
        .route("/api/stream", get(stream))
        .route("/api/bookmarks", get(list_bookmarks).post(add_bookmark))
        .route("/api/bookmarks/:id", axum::routing::delete(delete_bookmark))
        .route("/api/views", get(list_views).post(add_view))
        .with_state(state)
        .layer(CorsLayer::permissive())
}

pub async fn serve(engine: Engine, opts: ServeOpts) -> Result<()> {
    let (tx, _rx) = broadcast::channel(1024);
    let engine = Arc::new(RwLock::new(engine));
    for path in opts.follow_files {
        tokio::spawn(follow::follow_path(engine.clone(), path, tx.clone()));
    }
    if opts.follow_stdin {
        tokio::spawn(follow::follow_stdin(engine.clone(), tx.clone()));
    }
    for spec in opts.ssh {
        let origin = match follow::ssh_origin(&spec) {
            Ok(origin) => origin,
            Err(err) => {
                eprintln!("logfog: {err:#}");
                continue;
            }
        };
        let args = match ssh_follow_args(&spec) {
            Ok(args) => args,
            Err(err) => {
                eprintln!("logfog: {err:#}");
                continue;
            }
        };
        tokio::spawn(follow::follow_command(
            engine.clone(),
            "ssh",
            args,
            origin,
            tx.clone(),
        ));
    }
    for container in opts.docker {
        let args = match sources::docker_follow_args(&container, opts.remote_lines) {
            Ok(args) => args,
            Err(err) => {
                eprintln!("logfog: {err:#}");
                continue;
            }
        };
        tokio::spawn(follow::follow_command(
            engine.clone(),
            "docker",
            args,
            follow::docker_origin(&container),
            tx.clone(),
        ));
    }
    for unit in opts.journald {
        let args = match sources::journal_follow_args(&unit, opts.remote_lines) {
            Ok(args) => args,
            Err(err) => {
                eprintln!("logfog: {err:#}");
                continue;
            }
        };
        tokio::spawn(follow::follow_command(
            engine.clone(),
            "journalctl",
            args,
            follow::journal_origin(&unit),
            tx.clone(),
        ));
    }
    let session = Arc::new(SessionStore::open(&opts.db_path)?);
    let state = AppState {
        engine,
        session,
        tx,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, opts.port)).await?;
    let addr = listener.local_addr()?;
    eprintln!("LogFog explorer at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn ssh_follow_args(spec: &str) -> Result<Vec<String>> {
    let (target, remote) = sources::parse_ssh_spec(spec)?;
    let script = format!("tail -n +1 -F -- {remote}");
    Ok(vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        target,
        "sh".into(),
        "-c".into(),
        script,
    ])
}

fn asset_response(path: &str) -> Response {
    match Asset::get(path) {
        Some(content) => {
            let mime = match path.rsplit('.').next() {
                Some("css") => "text/css; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("html") => "text/html; charset=utf-8",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, mime)], content.data.into_owned()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn index_html() -> Response {
    asset_response("index.html")
}
async fn app_js() -> Response {
    asset_response("app.js")
}
async fn app_css() -> Response {
    asset_response("app.css")
}
async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct QArgs {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    from_ns: Option<i64>,
    to_ns: Option<i64>,
    sort: Option<String>,
    buckets: Option<usize>,
    id: Option<u32>,
    file_id: Option<u32>,
    offset_bytes: Option<u64>,
    line: Option<u32>,
    n: Option<u32>,
}

async fn summary(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.summary(&args.q) {
        Ok(summary) => Json(summary).into_response(),
        Err(err) => bad(err),
    }
}

async fn query_events(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.query_page(PageQuery {
        query: &args.q,
        from_ns: args.from_ns,
        to_ns: args.to_ns,
        offset: args.offset.unwrap_or(0),
        limit: args.limit.unwrap_or(200).min(5_000),
        token_budget: None,
        dedup: false,
    }) {
        Ok(page) => Json(page).into_response(),
        Err(err) => bad(err),
    }
}

async fn patterns(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.patterns(&args.q, args.sort.as_deref().unwrap_or("count")) {
        Ok(rows) => Json(json!({ "patterns": rows })).into_response(),
        Err(err) => bad(err),
    }
}

async fn facets(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.facets_for(&args.q, 12) {
        Ok(facets) => Json(json!({ "facets": facets })).into_response(),
        Err(err) => bad(err),
    }
}

async fn histogram(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.histogram(&args.q, args.buckets.unwrap_or(48)) {
        Ok(hist) => Json(hist).into_response(),
        Err(err) => bad(err),
    }
}

async fn context(State(st): State<AppState>, Query(args): Query<QArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    match eng.context(
        args.id,
        args.file_id,
        args.offset_bytes.or(args.offset.map(|n| n as u64)),
        args.line,
        args.n.unwrap_or(20),
    ) {
        Ok(ctx) => Json(ctx).into_response(),
        Err(err) => bad(err),
    }
}

async fn trace(State(st): State<AppState>, Query(args): Query<TraceArgs>) -> Response {
    let eng = st.engine.read().unwrap_or_else(|e| e.into_inner());
    let events = eng.trace(&args.id);
    Json(json!({ "id": args.id, "events": events })).into_response()
}

#[derive(Deserialize)]
struct TraceArgs {
    id: String,
}

async fn stream(
    State(st): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let rx = st.tx.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(data) => return Some((Ok(Event::default().data(data)), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Deserialize)]
struct BookmarkIn {
    label: String,
    query: String,
}

async fn list_bookmarks(State(st): State<AppState>) -> Response {
    match st.session.list_bookmarks() {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => bad(err),
    }
}

async fn add_bookmark(State(st): State<AppState>, Json(body): Json<BookmarkIn>) -> Response {
    match st.session.add_bookmark(&body.label, &body.query) {
        Ok(row) => (StatusCode::CREATED, Json(row)).into_response(),
        Err(err) => bad(err),
    }
}

async fn delete_bookmark(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    match st.session.delete_bookmark(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => bad(err),
    }
}

#[derive(Deserialize)]
struct ViewIn {
    name: String,
    query: String,
}

async fn list_views(State(st): State<AppState>) -> Response {
    match st.session.list_views() {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => bad(err),
    }
}

async fn add_view(State(st): State<AppState>, Json(body): Json<ViewIn>) -> Response {
    match st.session.add_view(&body.name, &body.query) {
        Ok(row) => (StatusCode::CREATED, Json(row)).into_response(),
        Err(err) => bad(err),
    }
}

fn bad(err: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
        .into_response()
}
