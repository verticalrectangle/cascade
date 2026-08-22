use std::path::{Path, PathBuf};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{FromRequestParts, State},
    http::{header::AUTHORIZATION, request::Parts, HeaderMap, StatusCode},
    Json,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{json_err, AppState};

const TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct AuthUser {
    #[allow(dead_code)]
    pub email: String,
    pub uid: String,
    pub is_admin: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
}

fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn has_column(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    Ok(table_columns(conn, table)?.iter().any(|c| c == column))
}

pub fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> anyhow::Result<()> {
    if !has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

fn users_schema_current(cols: &[String]) -> bool {
    cols.iter().any(|c| c == "id")
        && cols.iter().any(|c| c == "email")
        && cols.iter().any(|c| c == "pass_hash")
        && cols.iter().any(|c| c == "is_admin")
        && cols.iter().any(|c| c == "created_at")
}

fn create_users_table(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            email TEXT UNIQUE NOT NULL,
            pass_hash TEXT NOT NULL,
            is_admin INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );",
    )?;
    Ok(())
}

fn migrate_users_table(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "users")? {
        create_users_table(conn)?;
        return Ok(());
    }
    let cols = table_columns(conn, "users")?;
    if cols.is_empty() {
        create_users_table(conn)?;
        return Ok(());
    }
    if users_schema_current(&cols) {
        return Ok(());
    }

    let hash_col = if cols.iter().any(|c| c == "pass_hash") {
        "pass_hash"
    } else if cols.iter().any(|c| c == "password_hash") {
        "password_hash"
    } else {
        anyhow::bail!("users table missing password hash column");
    };
    let has_id = cols.iter().any(|c| c == "id");
    let has_admin = cols.iter().any(|c| c == "is_admin");
    let has_created = cols.iter().any(|c| c == "created_at");

    conn.execute_batch(
        "CREATE TABLE users_new (
            id TEXT PRIMARY KEY,
            email TEXT UNIQUE NOT NULL,
            pass_hash TEXT NOT NULL,
            is_admin INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );",
    )?;

    let select = format!(
        "SELECT email, {hash_col}{}{}{} FROM users",
        if has_id { ", id" } else { "" },
        if has_admin { ", is_admin" } else { "" },
        if has_created { ", created_at" } else { "" },
    );
    let mut stmt = conn.prepare(&select)?;
    let mut rows = stmt.query([])?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut copied: Vec<(String, String, String, i64, String)> = Vec::new();
    while let Some(row) = rows.next()? {
        let email: String = row.get(0)?;
        let pass_hash: String = row.get(1)?;
        let mut idx = 2;
        let id = if has_id {
            let v: String = row.get(idx)?;
            idx += 1;
            if v.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                v
            }
        } else {
            Uuid::new_v4().to_string()
        };
        let is_admin: i64 = if has_admin {
            let v: i64 = row.get(idx)?;
            idx += 1;
            v
        } else {
            0
        };
        let created_at = if has_created {
            let v: String = row.get(idx)?;
            if v.is_empty() {
                now.clone()
            } else {
                v
            }
        } else {
            now.clone()
        };
        copied.push((id, email, pass_hash, is_admin, created_at));
    }
    drop(rows);
    drop(stmt);
    for (id, email, pass_hash, is_admin, created_at) in copied {
        conn.execute(
            "INSERT INTO users_new (id, email, pass_hash, is_admin, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, email, pass_hash, is_admin, created_at],
        )?;
    }
    conn.execute_batch("DROP TABLE users; ALTER TABLE users_new RENAME TO users;")?;
    Ok(())
}

pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    migrate_users_table(conn)?;
    create_users_table(conn)?;
    Ok(())
}

/// Seed users from `CASCADE_ALLOW_PASSWORDS` only when the users table is empty.
pub fn seed_if_empty(conn: &Connection, allow: &[(String, String)]) -> anyhow::Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count > 0 || allow.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().to_rfc3339();
    for (email, password) in allow {
        let hash = hash_password(password)?;
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO users (id, email, pass_hash, is_admin, created_at) VALUES (?1, ?2, ?3, 1, ?4)",
            rusqlite::params![id, email, hash, now],
        )?;
        tracing::info!(email, "seeded user");
    }
    Ok(())
}

/// Mark every `CASCADE_ALLOW_PASSWORDS` email as admin (existing rows keep their hash).
pub fn promote_seeded_admins(conn: &Connection, allow: &[(String, String)]) -> anyhow::Result<()> {
    for (email, _) in allow {
        conn.execute(
            "UPDATE users SET is_admin = 1 WHERE email = ?1",
            rusqlite::params![email],
        )?;
    }
    Ok(())
}

pub fn resolve_user(conn: &Connection, email: &str) -> Option<(String, bool)> {
    conn.query_row(
        "SELECT id, is_admin FROM users WHERE email = ?1",
        [email],
        |r| {
            let uid: String = r.get(0)?;
            let admin: i64 = r.get(1)?;
            Ok((uid, admin != 0))
        },
    )
    .ok()
}

pub fn seeded_uid(conn: &Connection, allow: &[(String, String)]) -> Option<String> {
    for (email, _) in allow {
        if let Some((uid, _)) = resolve_user(conn, email) {
            return Some(uid);
        }
    }
    conn.query_row(
        "SELECT id FROM users WHERE is_admin = 1 ORDER BY created_at ASC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .ok()
}

pub fn admin_uid(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT id FROM users WHERE is_admin = 1 ORDER BY created_at ASC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// Assign pre-existing rows with NULL/empty owner to `uid`.
pub fn assign_unowned(conn: &Connection, uid: &str) -> anyhow::Result<()> {
    if table_exists(conn, "machines")? && has_column(conn, "machines", "account")? {
        let _ = conn.execute(
            "UPDATE machines SET owner = account WHERE (owner IS NULL OR owner = '') AND account != ''",
            [],
        );
    }
    for table in [
        "sessions",
        "machines",
        "machine_sessions",
        "terminal_sessions",
    ] {
        if table_exists(conn, table)? && has_column(conn, table, "owner")? {
            conn.execute(
                &format!("UPDATE {table} SET owner = ?1 WHERE owner IS NULL OR owner = ''"),
                [uid],
            )?;
        }
    }
    Ok(())
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))?;
    Ok(hash.to_string())
}

