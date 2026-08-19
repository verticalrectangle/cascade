use cascade_core::{
    CloudClient, CloudCommand, OmpSession, SessionEvent, SessionManager, SessionMeta,
    SessionRegistry, SessionSnapshot, SpawnOptions, UiAnswer,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Local,
    Cloud,
}

pub enum SessionBackend {
    Local(OmpSession),
    Cloud {
        session_id: String,
        cmd: mpsc::UnboundedSender<CloudCommand>,
    },
}

impl SessionBackend {
    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Local(_) => BackendKind::Local,
            Self::Cloud { .. } => BackendKind::Cloud,
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Local(s) => s.id().to_string(),
            Self::Cloud { session_id, .. } => session_id.clone(),
        }
    }

    pub async fn prompt(&self, message: String) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.prompt(message).await,
            Self::Cloud { cmd, .. } => {
                cmd.send(CloudCommand::Prompt { message })?;
                Ok(())
            }
        }
    }

    pub async fn abort(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.abort().await,
            Self::Cloud { cmd, .. } => {
                cmd.send(CloudCommand::Abort)?;
                Ok(())
            }
        }
    }

    pub async fn answer_ui(&self, request_id: String, response: UiAnswer) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.answer_ui(request_id, response).await,
            Self::Cloud { cmd, .. } => {
                cmd.send(CloudCommand::AnswerUi {
                    request_id,
                    response,
                })?;
                Ok(())
            }
        }
    }
}

pub enum Cmd {
    Login {
        email: String,
        password: String,
    },
    Logout,
    SaveCloudUrl(String),
    RefreshSessions,
    NewSession {
        kind: BackendKind,
        cwd: String,
        model: Option<String>,
    },
    OpenSession {
        id: String,
        kind: BackendKind,
    },
    Prompt(String),
    Abort,
    Answer {
        request_id: String,
        response: UiAnswer,
    },
    RefreshState,
}

pub enum UiMsg {
    NeedLogin {
        error: Option<String>,
    },
    LoggedIn {
        url: String,
    },
    SessionList(Vec<SessionMeta>),
    Attached {
        id: String,
        kind: BackendKind,
        snapshot: Option<SessionSnapshot>,
    },
    Event(SessionEvent),
    Toast(String),
    Error(String),
    LoggedOut,
}

