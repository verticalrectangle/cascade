use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::{hash_password, issue_token, AuthUser};
use crate::{json_err, AppState};

const REGISTER_MAX_PER_MINUTE: usize = 5;
const REGISTER_WINDOW: Duration = Duration::from_secs(60);
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

static REGISTER_ATTEMPTS: OnceLock<Mutex<HashMap<IpAddr, Vec<Instant>>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RegisterRequest {
    email: String,
    password: String,
    invite: String,
}

fn client_ip(headers: &HeaderMap) -> IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            if let Ok(ip) = first.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }
    IpAddr::from([0, 0, 0, 0])
}

pub fn init_invites(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS invites (
            code TEXT PRIMARY KEY,
            created_by TEXT NOT NULL,
            used_by TEXT
        );",
    )?;
    Ok(())
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !allow_register(client_ip(&headers)) {
        return Err(json_err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"));
    }

    let email = body.email.trim().to_string();
    let invite = body.invite.trim().to_ascii_uppercase();
    if !email_ok(&email) || body.password.len() < 8 {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "invalid email or password",
        ));
    }
    if invite.is_empty() {
        return Err(json_err(StatusCode::FORBIDDEN, "invalid invite"));
    }

    let db_path = state.db_path.clone();
    let password = body.password.clone();
    let email_for_db = email.clone();
    tokio::task::spawn_blocking(move || register_user(&db_path, &email_for_db, &password, &invite))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| match e {
            RegisterError::Invite => json_err(StatusCode::FORBIDDEN, "invalid invite"),
            RegisterError::EmailTaken => json_err(StatusCode::CONFLICT, "email already registered"),
            RegisterError::Other(msg) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &msg),
        })?;

    let token = issue_token(&state.jwt_secret, &email)
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(serde_json::json!({ "token": token })))
}

