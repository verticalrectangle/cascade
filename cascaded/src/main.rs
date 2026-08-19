//! cascaded — cloud and desktop daemon (one binary, two roles).
//!
//! # Environment
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `CASCADE_ROLE` | `cloud` | `cloud` or `desktop` |
//! | `CASCADE_BIND` | `0.0.0.0:7700` | HTTP bind (cloud role) |
//! | `CASCADE_DB` | `cascade.db` | SQLite path (users, machines, session registry) |
//! | `CASCADE_JWT_SECRET` | (required for cloud) | HS256 secret for login tokens |
//! | `CASCADE_ALLOW_PASSWORDS` | empty | First-boot users: `email:password,email2:pass2` |
//! | `CASCADE_CLOUD_URL` | — | Desktop: cloud base URL (`https://host`) |
//! | `CASCADE_MACHINE_NAME` | — | Desktop: display name registered on `/relay` |
//! | `CASCADE_MACHINE_TOKEN` | — | Desktop: shared secret; stable machine id |
//! | `CASCADE_TERMINAL_TOKEN` | — | Shared bearer for POST/DELETE `/register-terminal` (`X-Cascade-Token`) |
//!
//! Args override env: `--role cloud`, `--bind 0.0.0.0:7700`, `--db ./cascade.db`.
//!
//! # systemd --user (cloud)
//!
//! ```ini
//! # ~/.config/systemd/user/cascaded.service
//! [Unit]
//! Description=Cascade daemon
//! After=network-online.target
//!
//! [Service]
//! ExecStart=%h/.local/bin/cascaded
//! Environment=CASCADE_ROLE=cloud
//! Environment=CASCADE_BIND=0.0.0.0:7700
//! Environment=CASCADE_DB=%h/.local/share/cascade/cascade.db
//! Environment=CASCADE_JWT_SECRET=change-me
//! Environment=CASCADE_ALLOW_PASSWORDS=alexis@wickrunner.com:password
//! Restart=on-failure
//!
//! [Install]
//! WantedBy=default.target
//! ```
//!
//! Desktop unit: set `CASCADE_ROLE=desktop`, `CASCADE_CLOUD_URL`,
//! `CASCADE_MACHINE_NAME`, `CASCADE_MACHINE_TOKEN`. SIGTERM runs
//! `SessionManager::shutdown_all`.

mod auth;
mod desktop;
mod relay;
mod routes;
mod terminal;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use cascade_core::{SessionManager, SessionRegistry};
use relay::RelayRouter;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Cloud,
    Desktop,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub role: Role,
    pub bind: SocketAddr,
    pub db: PathBuf,
    pub jwt_secret: String,
    pub cloud_url: Option<String>,
    pub machine_name: Option<String>,
    pub machine_token: Option<String>,
    pub allow_passwords: Vec<(String, String)>,
    pub terminal_token: String,
}

#[derive(Clone)]
pub struct AppState {
    pub jwt_secret: String,
    pub db_path: PathBuf,
    pub sessions: SessionManager,
    pub relay: RelayRouter,
    pub terminal_token: String,
}

pub fn json_err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn parse_allow_passwords(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (email, password) = entry.split_once(':')?;
            let email = email.trim();
            let password = password.trim();
            if email.is_empty() || password.is_empty() {
                return None;
            }
            Some((email.to_string(), password.to_string()))
        })
        .collect()
}