pub async fn worker(cmd_rx: async_channel::Receiver<Cmd>, ui_tx: async_channel::Sender<UiMsg>) {
    let mut settings = Settings::load();
    let _ = std::fs::create_dir_all(Settings::config_dir());
    let registry = match SessionRegistry::open(&Settings::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            let _ = ui_tx
                .send(UiMsg::Error(format!("registry: {e:#}")))
                .await;
            return;
        }
    };
    let manager = SessionManager::new(registry);
    let mut cloud: Option<CloudClient> = None;
    let mut current: Option<SessionBackend> = None;
    let mut pump: Option<AbortHandle> = None;

    if let Some(token) = settings.token.clone() {
        match CloudClient::connect(&settings.cloud_url, &token).await {
            Ok(c) => {
                cloud = Some(c);
                let _ = ui_tx
                    .send(UiMsg::LoggedIn {
                        url: settings.cloud_url.clone(),
                    })
                    .await;
                push_sessions(&manager, cloud.as_ref(), &ui_tx).await;
            }
            Err(e) => {
                settings.token = None;
                let _ = settings.save();
                let _ = ui_tx
                    .send(UiMsg::NeedLogin {
                        error: Some(format!("session expired: {e:#}")),
                    })
                    .await;
            }
        }
    } else {
        let _ = ui_tx
            .send(UiMsg::NeedLogin { error: None })
            .await;
    }

    while let Ok(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Login { email, password } => {
                match CloudClient::login(&settings.cloud_url, &email, &password).await {
                    Ok(token) => {
                        settings.token = Some(token.clone());
                        let _ = settings.save();
                        match CloudClient::connect(&settings.cloud_url, &token).await {
                            Ok(c) => {
                                cloud = Some(c);
                                let _ = ui_tx
                                    .send(UiMsg::LoggedIn {
                                        url: settings.cloud_url.clone(),
                                    })
                                    .await;
                                push_sessions(&manager, cloud.as_ref(), &ui_tx).await;
                            }
                            Err(e) => {
                                let _ = ui_tx
                                    .send(UiMsg::NeedLogin {
                                        error: Some(format!("connect failed: {e:#}")),
                                    })
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = ui_tx
                            .send(UiMsg::NeedLogin {
                                error: Some(format!("{e:#}")),
                            })
                            .await;
                    }
                }
            }
            Cmd::Logout => {
                if let Some(h) = pump.take() {
                    h.abort();
                }
                current = None;
                cloud = None;
                settings.token = None;
                let _ = settings.save();
                let _ = ui_tx.send(UiMsg::LoggedOut).await;
            }
            Cmd::SaveCloudUrl(url) => {
                settings.cloud_url = url;
                let _ = settings.save();
            }
            Cmd::RefreshSessions => {
                push_sessions(&manager, cloud.as_ref(), &ui_tx).await;
            }
            Cmd::NewSession { kind, cwd, model } => {
                settings.last_backend = match kind {
                    BackendKind::Local => "local".into(),
                    BackendKind::Cloud => "cloud".into(),
                };
                let _ = settings.save();
                match kind {
                    BackendKind::Local => {
                        let opts = SpawnOptions {
                            cwd: std::path::PathBuf::from(&cwd),
                            model,
                            ..SpawnOptions::default()
                        };
                        match manager.spawn(opts).await {
                            Ok(id) => {
                                if let Some(sess) = manager.get(&id).await {
                                    attach_local(
                                        sess,
                                        &mut current,
                                        &mut pump,
                                        &ui_tx,
                                    )
                                    .await;
                                    push_sessions(&manager, cloud.as_ref(), &ui_tx).await;
                                }
                            }
                            Err(e) => {
                                let _ = ui_tx
                                    .send(UiMsg::Error(format!("spawn local: {e:#}")))
                                    .await;
                            }
                        }
                    }
                    BackendKind::Cloud => {
                        let Some(client) = cloud.as_ref() else {
                            let _ = ui_tx
                                .send(UiMsg::Error("not connected to cloud".into()))
                                .await;
                            continue;
                        };
                        match client
                            .create_session(None, &cwd, model)
                            .await
                        {
                            Ok(id) => match client.attach(&id).await {
                                Ok((ev_rx, cmd_tx)) => {
                                    if let Some(h) = pump.take() {
                                        h.abort();
                                    }
                                    current = Some(SessionBackend::Cloud {
                                        session_id: id.clone(),
                                        cmd: cmd_tx,
                                    });
                                    pump = Some(spawn_mpsc_pump(ev_rx, ui_tx.clone()));
                                    let _ = ui_tx
                                        .send(UiMsg::Attached {
                                            id,
                                            kind: BackendKind::Cloud,
                                            snapshot: None,
                                        })
                                        .await;
                                    push_sessions(&manager, cloud.as_ref(), &ui_tx).await;
                                }
                                Err(e) => {
                                    let _ = ui_tx
                                        .send(UiMsg::Error(format!("attach: {e:#}")))
                                        .await;
                                }
                            },
                            Err(e) => {
                                let _ = ui_tx
                                    .send(UiMsg::Error(format!("create session: {e:#}")))
                                    .await;
                            }
                        }
                    }
                }
            }
            Cmd::OpenSession { id, kind } => match kind {
                BackendKind::Local => {
                    if let Some(sess) = manager.get(&id).await {
                        attach_local(sess, &mut current, &mut pump, &ui_tx).await;
                    } else {
                        let _ = ui_tx
                            .send(UiMsg::Error(format!("local session {id} not running")))
                            .await;
                    }
                }
                BackendKind::Cloud => {
                    let Some(client) = cloud.as_ref() else {
                        let _ = ui_tx
                            .send(UiMsg::Error("not connected to cloud".into()))
                            .await;
                        continue;
                    };
                    match client.attach(&id).await {
                        Ok((ev_rx, cmd_tx)) => {
                            if let Some(h) = pump.take() {
                                h.abort();
                            }
                            current = Some(SessionBackend::Cloud {
                                session_id: id.clone(),
                                cmd: cmd_tx,
                            });
                            pump = Some(spawn_mpsc_pump(ev_rx, ui_tx.clone()));
                            let _ = ui_tx
                                .send(UiMsg::Attached {
                                    id,
                                    kind: BackendKind::Cloud,
                                    snapshot: None,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiMsg::Error(format!("attach: {e:#}"))).await;
                        }
                    }
                }
            },
            Cmd::Prompt(message) => {
                if let Some(b) = current.as_ref() {
                    if let Err(e) = b.prompt(message).await {
                        let _ = ui_tx.send(UiMsg::Error(format!("prompt: {e:#}"))).await;
                    }
                } else {
                    let _ = ui_tx
                        .send(UiMsg::Error("no session attached".into()))
                        .await;
                }
            }
            Cmd::Abort => {
                if let Some(b) = current.as_ref() {
                    if let Err(e) = b.abort().await {
                        let _ = ui_tx.send(UiMsg::Error(format!("abort: {e:#}"))).await;
                    }
                }
            }
            Cmd::Answer {
                request_id,
                response,
            } => {
                if let Some(b) = current.as_ref() {
                    if let Err(e) = b.answer_ui(request_id, response).await {
                        let _ = ui_tx.send(UiMsg::Error(format!("answer: {e:#}"))).await;
                    }
                }
            }
            Cmd::RefreshState => {
                if let Some(SessionBackend::Local(s)) = current.as_ref() {
                    match s.get_state().await {
                        Ok(st) => {
                            let _ = ui_tx
                                .send(UiMsg::Event(SessionEvent::TodoChanged {
                                    phases: st.todo_phases,
                                }))
                                .await;
                        }
                        Err(e) => {
                            let _ = ui_tx
                                .send(UiMsg::Error(format!("get_state: {e:#}")))
                                .await;
                        }
                    }
                }
            }
        }
    }
}

async fn attach_local(
    sess: OmpSession,
    current: &mut Option<SessionBackend>,
    pump: &mut Option<AbortHandle>,
    ui_tx: &async_channel::Sender<UiMsg>,
) {
    if let Some(h) = pump.take() {
        h.abort();
    }
    let id = sess.id().to_string();
    let snapshot = sess.snapshot().await;
    let rx = sess.subscribe();
    *pump = Some(spawn_broadcast_pump(rx, ui_tx.clone()));
    *current = Some(SessionBackend::Local(sess));
    let _ = ui_tx
        .send(UiMsg::Attached {
            id,
            kind: BackendKind::Local,
            snapshot: Some(snapshot),
        })
        .await;
}

fn spawn_broadcast_pump(
    mut rx: tokio::sync::broadcast::Receiver<SessionEvent>,
    ui_tx: async_channel::Sender<UiMsg>,
) -> AbortHandle {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if ui_tx.send(UiMsg::Event(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .abort_handle()
}

fn spawn_mpsc_pump(
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    ui_tx: async_channel::Sender<UiMsg>,
) -> AbortHandle {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if ui_tx.send(UiMsg::Event(ev)).await.is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

async fn push_sessions(
    manager: &SessionManager,
    cloud: Option<&CloudClient>,
    ui_tx: &async_channel::Sender<UiMsg>,
) {
    let mut list = manager.list().await;
    if let Some(c) = cloud {
        match c.list_sessions().await {
            Ok(mut remote) => list.append(&mut remote),
            Err(e) => {
                let _ = ui_tx
                    .send(UiMsg::Error(format!("list cloud sessions: {e:#}")))
                    .await;
            }
        }
    }
    list.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    let _ = ui_tx.send(UiMsg::SessionList(list)).await;
}
