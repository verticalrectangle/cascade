use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::{
        rejection::JsonRejection,
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use cascade_core::{CloudCommand, SessionEvent, SessionManager, SessionSnapshot, SpawnOptions};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::{json_err, AppState};

const PUBLIC_MAX_PER_MINUTE: usize = 5;
const PUBLIC_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_SHARE_HOURS: f64 = 24.0;
const VIEWER_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data:; frame-ancestors 'none'";
const VIEWER_FALLBACK: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
</head>
<body>
<h1>__TITLE__</h1>
<p>session __SESSION_ID__ expires __EXPIRES__</p>
<script>
window.CASCADE_SHARE = { token: "__TOKEN__", sessionId: "__SESSION_ID__", expires: "__EXPIRES__" };
</script>
</body>
</html>
"#;

static PUBLIC_ATTEMPTS: OnceLock<Mutex<HashMap<IpAddr, Vec<Instant>>>> = OnceLock::new();

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub machine: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct ShareResponse {
    pub token: String,
    pub url: String,
    pub expires_at: Option<String>,
    pub expires_in_hours: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateShareRequest {
    #[serde(default)]
    pub expires_in_hours: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ShareLookup {
    pub session_id: String,
    pub read_only: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct StreamQuery {
    share: Option<String>,
    /// Owner attaches may bound the initial snapshot to the newest N
    /// messages; older pages stream up via `get_snapshot` commands.
    tail: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArchivedShare {
    pub snapshot: SessionSnapshot,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
struct ShareRow {
    token: String,
    session_id: String,
    expires_at: Option<String>,
    final_snapshot: Option<String>,
}

enum StreamAccess {
    Owner,
    ReadOnly,
}

enum ShareMatch {
    Ok,
    Expired,
    No,
}

pub fn init_share_tables(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS shares (
            token TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT,
            final_snapshot TEXT
        );
        CREATE INDEX IF NOT EXISTS shares_session ON shares(session_id);",
    )?;
    let needs_expiry_backfill = !crate::auth::has_column(conn, "shares", "expires_at")?;
    crate::auth::ensure_column(conn, "shares", "expires_at", "TEXT")?;
    crate::auth::ensure_column(conn, "shares", "final_snapshot", "TEXT")?;
    if needs_expiry_backfill {
        let rows: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT token, created_at FROM shares")?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (token, created) in rows {
            let exp = DateTime::parse_from_rfc3339(&created)
                .map(|d| d.with_timezone(&Utc) + chrono::Duration::hours(24))
                .unwrap_or_else(|_| Utc::now() + chrono::Duration::hours(24));
            conn.execute(
                "UPDATE shares SET expires_at = ?1 WHERE token = ?2",
                rusqlite::params![exp.to_rfc3339(), token],
            )?;
        }
    }
    Ok(())
}

pub async fn list_machines(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<cascade_core::MachineInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let mut machines = vec![cascade_core::MachineInfo {
        id: "cloud".into(),
        name: "cloud".into(),
        online: true,
        is_cloud: true,
    }];
    let remote = state
        .relay
        .list_machines_async(&user.uid)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    machines.extend(remote);
    Ok(Json(machines))
}

pub async fn mint_machine_token(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<TokenResponse>, (StatusCode, Json<serde_json::Value>)> {
    let db = state.db_path.clone();
    let uid = user.uid;
    let token = tokio::task::spawn_blocking(move || crate::relay::mint_machine_token(&db, &uid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(TokenResponse { token }))
}

pub async fn delete_machine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let deleted = state
        .relay
        .delete_owned_machine(&id, &user.uid)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if !deleted {
        return Err(json_err(StatusCode::NOT_FOUND, "machine not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct ListedSession {
    pub id: String,
    pub omp_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub session_file: Option<String>,
    pub machine: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_active: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    /// Process is running (None = unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    /// Actively streaming right now (None = unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working: Option<bool>,
    /// True when the session has no content — clients hide these from the list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty: Option<bool>,
    /// "spawned" | "discovered" | "terminal" (machine/terminal rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

pub async fn list_sessions(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ListedSession>>, (StatusCode, Json<serde_json::Value>)> {
    let uid = user.uid.clone();
    let metas: Vec<cascade_core::SessionMeta> = state
        .sessions
        .list()
        .await
        .into_iter()
        .filter(|m| m.owner == uid)
        .collect();
    let mut out: Vec<ListedSession> = Vec::with_capacity(metas.len());
    for m in metas {
        // Process truth for cloud-local sessions: the manager only holds live processes.
        let (live, working) = match state.sessions.get(&m.id).await {
            Some(sess) => (Some(true), Some(sess.is_streaming().await)),
            None => (Some(false), Some(false)),
        };
        let empty = Some(m.name.is_none() && live == Some(false));
        out.push(ListedSession {
            id: m.id,
            omp_session_id: m.omp_session_id,
            name: m.name,
            cwd: m.cwd,
            session_file: m.session_file,
            machine: m.machine,
            created_at: m.created_at,
            last_active: m.last_active,
            kind: "managed".into(),
            join_handle: None,
            view_handle: None,
            pid: None,
            live,
            working,
            empty,
            origin: None,
        });
    }
    let db = state.db_path.clone();
    let term_uid = uid.clone();
    let terminals = tokio::task::spawn_blocking(move || crate::terminal::list(&db, &term_uid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    for m in state.relay.list_machine_sessions(&uid).unwrap_or_default() {
        let created = chrono::DateTime::parse_from_rfc3339(&m.created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        let is_discovered = m.origin == "discovered";
        // Discovered rows are alive by file freshness (15 min), not machine presence.
        let last_active = m
            .last_active
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or(created);
        let live = if is_discovered {
            Some((chrono::Utc::now() - last_active).num_minutes() < 15)
        } else {
            Some(state.relay.machine_online(&m.machine).await)
        };
        // A discovered file being written right now means the owning process is
        // producing output — treat sub-minute freshness as actively working.
        let working = if is_discovered {
            Some((chrono::Utc::now() - last_active).num_seconds() < 60)
        } else {
            None
        };
        let empty = if is_discovered { Some(m.messages == 0) } else { Some(m.name.is_none() && live == Some(false)) };
        out.push(ListedSession {
            id: m.id,
            omp_session_id: None,
            name: m.name,
            cwd: m.cwd,
            session_file: None,
            machine: m.machine,
            created_at: created,
            last_active,
            kind: "managed".into(),
            join_handle: None,
            view_handle: None,
            pid: None,
            live,
            working,
            empty,
            origin: Some(m.origin),
        });
    }
    for t in terminals {
        let created = chrono::DateTime::parse_from_rfc3339(&t.created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        out.push(ListedSession {
            id: t.session_id,
            omp_session_id: None,
            name: t.title,
            cwd: t.cwd,
            session_file: None,
            machine: t.machine,
            created_at: created,
            last_active: created,
            kind: "terminal".into(),
            join_handle: Some(t.join_handle),
            view_handle: Some(t.view_handle),
            pid: t.pid,
            live: Some(true),
            working: None,
            empty: Some(false),
            origin: Some("terminal".into()),
        });
    }
    Ok(Json(out))
}

pub async fn create_session(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let cwd = if body.cwd.trim().is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    } else {
        body.cwd.clone()
    };
    let cwd = cwd.as_str();
    let machine = body.machine.as_deref().unwrap_or("cloud");
    if machine == "cloud" {
        if !user.is_admin {
            return Err(json_err(
                StatusCode::FORBIDDEN,
                "cloud spawn requires admin",
            ));
        }
    } else if !state.relay.owns_machine(machine, &user.uid) {
        return Err(json_err(StatusCode::FORBIDDEN, "machine not found"));
    }

    if machine != "cloud" {
        return state
            .relay
            .forward_spawn(machine, cwd, body.model, &user.uid)
            .await
            .map(|id| Json(CreateSessionResponse { id }))
            .map_err(Into::into);
    }

    let opts = SpawnOptions {
        cwd: PathBuf::from(cwd),
        model: body.model,
        ..SpawnOptions::default()
    };
    let id = state
        .sessions
        .spawn(opts)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    if let Some(mut meta) = state.sessions.list().await.into_iter().find(|m| m.id == id) {
        meta.machine = "cloud".into();
        meta.owner = user.uid;
        let db = state.db_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            cascade_core::SessionRegistry::open(&db).and_then(|reg| reg.upsert(&meta))
        })
        .await;
    }

    Ok(Json(CreateSessionResponse { id }))
}

pub async fn delete_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    if state.sessions.get(&id).await.is_none() && state.relay.machine_of(&id).is_some() {
        state
            .relay
            .forward_shutdown(&id)
            .await
            .map_err(|e| (e.status, Json(serde_json::json!({ "error": e.message }))))?;
        return Ok(StatusCode::NO_CONTENT);
    }
    state
        .sessions
        .shutdown(&id)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let db = state.db_path.clone();
    let id_rm = id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        cascade_core::SessionRegistry::open(&db).and_then(|reg| reg.remove(&id_rm))
    })
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CreateShareRequest>, JsonRejection>,
) -> Result<Json<ShareResponse>, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    let parsed = match body {
        Ok(Json(v)) => v,
        Err(JsonRejection::MissingJsonContentType(_)) => CreateShareRequest::default(),
        Err(_) => return Err(json_err(StatusCode::BAD_REQUEST, "invalid json")),
    };
    let hours = parse_expires_in_hours(&parsed.expires_in_hours)
        .map_err(|e| json_err(StatusCode::BAD_REQUEST, e))?;
    let expires_at = expires_at_from_hours(hours, Utc::now());
    let hours_json = hours_json(hours);
    let db = state.db_path.clone();
    let session_id = id.clone();
    let expires_store = expires_at.clone();
    let token =
        tokio::task::spawn_blocking(move || mint_share(&db, &session_id, expires_store.as_deref()))
            .await
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(ShareResponse {
        url: share_url(&headers, &token),
        token,
        expires_at,
        expires_in_hours: hours_json,
    }))
}

pub async fn get_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ShareResponse>, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    let db = state.db_path.clone();
    let session_id = id.clone();
    let row = tokio::task::spawn_blocking(move || lookup_share_by_session(&db, &session_id))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let Some(row) = row else {
        return Err(json_err(StatusCode::NOT_FOUND, "not shared"));
    };
    Ok(Json(ShareResponse {
        url: share_url(&headers, &row.token),
        token: row.token,
        expires_in_hours: remaining_hours_json(row.expires_at.as_deref(), Utc::now()),
        expires_at: row.expires_at,
    }))
}

/// Public: resolve a view-link token to the session it watches.
pub async fn resolve_share(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let html = prefers_html(accept_header(&headers));
    let db = state.db_path.clone();
    let tok = token.clone();
    let row = match tokio::task::spawn_blocking(move || lookup_share_by_token(&db, &tok)).await {
        Ok(Ok(row)) => row,
        Ok(Err(e)) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response();
        }
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response();
        }
    };
    let Some(row) = row else {
        if !record_public_failure(public_ip(&headers, addr)) {
            return if html {
                (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response()
            } else {
                json_err(StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response()
            };
        }
        return if html {
            viewer_html(StatusCode::NOT_FOUND, error_page("Share link not found"))
        } else {
            json_err(StatusCode::NOT_FOUND, "unknown view link").into_response()
        };
    };
    if share_is_expired(row.expires_at.as_deref(), Utc::now()) {
        return if html {
            viewer_html(StatusCode::GONE, error_page("Share link expired"))
        } else {
            json_err(StatusCode::GONE, "share link expired").into_response()
        };
    }

    if html {
        let title = viewer_title(&state, &row.session_id, row.final_snapshot.as_deref()).await;
        let expires = expires_placeholder(row.expires_at.as_deref());
        let body = render_viewer(&token, &row.session_id, &title, &expires);
        viewer_html(StatusCode::OK, body)
    } else {
        Json(ShareLookup {
            session_id: row.session_id,
            read_only: true,
        })
        .into_response()
    }
}

pub async fn delete_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    let db = state.db_path.clone();
    let session_id = id.clone();
    tokio::task::spawn_blocking(move || revoke_shares(&db, &session_id))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn session_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let header_tok = crate::auth::bearer_from_headers(&headers).map(str::to_string);
    let query_tok = query.share.filter(|s| !s.is_empty());
    let token = match (header_tok.as_deref(), query_tok.as_deref()) {
        (Some(h), _) => h.to_string(),
        (None, Some(s)) => s.to_string(),
        (None, None) => {
            return Err(json_err(StatusCode::UNAUTHORIZED, "missing authorization"));
        }
    };

    let access = match authorize_stream(&state, &id, &token).await {
        Ok(a) => a,
        Err(e) => {
            // Only unknown-token failures count toward the public bucket.
            let is_unknown = matches!(e.0, StatusCode::UNAUTHORIZED);
            if is_unknown && !record_public_failure(public_ip(&headers, addr)) {
                return Err(json_err(StatusCode::TOO_MANY_REQUESTS, "too many requests"));
            }
            return Err(e);
        }
    };
    let read_only = match access {
        StreamAccess::Owner => false,
        StreamAccess::ReadOnly => true,
    };

    if let Some(session) = state.sessions.get(&id).await {
        let db_path = state.db_path.clone();
        let manager = state.sessions.clone();
        let tail = if read_only { None } else { query.tail };
        return Ok(ws.on_upgrade(move |socket| {
            handle_stream(socket, session, manager, read_only, db_path, tail)
        }));
    }
    if let Some(machine) = state.relay.machine_of(&id) {
        if state.relay.machine_online(&machine).await {
            let router = state.relay.clone();
            let tail = if read_only { None } else { query.tail };
            return Ok(ws.on_upgrade(move |socket| async move {
                router.proxy_stream(machine, id, socket, read_only, tail).await;
            }));
        }
        if !read_only {
            return Err(json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("machine {machine} is offline"),
            ));
        }
    }
    if read_only {
        let db = state.db_path.clone();
        let sid = id.clone();
        let archive = tokio::task::spawn_blocking(move || load_share_archive(&db, &sid))
            .await
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
            .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        if let Some(archive) = archive {
            return Ok(ws.on_upgrade(move |socket| handle_archived_stream(socket, archive)));
        }
    }
    Err(json_err(StatusCode::NOT_FOUND, "session not found"))
}

async fn authorize_stream(
    state: &AppState,
    session_id: &str,
    token: &str,
) -> Result<StreamAccess, (StatusCode, Json<serde_json::Value>)> {
    if let Ok(email) = crate::auth::verify_token(&state.jwt_secret, token) {
        let db = state.db_path.clone();
        let email_lookup = email.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db)?;
            Ok::<_, anyhow::Error>(crate::auth::resolve_user(&conn, &email_lookup))
        })
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        let Some((uid, _)) = resolved else {
            return Err(json_err(StatusCode::UNAUTHORIZED, "invalid token"));
        };
        if !owns_session(state, &uid, session_id).await {
            return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
        }
        return Ok(StreamAccess::Owner);
    }

    let db = state.db_path.clone();
    let tok = token.to_string();
    let sid = session_id.to_string();
    let status = tokio::task::spawn_blocking(move || share_status(&db, &tok, &sid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    match status {
        ShareMatch::Ok => Ok(StreamAccess::ReadOnly),
        ShareMatch::Expired => Err(json_err(StatusCode::NOT_FOUND, "share link expired")),
        ShareMatch::No => Err(json_err(StatusCode::UNAUTHORIZED, "invalid token")),
    }
}

async fn owns_session(state: &AppState, uid: &str, session_id: &str) -> bool {
    let db = state.db_path.clone();
    let sid = session_id.to_string();
    let uid_owned = uid.to_string();
    let registry_owner = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let reg = cascade_core::SessionRegistry::open(&db)?;
        Ok(reg.get(&sid)?.map(|m| m.owner))
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten();
    if let Some(owner) = registry_owner {
        if owner == uid {
            return true;
        }
    }
    match state.relay.session_owner(session_id) {
        Some(owner) => owner == uid_owned,
        None => false,
    }
}

fn mint_share(
    db: &std::path::Path,
    session_id: &str,
    expires_at: Option<&str>,
) -> anyhow::Result<String> {
    let conn = Connection::open(db)?;
    conn.execute("DELETE FROM shares WHERE session_id = ?1", [session_id])?;
    let token = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO shares (token, session_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![token, session_id, Utc::now().to_rfc3339(), expires_at],
    )?;
    Ok(token)
}

fn revoke_shares(db: &std::path::Path, session_id: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("DELETE FROM shares WHERE session_id = ?1", [session_id])?;
    Ok(())
}

fn share_status(db: &std::path::Path, token: &str, session_id: &str) -> anyhow::Result<ShareMatch> {
    match lookup_share_by_token(db, token)? {
        None => Ok(ShareMatch::No),
        Some(row) if row.session_id != session_id => Ok(ShareMatch::No),
        Some(row) if share_is_expired(row.expires_at.as_deref(), Utc::now()) => {
            Ok(ShareMatch::Expired)
        }
        Some(_) => Ok(ShareMatch::Ok),
    }
}

#[cfg(test)]
fn share_matches(db: &std::path::Path, token: &str, session_id: &str) -> anyhow::Result<bool> {
    Ok(matches!(
        share_status(db, token, session_id)?,
        ShareMatch::Ok
    ))
}

#[cfg(test)]
fn share_session_for_token(db: &std::path::Path, token: &str) -> anyhow::Result<Option<String>> {
    Ok(lookup_share_by_token(db, token)?.map(|r| r.session_id))
}

#[cfg(test)]
fn share_token_for_session(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(lookup_share_by_session(db, session_id)?.map(|r| r.token))
}

fn lookup_share_by_token(db: &std::path::Path, token: &str) -> anyhow::Result<Option<ShareRow>> {
    let conn = Connection::open(db)?;
    let found = conn
        .query_row(
            "SELECT token, session_id, expires_at, final_snapshot FROM shares WHERE token = ?1",
            [token],
            row_to_share,
        )
        .optional()?;
    Ok(found)
}

fn lookup_share_by_session(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<ShareRow>> {
    let conn = Connection::open(db)?;
    let found = conn
        .query_row(
            "SELECT token, session_id, expires_at, final_snapshot FROM shares WHERE session_id = ?1",
            [session_id],
            row_to_share,
        )
        .optional()?;
    Ok(found)
}

fn row_to_share(r: &rusqlite::Row<'_>) -> rusqlite::Result<ShareRow> {
    Ok(ShareRow {
        token: r.get(0)?,
        session_id: r.get(1)?,
        expires_at: r.get(2)?,
        final_snapshot: r.get(3)?,
    })
}

fn load_share_archive(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<ArchivedShare>> {
    let Some(row) = lookup_share_by_session(db, session_id)? else {
        return Ok(None);
    };
    if share_is_expired(row.expires_at.as_deref(), Utc::now()) {
        return Ok(None);
    }
    let Some(raw) = row.final_snapshot.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&raw).ok())
}

pub(crate) fn persist_share_archive(
    db: &std::path::Path,
    session_id: &str,
    snapshot: &SessionSnapshot,
    title: Option<&str>,
) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    let payload = serde_json::to_string(&ArchivedShare {
        snapshot: snapshot.clone(),
        title: title.map(str::to_string).filter(|s| !s.is_empty()),
    })?;
    conn.execute(
        "UPDATE shares SET final_snapshot = ?1 WHERE session_id = ?2",
        rusqlite::params![payload, session_id],
    )?;
    Ok(())
}

pub(crate) fn apply_share_event(
    snapshot: &mut SessionSnapshot,
    title: &mut Option<String>,
    ev: &SessionEvent,
) {
    if let SessionEvent::SessionInfo { title: t, .. } = ev {
        if !t.is_empty() {
            *title = Some(t.clone());
        }
    }
    snapshot.apply(ev);
}

fn public_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())?;
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let proto = forwarded.unwrap_or_else(|| {
        if host.starts_with("localhost")
            || host.starts_with("127.")
            || host.starts_with("[::1]")
            || host.starts_with("0.0.0.0")
        {
            "http"
        } else {
            "https"
        }
    });
    Some(format!("{proto}://{host}"))
}