fn verify_password(hash: &str, password: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn lookup_hash(db_path: &Path, email: &str) -> anyhow::Result<Option<String>> {
    let conn = Connection::open(db_path)?;
    let mut stmt = conn.prepare("SELECT pass_hash FROM users WHERE email = ?1")?;
    let mut rows = stmt.query(rusqlite::params![email])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

pub fn issue_token(secret: &str, email: &str) -> anyhow::Result<String> {
    let exp = (chrono::Utc::now().timestamp() + TOKEN_TTL_SECS) as usize;
    let claims = Claims {
        sub: email.to_string(),
        exp,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| anyhow::anyhow!("jwt encode: {e}"))
}

pub fn verify_token(secret: &str, token: &str) -> anyhow::Result<String> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| anyhow::anyhow!("jwt: {e}"))?;
    Ok(data.claims.sub)
}

pub fn bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, Json<serde_json::Value>)> {
    let email = body.email.trim().to_string();
    if email.is_empty() || body.password.is_empty() {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "email and password required",
        ));
    }
    let db_path: PathBuf = state.db_path.clone();
    let password = body.password.clone();
    let email_for_lookup = email.clone();
    let hash = tokio::task::spawn_blocking(move || lookup_hash(&db_path, &email_for_lookup))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let Some(hash) = hash else {
        return Err(json_err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    };

    let password_ok = tokio::task::spawn_blocking(move || verify_password(&hash, &password))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if !password_ok {
        return Err(json_err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    }

    let token = issue_token(&state.jwt_secret, &email)
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(LoginResponse { token }))
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| json_err(StatusCode::UNAUTHORIZED, "missing authorization"))?;
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| json_err(StatusCode::UNAUTHORIZED, "expected Bearer token"))?;
        let email = verify_token(&state.jwt_secret, token)
            .map_err(|_| json_err(StatusCode::UNAUTHORIZED, "invalid token"))?;
        let db_path = state.db_path.clone();
        let email_lookup = email.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            Ok::<_, anyhow::Error>(resolve_user(&conn, &email_lookup))
        })
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        let Some((uid, is_admin)) = resolved else {
            return Err(json_err(StatusCode::UNAUTHORIZED, "invalid token"));
        };
        tracing::debug!(%email, %uid, is_admin, "resolved auth user");
        Ok(AuthUser {
            email,
            uid,
            is_admin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_users_and_preserves_password() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (
                email TEXT PRIMARY KEY,
                password_hash TEXT NOT NULL
            );",
        )
        .unwrap();
        let hash = hash_password("secret").unwrap();
        conn.execute(
            "INSERT INTO users (email, password_hash) VALUES (?1, ?2)",
            rusqlite::params!["alexis@wickrunner.com", hash],
        )
        .unwrap();

        init_tables(&conn).unwrap();
        promote_seeded_admins(&conn, &[("alexis@wickrunner.com".into(), "secret".into())]).unwrap();

        let stored: String = conn
            .query_row(
                "SELECT pass_hash FROM users WHERE email = ?1",
                ["alexis@wickrunner.com"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(verify_password(&stored, "secret"));
        let (uid, is_admin) = resolve_user(&conn, "alexis@wickrunner.com").expect("user");
        assert!(!uid.is_empty());
        assert!(is_admin);
        assert_eq!(
            seeded_uid(&conn, &[("alexis@wickrunner.com".into(), "x".into())]).as_deref(),
            Some(uid.as_str())
        );
    }

    #[test]
    fn seed_empty_creates_admin() {
        let conn = Connection::open_in_memory().unwrap();
        init_tables(&conn).unwrap();
        seed_if_empty(&conn, &[("alexis@wickrunner.com".into(), "pw".into())]).unwrap();
        let (_, is_admin) = resolve_user(&conn, "alexis@wickrunner.com").unwrap();
        assert!(is_admin);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        seed_if_empty(&conn, &[("other@example.com".into(), "pw".into())]).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn assign_unowned_fills_empty_owners() {
        let conn = Connection::open_in_memory().unwrap();
        init_tables(&conn).unwrap();
        seed_if_empty(&conn, &[("alexis@wickrunner.com".into(), "pw".into())]).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, owner TEXT NOT NULL DEFAULT '');
             CREATE TABLE machines (id TEXT PRIMARY KEY, owner TEXT NOT NULL DEFAULT '', account TEXT NOT NULL DEFAULT '');
             INSERT INTO sessions (id, owner) VALUES ('s1', '');
             INSERT INTO machines (id, owner, account) VALUES ('m1', '', 'legacy');",
        )
        .unwrap();
        let uid = seeded_uid(&conn, &[("alexis@wickrunner.com".into(), "pw".into())]).unwrap();
        assign_unowned(&conn, &uid).unwrap();
        let s: String = conn
            .query_row("SELECT owner FROM sessions WHERE id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let m: String = conn
            .query_row("SELECT owner FROM machines WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(s, uid);
        assert_eq!(m, "legacy");
        assign_unowned(&conn, &uid).unwrap();
        let m2: String = conn
            .query_row("SELECT owner FROM machines WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(m2, "legacy");
    }
}
