use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// SQLite-backed registry of known sessions (local or cloud-spawned).
#[derive(Clone)]
pub struct SessionRegistry {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub omp_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub session_file: Option<String>,
    pub machine: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
}

impl SessionRegistry {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open session registry {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                omp_session_id TEXT,
                name TEXT,
                cwd TEXT NOT NULL,
                session_file TEXT,
                machine TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_active TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn upsert(&self, meta: &SessionMeta) -> Result<()> {
        let conn = self.conn.lock().expect("registry mutex");
        conn.execute(
            "INSERT INTO sessions (id, omp_session_id, name, cwd, session_file, machine, created_at, last_active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                omp_session_id = excluded.omp_session_id,
                name = excluded.name,
                cwd = excluded.cwd,
                session_file = excluded.session_file,
                machine = excluded.machine,
                created_at = excluded.created_at,
                last_active = excluded.last_active",
            params![
                meta.id,
                meta.omp_session_id,
                meta.name,
                meta.cwd,
                meta.session_file,
                meta.machine,
                meta.created_at.to_rfc3339(),
                meta.last_active.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let conn = self.conn.lock().expect("registry mutex");
        let mut stmt = conn.prepare(
            "SELECT id, omp_session_id, name, cwd, session_file, machine, created_at, last_active
             FROM sessions ORDER BY last_active DESC",
        )?;
        let rows = stmt.query_map([], row_to_meta)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("registry mutex");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE sessions SET last_active = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("registry mutex");
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<SessionMeta>> {
        let conn = self.conn.lock().expect("registry mutex");
        let mut stmt = conn.prepare(
            "SELECT id, omp_session_id, name, cwd, session_file, machine, created_at, last_active
             FROM sessions WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![id], row_to_meta).optional()?)
    }
}

fn row_to_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    Ok(SessionMeta {
        id: row.get(0)?,
        omp_session_id: row.get(1)?,
        name: row.get(2)?,
        cwd: row.get(3)?,
        session_file: row.get(4)?,
        machine: row.get(5)?,
        created_at: parse_ts(&row.get::<_, String>(6)?),
        last_active: parse_ts(&row.get::<_, String>(7)?),
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