fn share_url(headers: &HeaderMap, token: &str) -> String {
    match public_origin(headers) {
        Some(origin) => format!("{origin}/s/{token}"),
        None => format!("/s/{token}"),
    }
}

/// `true` when Accept prefers text/html over application/json (browsers).
/// Missing Accept, equal q-values, and `*/*` resolve as JSON (app default).
pub fn prefers_html(accept: Option<&str>) -> bool {
    let Some(raw) = accept.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    q_for(raw, "text/html") > q_for(raw, "application/json")
}

fn q_for(accept: &str, exact: &str) -> f32 {
    let (want_ty, want_sub) = exact.split_once('/').unwrap_or((exact, "*"));
    let mut best: Option<f32> = None;
    for part in accept.split(',') {
        let mut segs = part.split(';');
        let media = segs.next().unwrap_or("").trim();
        let mut q = 1.0_f32;
        for p in segs {
            let p = p.trim();
            if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                q = v.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0);
            }
        }
        if q <= 0.0 {
            continue;
        }
        let matched = if media == "*/*" {
            true
        } else if let Some((t, s)) = media.split_once('/') {
            t.eq_ignore_ascii_case(want_ty) && (s == "*" || s.eq_ignore_ascii_case(want_sub))
        } else {
            false
        };
        if matched {
            best = Some(best.map_or(q, |b| b.max(q)));
        }
    }
    best.unwrap_or(-1.0)
}

