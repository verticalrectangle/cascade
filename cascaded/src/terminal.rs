use std::path::{Path, PathBuf};

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    Json,
};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{json_err, AppState};

/// Plugin/omp caller authenticated with `X-Cascade-Token`.
///
/// Accepts a per-account machine token or the shared `CASCADE_TERMINAL_TOKEN`
/// (admin back-compat).
pub struct TerminalAuth {
    pub owner: String,
    pub is_admin: bool,
}

impl FromRequestParts<AppState> for TerminalAuth {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get("x-cascade-token")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string())
            .ok_or_else(|| json_err(StatusCode::UNAUTHORIZED, "missing X-Cascade-Token"))?;

        if !state.terminal_token.is_empty() && presented == state.terminal_token {
            let db = state.db_path.clone();
            let uid = tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&db)?;
                Ok::<_, anyhow::Error>(crate::auth::admin_uid(&conn))
            })
            .await
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
            let Some(owner) = uid else {
                return Err(json_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no admin account for CASCADE_TERMINAL_TOKEN",
                ));
            };
            return Ok(TerminalAuth {
                owner,
                is_admin: true,
            });
        }

        let db = state.db_path.clone();
        let token = presented.clone();
        let owner = tokio::task::spawn_blocking(move || crate::relay::owner_for_token(&db, &token))
            .await
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        let Some(owner) = owner else {
            return Err(json_err(
                StatusCode::UNAUTHORIZED,
                "invalid X-Cascade-Token",
            ));
        };
        Ok(TerminalAuth {
            owner,
            is_admin: false,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterTerminalRequest {
    pub machine: String,
    pub session_id: String,
    pub join_handle: String,
    pub view_handle: String,
    pub cwd: String,
    pub title: Option<String>,
    pub pid: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UnregisterTerminalRequest {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub pid: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalSessionRow {
    pub session_id: String,
    pub machine: String,
    pub join_handle: String,
    pub view_handle: String,
    pub cwd: String,
    pub title: Option<String>,
    pub pid: Option<i64>,
    pub created_at: String,
}

pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS terminal_sessions (
            session_id TEXT PRIMARY KEY,
            machine TEXT NOT NULL,
            join_handle TEXT NOT NULL,
            view_handle TEXT NOT NULL,
            cwd TEXT NOT NULL,
            title TEXT,
            pid INTEGER,
            created_at TEXT NOT NULL,
            owner TEXT NOT NULL DEFAULT ''
        );",
    )?;
    crate::auth::ensure_column(
        conn,
        "terminal_sessions",
        "owner",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    Ok(())
}

fn upsert(db: &Path, row: &RegisterTerminalRequest, owner: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    let now = Utc::now().to_rfc3339();
    // omp's session id can be empty at session_start; fall back to a pid key so
    // the PRIMARY KEY stays unique and the row remains deletable.
    let session_id = if row.session_id.is_empty() {
        format!("pid-{}", row.pid.unwrap_or(0))
    } else {
        row.session_id.clone()
    };
    conn.execute(
        "INSERT INTO terminal_sessions
            (session_id, machine, join_handle, view_handle, cwd, title, pid, created_at, owner)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(session_id) DO UPDATE SET
            machine = excluded.machine,
            join_handle = excluded.join_handle,
            view_handle = excluded.view_handle,
            cwd = excluded.cwd,
            title = excluded.title,
            pid = excluded.pid,
            owner = CASE WHEN excluded.owner = '' THEN terminal_sessions.owner ELSE excluded.owner END",
        rusqlite::params![
            session_id,
            row.machine,
            row.join_handle,
            row.view_handle,
            row.cwd,
            row.title,
            row.pid,
            now,
            owner
        ],
    )?;
    Ok(())
}

fn remove(
    db: &Path,
    session_id: &str,
    pid: Option<i64>,
    owner: &str,
    is_admin: bool,
) -> anyhow::Result<usize> {
    let conn = Connection::open(db)?;
    let n = if is_admin {
        conn.execute(
            "DELETE FROM terminal_sessions WHERE session_id = ?1 OR (?2 IS NOT NULL AND pid = ?2)",
            rusqlite::params![session_id, pid],
        )?
    } else {
        conn.execute(
            "DELETE FROM terminal_sessions WHERE owner = ?3 AND (session_id = ?1 OR (?2 IS NOT NULL AND pid = ?2))",
            rusqlite::params![session_id, pid, owner],
        )?
    };
    Ok(n)
}

pub fn list(db: &Path, owner: &str) -> anyhow::Result<Vec<TerminalSessionRow>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare(
        "SELECT session_id, machine, join_handle, view_handle, cwd, title, pid, created_at
         FROM terminal_sessions WHERE owner = ?1 ORDER BY created_at DESC",
    )?;
    let mapped = stmt.query_map([owner], |r| {
        Ok(TerminalSessionRow {
            session_id: r.get(0)?,
            machine: r.get(1)?,
            join_handle: r.get(2)?,
            view_handle: r.get(3)?,
            cwd: r.get(4)?,
            title: r.get(5)?,
            pid: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for row in mapped {
        out.push(row?);
    }
    Ok(out)
}

pub async fn register(
    State(state): State<AppState>,
    tok: TerminalAuth,
    Json(body): Json<RegisterTerminalRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if body.session_id.trim().is_empty()
        || body.machine.trim().is_empty()
        || body.join_handle.trim().is_empty()
        || body.view_handle.trim().is_empty()
    {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "machine, session_id, join_handle, view_handle are required",
        ));
    }
    let db: PathBuf = state.db_path.clone();
    let owner = tok.owner;
    tokio::task::spawn_blocking(move || upsert(&db, &body, &owner))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unregister(
    State(state): State<AppState>,
    tok: TerminalAuth,
    Json(body): Json<UnregisterTerminalRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if body.session_id.trim().is_empty() && body.pid.is_none() {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "session_id or pid is required",
        ));
    }
    let db: PathBuf = state.db_path.clone();
    let id = body.session_id.clone();
    let pid = body.pid;
    let owner = tok.owner;
    let is_admin = tok.is_admin;
    let n = tokio::task::spawn_blocking(move || remove(&db, &id, pid, &owner, is_admin))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if n == 0 {
        return Err(json_err(
            StatusCode::NOT_FOUND,
            "terminal session not found",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}
