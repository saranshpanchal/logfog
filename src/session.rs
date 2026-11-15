//! SQLite session store for bookmarks, saved views, and small metadata.
//!
//! The log index itself is not stored here — only things the operator saves.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct SessionStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bookmark {
    pub id: i64,
    pub label: String,
    pub query: String,
    pub created_ns: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedView {
    pub id: i64,
    pub name: String,
    pub query: String,
    pub created_ns: i64,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS bookmarks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                label TEXT NOT NULL,
                query TEXT NOT NULL,
                created_ns INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS saved_views (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                query TEXT NOT NULL,
                created_ns INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        store.set_meta("opened_at", &now.to_string())?;
        Ok(store)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO session_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT value FROM session_meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn add_bookmark(&self, label: &str, query: &str) -> Result<Bookmark> {
        let created_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO bookmarks (label, query, created_ns) VALUES (?1, ?2, ?3)",
            params![label, query, created_ns],
        )?;
        let id = conn.last_insert_rowid();
        Ok(Bookmark {
            id,
            label: label.to_string(),
            query: query.to_string(),
            created_ns,
        })
    }

    pub fn list_bookmarks(&self) -> Result<Vec<Bookmark>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, label, query, created_ns FROM bookmarks ORDER BY id DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok(Bookmark {
                id: row.get(0)?,
                label: row.get(1)?,
                query: row.get(2)?,
                created_ns: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_bookmark(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM bookmarks WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn add_view(&self, name: &str, query: &str) -> Result<SavedView> {
        let created_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO saved_views (name, query, created_ns) VALUES (?1, ?2, ?3)",
            params![name, query, created_ns],
        )?;
        let id = conn.last_insert_rowid();
        Ok(SavedView {
            id,
            name: name.to_string(),
            query: query.to_string(),
            created_ns,
        })
    }

    pub fn list_views(&self) -> Result<Vec<SavedView>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, name, query, created_ns FROM saved_views ORDER BY id DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok(SavedView {
                id: row.get(0)?,
                name: row.get(1)?,
                query: row.get(2)?,
                created_ns: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}