/// NULL/empty `expires_at` means the share never expires.
pub fn share_is_expired(expires_at: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(raw) = expires_at.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => dt.with_timezone(&Utc) <= now,
        Err(_) => true,
    }
}

fn parse_expires_in_hours(value: &Option<serde_json::Value>) -> Result<Option<f64>, &'static str> {
    match value {
        None => Ok(Some(DEFAULT_SHARE_HOURS)),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            let h = n.as_f64().ok_or("invalid expires_in_hours")?;
            if !h.is_finite() || h < 0.0 {
                return Err("invalid expires_in_hours");
            }
            Ok(Some(h))
        }
        Some(_) => Err("invalid expires_in_hours"),
    }
}

fn expires_at_from_hours(hours: Option<f64>, now: DateTime<Utc>) -> Option<String> {
    let hours = hours?;
    let millis = (hours * 3_600_000.0).round() as i64;
    Some((now + chrono::Duration::milliseconds(millis)).to_rfc3339())
}

fn hours_json(hours: Option<f64>) -> serde_json::Value {
    match hours {
        None => serde_json::Value::Null,
        Some(h) if h.fract() == 0.0 && h.abs() < (i64::MAX as f64) => {
            serde_json::json!(h as i64)
        }
        Some(h) => serde_json::json!(h),
    }
}