pub async fn mint_invite(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !user.is_admin {
        return Err(json_err(StatusCode::FORBIDDEN, "admin only"));
    }
    let db_path = state.db_path.clone();
    let uid = user.uid.clone();
    let code = tokio::task::spawn_blocking(move || insert_invite(&db_path, &uid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    Ok(Json(serde_json::json!({ "code": code })))
}

fn email_ok(email: &str) -> bool {
    email.len() >= 5 && email.contains('@')
}

fn allow_register(ip: IpAddr) -> bool {
    let mut map = REGISTER_ATTEMPTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let stamps = map.entry(ip).or_default();
    stamps.retain(|t| now.saturating_duration_since(*t) < REGISTER_WINDOW);
    if stamps.len() >= REGISTER_MAX_PER_MINUTE {
        return false;
    }
    stamps.push(now);
    true
}

#[derive(Debug)]
enum RegisterError {
    Invite,
    EmailTaken,
    Other(String),
}

impl From<anyhow::Error> for RegisterError {
    fn from(e: anyhow::Error) -> Self {
        RegisterError::Other(e.to_string())
    }
}

impl From<rusqlite::Error> for RegisterError {
    fn from(e: rusqlite::Error) -> Self {
        if is_constraint(&e) {
            RegisterError::EmailTaken
        } else {
            RegisterError::Other(e.to_string())
        }
    }
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn register_user(
    db_path: &Path,
    email: &str,
    password: &str,
    invite: &str,
) -> Result<(), RegisterError> {
    let mut conn = Connection::open(db_path)?;
    let tx = conn.transaction()?;

    let used_by: Option<Option<String>> = tx
        .query_row(
            "SELECT used_by FROM invites WHERE code = ?1",
            rusqlite::params![invite],
            |r| r.get(0),
        )
        .optional()?;
    match used_by {
        Some(None) => {}
        Some(Some(_)) | None => return Err(RegisterError::Invite),
    }

    let taken: i64 = tx.query_row(
        "SELECT COUNT(*) FROM users WHERE email = ?1",
        rusqlite::params![email],
        |r| r.get(0),
    )?;
    if taken > 0 {
        return Err(RegisterError::EmailTaken);
    }

    let uid = Uuid::new_v4().to_string();
    let hash = hash_password(password)?;
    let created_at = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO users (id, email, pass_hash, is_admin, created_at)
         VALUES (?1, ?2, ?3, 0, ?4)",
        rusqlite::params![uid, email, hash, created_at],
    )?;

    let n = tx.execute(
        "UPDATE invites SET used_by = ?1 WHERE code = ?2 AND used_by IS NULL",
        rusqlite::params![uid, invite],
    )?;
    if n == 0 {
        return Err(RegisterError::Invite);
    }

    tx.commit()?;
    Ok(())
}

fn insert_invite(db_path: &Path, uid: &str) -> Result<String, String> {
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    for _ in 0..8 {
        let code = generate_invite_code();
        match conn.execute(
            "INSERT INTO invites (code, created_by, used_by) VALUES (?1, ?2, NULL)",
            rusqlite::params![code, uid],
        ) {
            Ok(_) => return Ok(code),
            Err(e) if is_constraint(&e) => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("could not allocate invite code".into())
}

fn generate_invite_code() -> String {
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(19);
    for g in 0..4 {
        if g > 0 {
            out.push('-');
        }
        for _ in 0..4 {
            let i = rng.gen_range(0..INVITE_ALPHABET.len());
            out.push(INVITE_ALPHABET[i] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    use std::path::PathBuf;

    fn temp_users_db() -> (PathBuf, Connection) {
        let path = std::env::temp_dir().join(format!("cascade-reg-{}.db", Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (
                id TEXT PRIMARY KEY,
                email TEXT UNIQUE NOT NULL,
                pass_hash TEXT NOT NULL,
                is_admin INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();
        init_invites(&conn).unwrap();
        (path, conn)
    }

    fn insert_user(conn: &Connection, email: &str, is_admin: i64) -> String {
        let uid = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO users (id, email, pass_hash, is_admin, created_at)
             VALUES (?1, ?2, 'x', ?3, 'now')",
            rusqlite::params![uid, email, is_admin],
        )
        .unwrap();
        uid
    }

    #[test]
    fn rejects_short_or_atless_email() {
        assert!(!email_ok("a@b"));
        assert!(!email_ok("abcde"));
        assert!(email_ok("a@bcd"));
    }

    #[test]
    fn invite_code_is_four_groups() {
        let code = generate_invite_code();
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 4);
        for part in parts {
            assert_eq!(part.len(), 4);
            assert!(part.bytes().all(|b| INVITE_ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn rate_limit_caps_five_per_ip() {
        let ip: IpAddr = format!("203.0.113.{}", rand::thread_rng().gen_range(1..250))
            .parse()
            .unwrap();
        for _ in 0..REGISTER_MAX_PER_MINUTE {
            assert!(allow_register(ip));
        }
        assert!(!allow_register(ip));
    }

    #[test]
    fn x_forwarded_for_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        let ip = client_ip(&headers);
        assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn password_hash_matches_login_verifier() {
        let hash = hash_password("s3cret!!").unwrap();
        let parsed = PasswordHash::new(&hash).unwrap();
        assert!(Argon2::default()
            .verify_password(b"s3cret!!", &parsed)
            .is_ok());
    }

    #[test]
    fn mint_and_register_consumes_invite() {
        let (path, conn) = temp_users_db();
        let admin = insert_user(&conn, "admin@wickrunner.com", 1);
        let code = insert_invite(&path, &admin).unwrap();
        assert_eq!(code.split('-').count(), 4);

        register_user(&path, "new@wickrunner.com", "password1", &code).unwrap();
        let used: String = conn
            .query_row(
                "SELECT used_by FROM invites WHERE code = ?1",
                rusqlite::params![code],
                |r| r.get(0),
            )
            .unwrap();
        let uid: String = conn
            .query_row(
                "SELECT id FROM users WHERE email = ?1",
                rusqlite::params!["new@wickrunner.com"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(used, uid);

        match register_user(&path, "other@wickrunner.com", "password1", &code) {
            Err(RegisterError::Invite) => {}
            other => panic!("expected Invite, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn register_rejects_bad_invite_and_dup_email() {
        let (path, conn) = temp_users_db();
        let admin = insert_user(&conn, "admin@wickrunner.com", 1);
        match register_user(
            &path,
            "a@wickrunner.com",
            "password1",
            "NOPE-NOPE-NOPE-NOPE",
        ) {
            Err(RegisterError::Invite) => {}
            other => panic!("expected Invite, got {other:?}"),
        }

        let code1 = insert_invite(&path, &admin).unwrap();
        let code2 = insert_invite(&path, &admin).unwrap();
        register_user(&path, "dup@wickrunner.com", "password1", &code1).unwrap();
        match register_user(&path, "dup@wickrunner.com", "password1", &code2) {
            Err(RegisterError::EmailTaken) => {}
            other => panic!("expected EmailTaken, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
