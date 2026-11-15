//! Live tail: notify + poll for files, and line streaming for stdin, SSH, Docker, journald.

use crate::engine::Engine;
use crate::sources;
use crate::types::{Origin, SourceKind};
use anyhow::Result;
use notify::Watcher;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

pub async fn follow_path(
    engine: Arc<RwLock<Engine>>,
    path: PathBuf,
    tx: broadcast::Sender<String>,
) {
    let parent = path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = wake_tx.blocking_send(());
            }
        }) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("logfog: notify: {err}");
                return;
            }
        };
    if let Err(err) = watcher.watch(&parent, notify::RecursiveMode::NonRecursive) {
        eprintln!("logfog: watch {}: {err}", parent.display());
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(700));
    loop {
        tokio::select! {
            _ = wake_rx.recv() => {}
            _ = ticker.tick() => {}
        }
        let ids = {
            let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
            eng.poll_path(&path).unwrap_or_default()
        };
        broadcast_ids(&engine, &tx, ids).await;
    }
}

pub async fn follow_stdin(engine: Arc<RwLock<Engine>>, tx: broadcast::Sender<String>) {
    let file_id = {
        let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
        eng.ensure_stdin()
    };
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                let ids = {
                    let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
                    eng.append_bytes(file_id, &bytes, true)
                };
                broadcast_ids(&engine, &tx, ids).await;
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("logfog: stdin: {err}");
                break;
            }
        }
    }
    let ids = {
        let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
        eng.append_bytes(file_id, &[], false)
    };
    broadcast_ids(&engine, &tx, ids).await;
}

pub async fn follow_command(
    engine: Arc<RwLock<Engine>>,
    program: &str,
    args: Vec<String>,
    origin: Origin,
    tx: broadcast::Sender<String>,
) {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("logfog: spawn {program}: {err}");
            return;
        }
    };
    let file_id = {
        let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
        match eng.ingest_bytes(origin, Vec::new()) {
            Ok(id) => id,
            Err(err) => {
                eprintln!("logfog: {err:#}");
                return;
            }
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let engine_out = engine.clone();
    let tx_out = tx.clone();
    let out_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            pump_lines(engine_out, file_id, BufReader::new(stdout), tx_out).await;
        }
    });
    let engine_err = engine.clone();
    let tx_err = tx.clone();
    let err_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            pump_lines(engine_err, file_id, BufReader::new(stderr), tx_err).await;
        }
    });
    let _ = child.wait().await;
    let _ = out_task.await;
    let _ = err_task.await;
    let ids = {
        let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
        eng.append_bytes(file_id, &[], false)
    };
    broadcast_ids(&engine, &tx, ids).await;
}

async fn pump_lines<R>(
    engine: Arc<RwLock<Engine>>,
    file_id: u32,
    reader: BufReader<R>,
    tx: broadcast::Sender<String>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let ids = {
            let mut eng = engine.write().unwrap_or_else(|e| e.into_inner());
            eng.append_bytes(file_id, &bytes, true)
        };
        broadcast_ids(&engine, &tx, ids).await;
    }
}

async fn broadcast_ids(
    engine: &Arc<RwLock<Engine>>,
    tx: &broadcast::Sender<String>,
    ids: Vec<u32>,
) {
    if ids.is_empty() {
        return;
    }
    let payloads: Vec<String> = {
        let eng = engine.read().unwrap_or_else(|e| e.into_inner());
        ids.into_iter()
            .filter_map(|id| eng.preview(id))
            .filter_map(|ev| serde_json::to_string(&ev).ok())
            .collect()
    };
    for payload in payloads {
        let _ = tx.send(payload);
    }
}

pub fn docker_origin(container: &str) -> Origin {
    Origin::named(SourceKind::Docker, format!("docker:{container}"), container)
}

pub fn journal_origin(unit: &str) -> Origin {
    Origin::named(SourceKind::Journald, format!("journal:{unit}"), unit)
}

pub fn ssh_origin(spec: &str) -> Result<Origin> {
    let host = sources::ssh_host(spec)?;
    let mut origin = Origin::named(SourceKind::Ssh, spec, "");
    origin.host = host;
    Ok(origin)
}