fn remaining_hours_json(expires_at: Option<&str>, now: DateTime<Utc>) -> serde_json::Value {
    let Some(raw) = expires_at.map(str::trim).filter(|s| !s.is_empty()) else {
        return serde_json::Value::Null;
    };
    let Ok(dt) = DateTime::parse_from_rfc3339(raw) else {
        return serde_json::Value::Null;
    };
    let secs = (dt.with_timezone(&Utc) - now).num_seconds().max(0) as f64;
    hours_json(Some((secs / 3600.0).ceil()))
}

fn accept_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
}

fn public_ip(headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }
    addr.ip()
}

/// True when this IP is over the public-failure budget. Counts ONLY failures
/// (bad/unknown tokens) — valid tokens are never throttled, so a viewer's
/// reconnect loop can't lock itself out of a good link.
fn record_public_failure(ip: IpAddr) -> bool {
    let mut map = PUBLIC_ATTEMPTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let stamps = map.entry(ip).or_default();
    stamps.retain(|t| now.saturating_duration_since(*t) < PUBLIC_WINDOW);
    if stamps.len() >= PUBLIC_MAX_PER_MINUTE {
        return false;
    }
    stamps.push(now);
    true
}

fn viewer_template() -> String {
    // Compile-time embed: the binary is self-contained on the server.
    let s = include_str!("../static/viewer.html");
    if s.contains("__TOKEN__") { s.to_string() } else { VIEWER_FALLBACK.to_string() }
}