impl Config {
    fn from_env_args() -> anyhow::Result<Self> {
        let mut role = env_opt("CASCADE_ROLE").unwrap_or_else(|| "cloud".into());
        let mut bind = env_opt("CASCADE_BIND").unwrap_or_else(|| "0.0.0.0:7700".into());
        let mut db = env_opt("CASCADE_DB").unwrap_or_else(|| "cascade.db".into());
        let mut jwt_secret = env_opt("CASCADE_JWT_SECRET");
        let mut cloud_url = env_opt("CASCADE_CLOUD_URL");
        let mut machine_name = env_opt("CASCADE_MACHINE_NAME");
        let mut machine_token = env_opt("CASCADE_MACHINE_TOKEN");
        let mut allow = env_opt("CASCADE_ALLOW_PASSWORDS").unwrap_or_default();
        let mut terminal_token = env_opt("CASCADE_TERMINAL_TOKEN").unwrap_or_default();

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let (key, val) = if let Some(rest) = arg.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    (k.to_string(), Some(v.to_string()))
                } else {
                    (rest.to_string(), args.next())
                }
            } else {
                anyhow::bail!("unexpected argument: {arg}");
            };
            match key.as_str() {
                "role" => role = val.ok_or_else(|| anyhow::anyhow!("--role needs a value"))?,
                "bind" => bind = val.ok_or_else(|| anyhow::anyhow!("--bind needs a value"))?,
                "db" => db = val.ok_or_else(|| anyhow::anyhow!("--db needs a value"))?,
                "jwt-secret" => {
                    jwt_secret = Some(val.ok_or_else(|| anyhow::anyhow!("--jwt-secret needs a value"))?)
                }
                "cloud-url" => {
                    cloud_url = Some(val.ok_or_else(|| anyhow::anyhow!("--cloud-url needs a value"))?)
                }
                "machine-name" => {
                    machine_name =
                        Some(val.ok_or_else(|| anyhow::anyhow!("--machine-name needs a value"))?)
                }
                "machine-token" => {
                    machine_token =
                        Some(val.ok_or_else(|| anyhow::anyhow!("--machine-token needs a value"))?)
                }
                "allow-passwords" => {
                    allow = val.ok_or_else(|| anyhow::anyhow!("--allow-passwords needs a value"))?
                }
                "terminal-token" => {
                    terminal_token = val.ok_or_else(|| anyhow::anyhow!("--terminal-token needs a value"))?
                }
                other => anyhow::bail!("unknown flag --{other}"),
            }
        }

        let role = match role.to_ascii_lowercase().as_str() {
            "cloud" => Role::Cloud,
            "desktop" => Role::Desktop,
            other => anyhow::bail!("CASCADE_ROLE must be cloud or desktop, got {other}"),
        };

        let jwt_secret = match jwt_secret {
            Some(s) => s,
            None if role == Role::Cloud => {
                anyhow::bail!("CASCADE_JWT_SECRET is required for cloud role")
            }
            None => String::new(),
        };

        Ok(Config {
            role,
            bind: bind.parse()?,
            db: PathBuf::from(db),
            jwt_secret,
            cloud_url,
            machine_name,
            machine_token,
            allow_passwords: parse_allow_passwords(&allow),
            terminal_token,
        })
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info,cascaded=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutdown signal received");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cfg = Config::from_env_args()?;

    let (shut_tx, shut_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shut_tx.send(true);
    });

    match cfg.role {
        Role::Cloud => run_cloud(cfg, shut_rx).await,
        Role::Desktop => desktop::run_desktop(cfg, shut_rx).await,
    }
}

async fn run_cloud(cfg: Config, mut shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
    if let Some(parent) = cfg.db.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    {
        let conn = rusqlite::Connection::open(&cfg.db)?;
        auth::init_tables(&conn)?;
        auth::seed_if_empty(&conn, &cfg.allow_passwords)?;
        relay::init_tables(&conn)?;
        terminal::init_tables(&conn)?;
    }

    let registry = SessionRegistry::open(&cfg.db)?;
    let sessions = SessionManager::new(registry);
    let relay = RelayRouter::new(cfg.db.clone())?;

    let state = AppState {
        jwt_secret: cfg.jwt_secret.clone(),
        db_path: cfg.db.clone(),
        sessions: sessions.clone(),
        relay,
        terminal_token: cfg.terminal_token.clone(),
    };

    let app = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/machines", get(routes::list_machines))
        .route(
            "/sessions",
            get(routes::list_sessions).post(routes::create_session),
        )
        .route("/sessions/{id}", axum::routing::delete(routes::delete_session))
        .route("/sessions/{id}/stream", get(routes::session_stream))
        .route("/relay", get(relay::relay_ws))
        .route(
            "/register-terminal",
            post(terminal::register).delete(terminal::unregister),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, db = %cfg.db.display(), "cascaded cloud listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;

    sessions.shutdown_all().await;
    Ok(())
}
