use std::path::{Path, PathBuf};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{FromRequestParts, State},
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    Json,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{json_err, AppState};

const TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub email: String,
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

pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
            email TEXT PRIMARY KEY,
            password_hash TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// Seed users from `CASCADE_ALLOW_PASSWORDS` only when the users table is empty.
pub fn seed_if_empty(conn: &Connection, allow: &[(String, String)]) -> anyhow::Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count > 0 || allow.is_empty() {
        return Ok(());
    }
    for (email, password) in allow {
        let hash = hash_password(password)?;
        conn.execute(
            "INSERT INTO users (email, password_hash) VALUES (?1, ?2)",
            rusqlite::params![email, hash],
        )?;
        tracing::info!(email, "seeded user");
    }
    Ok(())
}

fn hash_password(password: &str) -> anyhow::Result<String> {
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
    let mut stmt = conn.prepare("SELECT password_hash FROM users WHERE email = ?1")?;
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

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, Json<serde_json::Value>)> {
    let email = body.email.trim().to_string();
    if email.is_empty() || body.password.is_empty() {
        return Err(json_err(StatusCode::BAD_REQUEST, "email and password required"));
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
        Ok(AuthUser { email })
    }
}