fn render_viewer(token: &str, session_id: &str, title: &str, expires: &str) -> String {
    viewer_template()
        .replace("__TOKEN__", &html_escape(token))
        .replace("__SESSION_ID__", &html_escape(session_id))
        .replace("__TITLE__", &html_escape(title))
        .replace("__EXPIRES__", &html_escape(expires))
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn expires_placeholder(expires_at: Option<&str>) -> String {
    match expires_at.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => "never".into(),
    }
}

fn error_page(message: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{0}</title></head><body><h1>{0}</h1></body></html>",
        html_escape(message)
    )
}

fn viewer_html(status: StatusCode, body: String) -> Response {
    let mut res = Response::new(axum::body::Body::from(body));
    *res.status_mut() = status;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(VIEWER_CSP),
    );
    res
}

async fn viewer_title(state: &AppState, session_id: &str, archive_json: Option<&str>) -> String {
    if let Some(raw) = archive_json {
        if let Ok(arch) = serde_json::from_str::<ArchivedShare>(raw) {
            if let Some(t) = arch.title.filter(|s| !s.is_empty()) {
                return t;
            }
        }
    }
    if let Some(meta) = state
        .sessions
        .list()
        .await
        .into_iter()
        .find(|m| m.id == session_id)
    {
        if let Some(name) = meta.name.filter(|s| !s.is_empty()) {
            return name;
        }
    }
    let db = state.db_path.clone();
    let sid = session_id.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let reg = cascade_core::SessionRegistry::open(&db)?;
        Ok(reg.get(&sid)?.and_then(|m| m.name))
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten()
    .filter(|s| !s.is_empty())
    .unwrap_or_default()
}

async fn handle_archived_stream(socket: WebSocket, archive: ArchivedShare) {
    let (mut sink, mut stream) = socket.split();
    let snap = SessionEvent::Snapshot(archive.snapshot);
    if let Ok(text) = serde_json::to_string(&snap) {
        if sink.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }
    let exit = SessionEvent::ProcessExited { code: None };
    if let Ok(text) = serde_json::to_string(&exit) {
        let _ = sink.send(Message::Text(text.into())).await;
    }
    while let Some(incoming) = stream.next().await {
        match incoming {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(p)) => {
                if sink.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            Ok(_) => {}
        }
    }
}

async fn handle_stream(
    socket: WebSocket,
    session: cascade_core::OmpSession,
    manager: SessionManager,
    read_only: bool,
    db_path: PathBuf,
    tail: Option<u32>,
) {
    let (mut sink, mut stream) = socket.split();
    let session_id = session.id().to_string();
    let mut title = manager
        .list()
        .await
        .into_iter()
        .find(|m| m.id == session_id)
        .and_then(|m| m.name);

    let snapshot = session.snapshot().await;
    let snapshot = match tail {
        Some(t) => snapshot.paged(t as usize, None),
        None => snapshot,
    };
    let frame = SessionEvent::Snapshot(snapshot);
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = sink.send(Message::Text(text.into())).await;
    }

    let mut events = session.subscribe();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if read_only {
                            continue;
                        }
                        match serde_json::from_str::<CloudCommand>(&text) {
                            Ok(CloudCommand::Prompt { message }) => {
                                if let Err(e) = session.prompt(message).await {
                                    tracing::warn!(%e, "prompt failed");
                                }
                            }
                            Ok(CloudCommand::Abort) => {
                                if let Err(e) = session.abort().await {
                                    tracing::warn!(%e, "abort failed");
                                }
                            }
                            Ok(CloudCommand::AnswerUi { request_id, response }) => {
                                if let Err(e) = session.answer_ui(request_id, response).await {
                                    tracing::warn!(%e, "answer_ui failed");
                                }
                            }
                            Ok(CloudCommand::SetModel { provider, model_id }) => {
                                if let Err(e) = session.set_model(provider, model_id).await {
                                    tracing::warn!(%e, "set_model failed");
                                }
                            }
                            Ok(CloudCommand::SetThinking { level }) => {
                                if let Err(e) = session.set_thinking_level(level).await {
                                    tracing::warn!(%e, "set_thinking failed");
                                }
                            }
                            Ok(CloudCommand::GetState) => match session.get_state().await {
                                Ok(state) => {
                                    let frame = SessionEvent::StateChanged;
                                    let models = session.available_models().await.unwrap_or_default();
                                    let mut ev = serde_json::to_value(&frame)
                                        .unwrap_or(serde_json::Value::Null);
                                    if let Some(map) = ev.as_object_mut() {
                                        map.insert(
                                            "state".into(),
                                            serde_json::to_value(&state).unwrap_or(serde_json::Value::Null),
                                        );
                                        map.insert(
                                            "models".into(),
                                            serde_json::to_value(&models).unwrap_or(serde_json::Value::Null),
                                        );
                                    }
                                    if let Ok(text) = serde_json::to_string(&ev) {
                                        let _ = sink.send(Message::Text(text.into())).await;
                                    }
                                }
                                Err(e) => tracing::warn!(%e, "get_state failed"),
                            },
                            Ok(CloudCommand::GetSnapshot { limit, before }) => {
                                let snapshot = session.snapshot().await;
                                let snapshot = match limit {
                                    Some(l) => snapshot.paged(l as usize, before),
                                    None => snapshot,
                                };
                                let frame = SessionEvent::Snapshot(snapshot);
                                if let Ok(text) = serde_json::to_string(&frame) {
                                    let _ = sink.send(Message::Text(text.into())).await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%e, "invalid CloudCommand");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(ev) => {
                        if let SessionEvent::SessionInfo { title: t, .. } = &ev {
                            if !t.is_empty() {
                                title = Some(t.clone());
                            }
                        }
                        let exiting = matches!(ev, SessionEvent::ProcessExited { .. });
                        match serde_json::to_string(&ev) {
                            Ok(json) => {
                                if sink.send(Message::Text(json.into())).await.is_err() && !exiting {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!(%e, "serialize SessionEvent"),
                        }
                        if exiting {
                            let snap = session.snapshot().await;
                            let db = db_path.clone();
                            let sid = session_id.clone();
                            let title_owned = title.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                persist_share_archive(&db, &sid, &snap, title_owned.as_deref())
                            })
                            .await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session event lag");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn temp_share_db() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cascade-share-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        init_share_tables(&conn).unwrap();
        path
    }

    fn mint_default(path: &std::path::Path, session_id: &str) -> String {
        let exp = expires_at_from_hours(Some(24.0), Utc::now());
        mint_share(path, session_id, exp.as_deref()).unwrap()
    }

    #[test]
    fn mint_lookup_revoke_share() {
        let path = temp_share_db();
        let token = mint_default(&path, "sess-1");
        assert_eq!(
            share_session_for_token(&path, &token).unwrap().as_deref(),
            Some("sess-1")
        );
        assert_eq!(
            share_token_for_session(&path, "sess-1").unwrap().as_deref(),
            Some(token.as_str())
        );
        assert!(share_matches(&path, &token, "sess-1").unwrap());
        assert!(!share_matches(&path, &token, "other").unwrap());
        revoke_shares(&path, "sess-1").unwrap();
        assert!(share_session_for_token(&path, &token).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reminting_replaces_old_token() {
        let path = temp_share_db();
        let first = mint_default(&path, "sess-1");
        let second = mint_default(&path, "sess-1");
        assert_ne!(first, second);
        assert!(share_session_for_token(&path, &first).unwrap().is_none());
        assert_eq!(
            share_session_for_token(&path, &second).unwrap().as_deref(),
            Some("sess-1")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn share_url_uses_forwarded_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("wickrunner.com:7701"),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            share_url(&headers, "tok"),
            "https://wickrunner.com:7701/s/tok"
        );
        assert_eq!(share_url(&HeaderMap::new(), "tok"), "/s/tok");
    }

    #[test]
    fn prefers_html_negotiation() {
        assert!(!prefers_html(None));
        assert!(!prefers_html(Some("")));
        assert!(!prefers_html(Some("application/json")));
        assert!(!prefers_html(Some("*/*")));
        assert!(prefers_html(Some(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )));
        assert!(prefers_html(Some("text/html,application/json;q=0.9")));
        assert!(!prefers_html(Some("application/json,text/html;q=0.8")));
        assert!(!prefers_html(Some("text/html,application/json")));
    }

    #[test]
    fn share_expiry_check() {
        let now = Utc::now();
        assert!(!share_is_expired(None, now));
        assert!(!share_is_expired(Some(""), now));
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        assert!(!share_is_expired(Some(&future), now));
        assert!(share_is_expired(Some(&past), now));
        assert!(share_is_expired(Some(&now.to_rfc3339()), now));
        assert!(share_is_expired(Some("not-a-date"), now));
    }

    #[test]
    fn rolling_snapshot_archives_on_process_exit() {
        let mut snapshot = SessionSnapshot::default();
        let mut title = None;
        let message = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "done"}]
        });
        apply_share_event(
            &mut snapshot,
            &mut title,
            &SessionEvent::MessageEnd {
                message: message.clone(),
            },
        );
        apply_share_event(
            &mut snapshot,
            &mut title,
            &SessionEvent::ProcessExited { code: Some(0) },
        );
        let archived = serde_json::to_value(ArchivedShare { snapshot, title }).unwrap();
        assert_eq!(
            archived["snapshot"]["messages"].as_array().unwrap().len(),
            1
        );
        assert_eq!(archived["snapshot"]["messages"][0], message);
        assert_eq!(archived["snapshot"]["streaming"], serde_json::json!(false));
    }

    #[test]
    fn expired_share_does_not_match_stream() {
        let path = temp_share_db();
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let token = mint_share(&path, "sess-1", Some(&past)).unwrap();
        assert!(!share_matches(&path, &token, "sess-1").unwrap());
        assert!(matches!(
            share_status(&path, &token, "sess-1").unwrap(),
            ShareMatch::Expired
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn forever_share_never_expires() {
        let path = temp_share_db();
        let token = mint_share(&path, "sess-1", None).unwrap();
        let row = lookup_share_by_token(&path, &token).unwrap().unwrap();
        assert!(row.expires_at.is_none());
        assert!(share_matches(&path, &token, "sess-1").unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrates_existing_shares_to_24h_expiry() {
        let path = std::env::temp_dir().join(format!("cascade-share-mig-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE shares (
                token TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();
        let created = Utc::now() - chrono::Duration::hours(2);
        conn.execute(
            "INSERT INTO shares (token, session_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params!["tok", "sess-1", created.to_rfc3339()],
        )
        .unwrap();
        init_share_tables(&conn).unwrap();
        let exp: Option<String> = conn
            .query_row(
                "SELECT expires_at FROM shares WHERE token = 'tok'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let exp = DateTime::parse_from_rfc3339(&exp.unwrap())
            .unwrap()
            .with_timezone(&Utc);
        let expected = created + chrono::Duration::hours(24);
        assert!((exp - expected).num_seconds().abs() < 3);
        let _ = std::fs::remove_file(&path);
    }
}
