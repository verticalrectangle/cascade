use std::collections::HashMap;

use cascade_core::{
    CloudClient, CloudCommand, ListedSession, OmpSession, SessionEvent, SessionManager,
    SessionRegistry, SessionSnapshot, SpawnOptions, UiAnswer,
};
use cascade_relay::{CollabAttach, GuestCommand};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::settings::Settings;

/// Transcript page size: messages per snapshot page, both directions
/// (initial tail and scroll-up history pages).
pub(crate) const HISTORY_PAGE_U32: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Local,
    Cloud,
    Terminal,
}

pub enum SessionBackend {
    Local(OmpSession),
    Cloud {
        session_id: String,
        cmd: mpsc::UnboundedSender<CloudCommand>,
        /// Dual-channel attach: history rides the proxy stream, prompts ride
        /// the session's collab room. Set when the row has a join_handle.
        guest_cmd: Option<mpsc::Sender<GuestCommand>>,
        /// The room's event pump — must be aborted on teardown or every
        /// re-attach stacks another duplicate renderer of the same events.
        guest_pump: Option<AbortHandle>,
        /// The join_handle the guest channel is connected to — re-resolved
        /// on refresh so a re-registered room reconnects instead of going stale.
        guest_link: Option<String>,
    },
    Terminal {
        session_id: String,
        cmd: mpsc::Sender<GuestCommand>,
    },
}

impl SessionBackend {
    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Local(_) => BackendKind::Local,
            Self::Cloud { .. } => BackendKind::Cloud,
            Self::Terminal { .. } => BackendKind::Terminal,
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Local(s) => s.id().to_string(),
            Self::Cloud { session_id, .. } => session_id.clone(),
            Self::Terminal { session_id, .. } => session_id.clone(),
        }
    }

    pub async fn prompt(&self, message: String) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.prompt(message).await,
            Self::Cloud { cmd, guest_cmd, .. } => {
                if let Some(g) = guest_cmd {
                    return g
                        .send(GuestCommand::Prompt { text: message })
                        .await
                        .map_err(|e| anyhow::anyhow!("guest prompt: {e}"));
                }
                cmd.send(CloudCommand::Prompt { message })?;
                Ok(())
            }
            Self::Terminal { cmd, .. } => {
                cmd.send(GuestCommand::Prompt { text: message })
                    .await
                    .map_err(|e| anyhow::anyhow!("terminal prompt: {e}"))?;
                Ok(())
            }
        }
    }

    pub async fn abort(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.abort().await,
            Self::Cloud { cmd, guest_cmd, .. } => {
                if let Some(g) = guest_cmd {
                    return g
                        .send(GuestCommand::Abort)
                        .await
                        .map_err(|e| anyhow::anyhow!("guest abort: {e}"));
                }
                cmd.send(CloudCommand::Abort)?;
                Ok(())
            }
            Self::Terminal { cmd, .. } => {
                cmd.send(GuestCommand::Abort)
                    .await
                    .map_err(|e| anyhow::anyhow!("terminal abort: {e}"))?;
                Ok(())
            }
        }
    }

    /// Follow-up message while a turn is running (steer). The cloud/terminal
    /// protocols have no steer verb; fall back to a regular prompt there.
    pub async fn steer(&self, message: String) -> anyhow::Result<()> {
        match self {
            Self::Local(s) => s.steer(message).await,
            Self::Cloud { cmd, guest_cmd, .. } => {
                if let Some(g) = guest_cmd {
                    return g
                        .send(GuestCommand::Prompt { text: message })
                        .await
                        .map_err(|e| anyhow::anyhow!("guest steer: {e}"));
                }
                cmd.send(CloudCommand::Prompt { message })?;
                Ok(())
            }
            Self::Terminal { cmd, .. } => {
                cmd.send(GuestCommand::Prompt { text: message })
                    .await
                    .map_err(|e| anyhow::anyhow!("terminal steer: {e}"))?;
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
            Self::Terminal { .. } => Ok(()),
        }
    }
}

pub enum Cmd {
    Login {
        email: String,
        password: String,
    },
    Register {
        email: String,
        password: String,
        invite: String,
    },
    ShareSession,
    UnshareSession,
    /// Paste a view-share URL; worker resolves then [`Cmd::OpenShared`].
    OpenShareLink(String),
    OpenShared {
        session_id: String,
        token: String,
    },
    Logout,
    SaveCloudUrl(String),
    RefreshSessions,
    LoadHistory { before: u64 },
    NewSession {
        kind: BackendKind,
        cwd: String,
        model: Option<String>,
    },
    OpenSession {
        id: String,
        kind: BackendKind,
        join_handle: Option<String>,
        /// Discovered session without a collab room: view-only, prompts
        /// would be dropped server-side. Rail computes this.
        read_only: bool,
    },
    Prompt(String),
    Abort,
    /// Follow-up message while streaming (steer / queue).
    Queue(String),
    /// User opened the inbox dropdown: clear unseen entries and re-broadcast.
    InboxOpen,
    /// Persist the browser-pane URL for the currently attached session.
    PaneToggle(String),
    /// Resolve the Nth session of `kind` from the merged sorted list and open it
    /// (CASCADE_AUTOTEST hook; resolved through the normal OpenSession path).
    AutotestOpen {
        kind: BackendKind,
        index: usize,
    },
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
    ShareLink {
        session_id: String,
        url: String,
    },
    SharingStopped {
        session_id: String,
    },
    SessionList(Vec<ListedSession>),
    MachineNames(std::collections::HashMap<String, String>),
    Attached {
        id: String,
        kind: BackendKind,
        snapshot: Option<SessionSnapshot>,
        read_only: bool,
    },
    Event(SessionEvent),
    Toast(String),
    Error(String),
    /// The attached session cannot accept prompts (guest channel failed on
    /// a discovered row) — disable the composer honestly.
    ReadOnly(bool),
    LoggedOut,
    /// Unseen inbox entry count.
    InboxCount(usize),
    /// Full unseen inbox entry list (for the dropdown).
    InboxItems(Vec<InboxItem>),
    /// Browser-pane URL remembered for the just-attached session.
    PaneUrl(Option<String>),
    /// Live session state (local sessions only; model pill etc).
    SessionState(Box<cascade_core::RpcSessionState>),
}

/// One unseen-inbox entry (turn finished, question pending, error).
#[derive(Clone, Debug)]
pub struct InboxItem {
    pub text: String,
    pub session_id: Option<String>,
}

use std::sync::Arc;

use parking_lot::Mutex;

/// Unseen inbox entries shared between the worker loop and event pumps.
type Inbox = Arc<Mutex<Vec<InboxItem>>>;

fn inbox_push(inbox: &Inbox, ui_tx: &async_channel::Sender<UiMsg>, item: InboxItem) {
    let (n, items) = {
        let mut g = inbox.lock();
        g.push(item);
        (g.len(), g.clone())
    };
    let _ = ui_tx.send_blocking(UiMsg::InboxCount(n));
    let _ = ui_tx.send_blocking(UiMsg::InboxItems(items));
}

fn inbox_clear(inbox: &Inbox, ui_tx: &async_channel::Sender<UiMsg>) {
    inbox.lock().clear();
    let _ = ui_tx.send_blocking(UiMsg::InboxCount(0));
    let _ = ui_tx.send_blocking(UiMsg::InboxItems(Vec::new()));
}

pub async fn worker(
    cmd_rx: async_channel::Receiver<Cmd>,
    ui_tx: async_channel::Sender<UiMsg>,
    cmd_tx: async_channel::Sender<Cmd>,
) {
    let mut settings = Settings::load();
    let _ = std::fs::create_dir_all(Settings::config_dir());
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let registry = match SessionRegistry::open(&Settings::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            let _ = ui_tx.send(UiMsg::Error(format!("registry: {e:#}"))).await;
            return;
        }
    };
    let manager = SessionManager::new(registry);
    let mut cloud: Option<CloudClient> = None;
    let mut current: Option<SessionBackend> = None;
    let mut pump: Option<AbortHandle> = None;
    let mut terminal_links: HashMap<String, String> = HashMap::new();

    if let Some(token) = settings.token.clone() {
        match CloudClient::connect(&settings.cloud_url, &token).await {
            Ok(c) => {
                cloud = Some(c);
                let _ = ui_tx
                    .send(UiMsg::LoggedIn {
                        url: settings.cloud_url.clone(),
                    })
                    .await;
                push_sessions(&manager, cloud.as_ref(), &mut terminal_links, &ui_tx).await;
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
        let _ = ui_tx.send(UiMsg::NeedLogin { error: None }).await;
    }

    while let Ok(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Login { email, password } => {
                match CloudClient::login(&settings.cloud_url, &email, &password).await {
                    Ok(token) => {
                        apply_token(
                            token,
                            &mut settings,
                            &mut cloud,
                            &manager,
                            &mut terminal_links,
                            &ui_tx,
                        )
                        .await;
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
            Cmd::Register {
                email,
                password,
                invite,
            } => {
                match CloudClient::register(&settings.cloud_url, &email, &password, &invite).await {
                    Ok(token) => {
                        apply_token(
                            token,
                            &mut settings,
                            &mut cloud,
                            &manager,
                            &mut terminal_links,
                            &ui_tx,
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = ui_tx
                            .send(UiMsg::NeedLogin {
                                error: Some(e.to_string()),
                            })
                            .await;
                    }
                }
            }
            Cmd::ShareSession => {
                let Some(client) = cloud.as_ref() else {
                    let _ = ui_tx
                        .send(UiMsg::Error("not connected to cloud".into()))
                        .await;
                    continue;
                };
                match current.as_ref() {
                    Some(SessionBackend::Cloud { session_id, .. }) => {
                        match client.share_session(session_id).await {
                            Ok(url) => {
                                let _ = ui_tx
                                    .send(UiMsg::ShareLink {
                                        session_id: session_id.clone(),
                                        url,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = ui_tx.send(UiMsg::Error(format!("share: {e:#}"))).await;
                            }
                        }
                    }
                    _ => {
                        let _ = ui_tx
                            .send(UiMsg::Error(
                                "share is only available for cloud sessions".into(),
                            ))
                            .await;
                    }
                }
            }
            Cmd::UnshareSession => {
                let Some(client) = cloud.as_ref() else {
                    let _ = ui_tx
                        .send(UiMsg::Error("not connected to cloud".into()))
                        .await;
                    continue;
                };
                match current.as_ref() {
                    Some(SessionBackend::Cloud { session_id, .. }) => {
                        match client.unshare_session(session_id).await {
                            Ok(()) => {
                                let _ = ui_tx
                                    .send(UiMsg::SharingStopped {
                                        session_id: session_id.clone(),
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = ui_tx.send(UiMsg::Error(format!("unshare: {e:#}"))).await;
                            }
                        }
                    }
                    _ => {
                        let _ = ui_tx
                            .send(UiMsg::Error(
                                "share is only available for cloud sessions".into(),
                            ))
                            .await;
                    }
                }
            }
            Cmd::OpenShareLink(url) => {
                let Some(client) = cloud.as_ref() else {
                    let _ = ui_tx
                        .send(UiMsg::Error("not connected to cloud".into()))
                        .await;
                    continue;
                };
                match client.resolve_share(&url).await {
                    Ok(share) => {
                        let _ = cmd_tx
                            .send(Cmd::OpenShared {
                                session_id: share.session_id,
                                token: share.token,
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiMsg::Error(e.to_string())).await;
                    }
                }
            }
            Cmd::OpenShared { session_id, token } => {
                let Some(client) = cloud.as_ref() else {
                    let _ = ui_tx
                        .send(UiMsg::Error("not connected to cloud".into()))
                        .await;
                    continue;
                };
                attach_cloud(
                    client,
                    &session_id,
                    Some(&token),
                    true,
                    false,
                    &mut current,
                    &mut pump,
                    &ui_tx,
                    &settings,
                    &inbox,
                )
                .await;
            }
            Cmd::Logout => {
                if let Some(h) = pump.take() {
                    h.abort();
                }
                if let Some(SessionBackend::Cloud { guest_pump, .. }) = current.as_mut() {
                    if let Some(h) = guest_pump.take() {
                        h.abort();
                    }
                }
                current = None;
                cloud = None;
                terminal_links.clear();
                settings.token = None;
                let _ = settings.save();
                let _ = ui_tx.send(UiMsg::LoggedOut).await;
            }
            Cmd::SaveCloudUrl(url) => {
                settings.cloud_url = url;
                let _ = settings.save();
            }
            Cmd::LoadHistory { before } => {
                if let Some(SessionBackend::Cloud { cmd, .. }) = current.as_ref() {
                    let _ = cmd.send(CloudCommand::GetSnapshot {
                        limit: Some(HISTORY_PAGE_U32),
                        before: Some(before),
                    });
                }
            }
            Cmd::RefreshSessions => {
                push_sessions(&manager, cloud.as_ref(), &mut terminal_links, &ui_tx).await;
                // The room the guest channel points at may have died and
                // re-registered with a new id — re-resolve the handle.
                refresh_guest_channel(cloud.as_ref(), &mut current, &ui_tx, &inbox).await;
            }
            Cmd::NewSession { kind, cwd, model } => {
                settings.last_backend = match kind {
                    BackendKind::Local => "local".into(),
                    BackendKind::Cloud | BackendKind::Terminal => "cloud".into(),
                };
                let _ = settings.save();
                match kind {
                    BackendKind::Terminal => {
                        let _ = ui_tx
                            .send(UiMsg::Error(
                                "terminal sessions are attached from the list, not created here"
                                    .into(),
                            ))
                            .await;
                    }
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
                                        &inbox,
                                        &settings,
                                    )
                                    .await;
                                    push_sessions(
                                        &manager,
                                        cloud.as_ref(),
                                        &mut terminal_links,
                                        &ui_tx,
                                    )
                                    .await;
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
                        match client.create_session(None, &cwd, model).await {
                            Ok(id) => match client.attach(&id).await {
                                Ok((ev_rx, cmd_tx)) => {
                                    if let Some(h) = pump.take() {
                                        h.abort();
                                    }
                                    current = Some(SessionBackend::Cloud {
                                        session_id: id.clone(),
                                        cmd: cmd_tx,
                                        guest_cmd: None,
                                        guest_pump: None,
                                        guest_link: None,
                                    });
                                    pump =
                                        Some(spawn_mpsc_pump(ev_rx, ui_tx.clone(), inbox.clone()));
                                    let _ = ui_tx
                                        .send(UiMsg::Attached {
                                            id: id.clone(),
                                            kind: BackendKind::Cloud,
                                            snapshot: None,
                                            read_only: false,
                                        })
                                        .await;
                                    send_pane_url(&settings, &id, &ui_tx).await;
                                    push_sessions(
                                        &manager,
                                        cloud.as_ref(),
                                        &mut terminal_links,
                                        &ui_tx,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    let _ =
                                        ui_tx.send(UiMsg::Error(format!("attach: {e:#}"))).await;
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
            Cmd::AutotestOpen { kind, index } => {
                // Same merge + sort as push_sessions.
                let mut list = listed_from_manager(&manager).await;
                if let Some(c) = cloud.as_ref() {
                    if let Ok(mut remote) = c.list_sessions().await {
                        list.append(&mut remote);
                    }
                }
                list.sort_by(|a, b| b.last_active.cmp(&a.last_active));
                let filtered: Vec<_> = list
                    .into_iter()
                    .filter(|m| match kind {
                        // Mirror meta_kind: pure terminal = join_handle with a
                        // non-discovered origin; merged discovered rows stay
                        // Local so the dual-channel attach gets exercised.
                        BackendKind::Terminal => {
                            m.join_handle.is_some()
                                && m.origin.as_deref() != Some("discovered")
                        }
                        BackendKind::Cloud => {
                            m.kind != "terminal" && (m.machine == "cloud" || m.machine.is_empty())
                        }
                        BackendKind::Local => {
                            !(m.machine == "cloud" || m.machine.is_empty())
                                && !(m.join_handle.is_some()
                                    && m.origin.as_deref() != Some("discovered"))
                        }
                    })
                    .collect();
                match filtered.get(index) {
                    Some(meta) => {
                        let id = meta.id.clone();
                        let jh = meta
                            .join_handle
                            .clone()
                            .or_else(|| terminal_links.get(&id).cloned());
                        let _ = cmd_tx
                            .send(Cmd::OpenSession {
                                id,
                                kind,
                                join_handle: jh,
                                read_only: false,
                            })
                            .await;
                    }
                    None => {
                        let _ = ui_tx
                            .send(UiMsg::Error(format!(
                                "autotest: no {kind:?} session at index {index}"
                            )))
                            .await;
                    }
                }
            }
            Cmd::OpenSession {
                id,
                kind,
                join_handle,
                read_only,
            } => match kind {
                BackendKind::Local => {
                    if let Some(sess) = manager.get(&id).await {
                        attach_local(sess, &mut current, &mut pump, &ui_tx, &inbox, &settings)
                            .await;
                    } else if let Some(client) = cloud.as_ref() {
                        // Machine-hosted row (spawned or discovered on a desktop
                        // daemon): history attaches through the cloud relay
                        // proxy; prompts ride the collab room when the row has
                        // a join_handle (dual-channel attach).
                        let has_room = join_handle
                            .as_deref()
                            .is_some_and(|s| !s.is_empty());
                        let attached = attach_cloud(
                            client,
                            &id,
                            None,
                            read_only,
                            has_room,
                            &mut current,
                            &mut pump,
                            &ui_tx,
                            &settings,
                            &inbox,
                        )
                        .await;
                        if attached {
                            if let Some(link) =
                                join_handle.as_deref().filter(|s| !s.is_empty())
                            {
                                match CollabAttach::connect(link).await {
                                    Ok((mut guest_ev, guest_tx)) => {
                                        if let Some(SessionBackend::Cloud {
                                            guest_cmd, guest_link, ..
                                        }) = current.as_mut()
                                        {
                                            *guest_cmd = Some(guest_tx);
                                            *guest_link = Some(link.to_string());
                                        }
                                        // The room is the live channel:
                                        // message_update deltas, early tool
                                        // starts, completions. The proxy
                                        // keeps history (snapshot pages);
                                        // its live events are filtered out
                                        // at attach so nothing double-renders.
                                        let handle = spawn_broadcast_pump(
                                            guest_ev,
                                            ui_tx.clone(),
                                            inbox.clone(),
                                        );
                                        if let Some(SessionBackend::Cloud { guest_pump, .. }) =
                                            current.as_mut()
                                        {
                                            *guest_pump = Some(handle);
                                        }
                                    }
                                    Err(e) => {
                                        let _ = ui_tx
                                            .send(UiMsg::Toast(format!(
                                                "prompt channel unavailable: {e:#}"
                                            )))
                                            .await;
                                        // Discovered row, no live room: the
                                        // proxy drops prompts — disable the
                                        // composer instead of lying.
                                        let _ = ui_tx.send(UiMsg::ReadOnly(true)).await;
                                    }
                                }
                            }
                        }
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
                                guest_cmd: None,
                                guest_pump: None,
                                guest_link: None,
                            });
                            pump = Some(spawn_mpsc_pump(ev_rx, ui_tx.clone(), inbox.clone()));
                            let _ = ui_tx
                                .send(UiMsg::Attached {
                                    id: id.clone(),
                                    kind: BackendKind::Cloud,
                                    snapshot: None,
                                    read_only: false,
                                })
                                .await;
                            send_pane_url(&settings, &id, &ui_tx).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiMsg::Error(format!("attach: {e:#}"))).await;
                        }
                    }
                }
                BackendKind::Terminal => {
                    let link = join_handle
                        .or_else(|| terminal_links.get(&id).cloned())
                        .filter(|s| !s.is_empty());
                    let Some(link) = link else {
                        let _ = ui_tx
                            .send(UiMsg::Error(format!(
                                "terminal session {id} has no join handle"
                            )))
                            .await;
                        continue;
                    };
                    // A TUI can outlive its collab room (host socket died,
                    // process lived). A dead room IS an ended session —
                    // surface it as such instead of a scary attach error.
                    match CollabAttach::connect(&link).await {
                        Err(e) if format!("{e:#}").contains("no such room") => {
                            let _ = ui_tx
                                .send(UiMsg::Toast("session ended (room closed)".into()))
                                .await;
                            continue;
                        }
                        Ok((ev_rx, cmd_tx)) => {
                            if let Some(h) = pump.take() {
                                h.abort();
                            }
                            current = Some(SessionBackend::Terminal {
                                session_id: id.clone(),
                                cmd: cmd_tx,
                            });
                            pump = Some(spawn_broadcast_pump(ev_rx, ui_tx.clone(), inbox.clone()));
                            let _ = ui_tx
                                .send(UiMsg::Attached {
                                    id: id.clone(),
                                    kind: BackendKind::Terminal,
                                    snapshot: None,
                                    read_only: false,
                                })
                                .await;
                            send_pane_url(&settings, &id, &ui_tx).await;
                        }
                        Err(e) => {
                            let _ = ui_tx
                                .send(UiMsg::Error(format!("collab attach: {e:#}")))
                                .await;
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
                    let _ = ui_tx.send(UiMsg::Error("no session attached".into())).await;
                }
            }
            Cmd::Abort => {
                if let Some(b) = current.as_ref() {
                    if let Err(e) = b.abort().await {
                        let _ = ui_tx.send(UiMsg::Error(format!("abort: {e:#}"))).await;
                    }
                }
            }
            Cmd::Queue(message) => {
                if let Some(b) = current.as_ref() {
                    if let Err(e) = b.steer(message).await {
                        let _ = ui_tx.send(UiMsg::Error(format!("queue: {e:#}"))).await;
                    }
                } else {
                    let _ = ui_tx.send(UiMsg::Error("no session attached".into())).await;
                }
            }
            Cmd::InboxOpen => {
                inbox_clear(&inbox, &ui_tx);
            }
            Cmd::PaneToggle(url) => {
                if let Some(b) = current.as_ref() {
                    settings.pane_urls.insert(b.id(), url);
                    let _ = settings.save();
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
                                    phases: st.todo_phases.clone(),
                                }))
                                .await;
                            let _ = ui_tx.send(UiMsg::SessionState(Box::new(st))).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiMsg::Error(format!("get_state: {e:#}"))).await;
                        }
                    }
                }
            }
        }
    }
}

async fn apply_token(
    token: String,
    settings: &mut Settings,
    cloud: &mut Option<CloudClient>,
    manager: &SessionManager,
    terminal_links: &mut HashMap<String, String>,
    ui_tx: &async_channel::Sender<UiMsg>,
) {
    settings.token = Some(token.clone());
    let _ = settings.save();
    match CloudClient::connect(&settings.cloud_url, &token).await {
        Ok(c) => {
            *cloud = Some(c);
            let _ = ui_tx
                .send(UiMsg::LoggedIn {
                    url: settings.cloud_url.clone(),
                })
                .await;
            push_sessions(manager, cloud.as_ref(), terminal_links, ui_tx).await;
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

async fn attach_local(
    sess: OmpSession,
    current: &mut Option<SessionBackend>,
    pump: &mut Option<AbortHandle>,
    ui_tx: &async_channel::Sender<UiMsg>,
    inbox: &Inbox,
    settings: &Settings,
) {
    if let Some(h) = pump.take() {
        h.abort();
    }
    let id = sess.id().to_string();
    let snapshot = sess.snapshot().await;
    let rx = sess.subscribe();
    *pump = Some(spawn_broadcast_pump(rx, ui_tx.clone(), inbox.clone()));
    *current = Some(SessionBackend::Local(sess));
    let _ = ui_tx
        .send(UiMsg::Attached {
            id: id.clone(),
            kind: BackendKind::Local,
            snapshot: Some(snapshot),
            read_only: false,
        })
        .await;
    send_pane_url(settings, &id, ui_tx).await;
}

async fn send_pane_url(settings: &Settings, id: &str, ui_tx: &async_channel::Sender<UiMsg>) {
    let _ = ui_tx
        .send(UiMsg::PaneUrl(settings.pane_urls.get(id).cloned()))
        .await;
}

fn spawn_broadcast_pump(
    mut rx: tokio::sync::broadcast::Receiver<SessionEvent>,
    ui_tx: async_channel::Sender<UiMsg>,
    inbox: Inbox,
) -> AbortHandle {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    pump_inbox(&ev, &inbox, &ui_tx);
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

/// Dual-channel variant: only history + lifecycle events pass; live content
/// (deltas, message completions, tool events) arrives via the collab room.
fn spawn_mpsc_pump_filtered(
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    ui_tx: async_channel::Sender<UiMsg>,
    inbox: Inbox,
) -> AbortHandle {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let keep = matches!(
                ev,
                SessionEvent::Snapshot(_)
                    | SessionEvent::StateChanged
                    | SessionEvent::ProcessExited { .. }
            );
            if !keep {
                continue;
            }
            pump_inbox(&ev, &inbox, &ui_tx);
            if ui_tx.send(UiMsg::Event(ev)).await.is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

fn spawn_mpsc_pump(
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    ui_tx: async_channel::Sender<UiMsg>,
    inbox: Inbox,
) -> AbortHandle {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            pump_inbox(&ev, &inbox, &ui_tx);
            if ui_tx.send(UiMsg::Event(ev)).await.is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

/// Feed the unseen-inbox from noteworthy session events.
fn pump_inbox(ev: &SessionEvent, inbox: &Inbox, ui_tx: &async_channel::Sender<UiMsg>) {
    let item = match ev {
        SessionEvent::AgentEnd => Some(InboxItem {
            text: "turn completed".into(),
            session_id: None,
        }),
        SessionEvent::UiRequest(req) => Some(InboxItem {
            text: format!(
                "question: {}",
                req.title
                    .as_deref()
                    .or(req.message.as_deref())
                    .unwrap_or("input requested")
            ),
            session_id: None,
        }),
        SessionEvent::Notice { level, message } if level == "error" => Some(InboxItem {
            text: format!("error: {message}"),
            session_id: None,
        }),
        _ => None,
    };
    if let Some(item) = item {
        inbox_push(inbox, ui_tx, item);
    }
}

async fn listed_from_manager(manager: &SessionManager) -> Vec<ListedSession> {
    let mut list = Vec::new();
    for m in manager.list().await {
        let (live, working) = match manager.get(&m.id).await {
            Some(sess) => (Some(true), Some(sess.is_streaming().await)),
            None => (Some(false), Some(false)),
        };
        list.push(ListedSession::from_meta(m, live, working));
    }
    list
}

async fn push_sessions(
    manager: &SessionManager,
    cloud: Option<&CloudClient>,
    terminal_links: &mut HashMap<String, String>,
    ui_tx: &async_channel::Sender<UiMsg>,
) {
    let mut list = listed_from_manager(manager).await;
    terminal_links.clear();
    if let Some(c) = cloud {
        match c.list_sessions().await {
            Ok(mut remote) => {
                for m in &remote {
                    if m.kind == "terminal" {
                        if let Some(h) = &m.join_handle {
                            terminal_links.insert(m.id.clone(), h.clone());
                        }
                    }
                }
                list.append(&mut remote);
            }
            Err(e) => {
                let _ = ui_tx
                    .send(UiMsg::Error(format!("list cloud sessions: {e:#}")))
                    .await;
            }
        }
    }
    for m in &mut list {
        if m.kind == "terminal" && m.live.is_none() {
            m.live = Some(true);
        }
    }
    list.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    let _ = ui_tx.send(UiMsg::SessionList(list)).await;
    // Device names for the rail (machine uuid → friendly name).
    if let Some(c) = cloud {
        if let Ok(machines) = c.list_machines().await {
            let names: std::collections::HashMap<String, String> =
                machines.into_iter().map(|m| (m.id, m.name)).collect();
            let _ = ui_tx.send(UiMsg::MachineNames(names)).await;
        }
    }
}

/// If the attached session's join_handle changed (the room died and
/// re-registered with a new id), reconnect the guest channel to the new
/// room and retire the dead one.
async fn refresh_guest_channel(
    cloud: Option<&CloudClient>,
    current: &mut Option<SessionBackend>,
    ui_tx: &async_channel::Sender<UiMsg>,
    inbox: &Inbox,
) {
    let Some(client) = cloud else { return };
    let (id, current_link) = match current.as_ref() {
        Some(SessionBackend::Cloud { session_id, guest_link, .. }) => {
            (session_id.clone(), guest_link.clone())
        }
        _ => return,
    };
    let Ok(sessions) = client.list_sessions().await else { return };
    let Some(meta) = sessions.iter().find(|m| m.id == id) else { return };
    let new_link = meta.join_handle.clone().filter(|s| !s.is_empty());
    if new_link == current_link {
        return;
    }
    if let Some(SessionBackend::Cloud { guest_pump, guest_cmd, guest_link, .. }) = current.as_mut()
    {
        if let Some(h) = guest_pump.take() {
            h.abort();
        }
        *guest_cmd = None;
        *guest_link = None;
    }
    let Some(link) = new_link else { return };
    match CollabAttach::connect(&link).await {
        Ok((guest_ev, guest_tx)) => {
            if let Some(SessionBackend::Cloud { guest_cmd, guest_link, guest_pump, .. }) =
                current.as_mut()
            {
                *guest_cmd = Some(guest_tx);
                *guest_link = Some(link);
                *guest_pump = Some(spawn_broadcast_pump(guest_ev, ui_tx.clone(), inbox.clone()));
            }
        }
        Err(e) => {
            let _ = ui_tx
                .send(UiMsg::Toast(format!("prompt channel unavailable: {e:#}")))
                .await;
        }
    }
}

async fn attach_cloud(
    client: &CloudClient,
    session_id: &str,
    share_token: Option<&str>,
    read_only: bool,
    suppress_live: bool,
    current: &mut Option<SessionBackend>,
    pump: &mut Option<AbortHandle>,
    ui_tx: &async_channel::Sender<UiMsg>,
    settings: &Settings,
    inbox: &Inbox,
) -> bool {
    // Owner attach pages the transcript (tail first, older on scroll-up);
    // shared/guest attach still gets the full snapshot — read-only streams
    // drop page commands, and the client buffers the overflow locally.
    // When a collab room is attached alongside, live events arrive there
    // instead — this stream stays history-only (snapshots + lifecycle).
    let result = if let Some(tok) = share_token {
        client.attach_shared(session_id, tok).await
    } else {
        client
            .attach_paged(session_id, HISTORY_PAGE_U32)
            .await
    };
    match result {
        Ok((ev_rx, cmd_tx)) => {
            if let Some(h) = pump.take() {
                h.abort();
            }
            if let Some(SessionBackend::Cloud { guest_pump, .. }) = current.as_mut() {
                if let Some(h) = guest_pump.take() {
                    h.abort();
                }
            }
            *current = Some(SessionBackend::Cloud {
                session_id: session_id.to_string(),
                cmd: cmd_tx,
                guest_cmd: None,
                guest_pump: None,
                guest_link: None,
            });
            *pump = Some(if suppress_live {
                spawn_mpsc_pump_filtered(ev_rx, ui_tx.clone(), inbox.clone())
            } else {
                spawn_mpsc_pump(ev_rx, ui_tx.clone(), inbox.clone())
            });
            let _ = ui_tx
                .send(UiMsg::Attached {
                    id: session_id.to_string(),
                    kind: BackendKind::Cloud,
                    snapshot: None,
                                                        read_only: read_only || share_token.is_some(),
                })
                .await;
            send_pane_url(settings, session_id, ui_tx).await;
        }
        Err(e) => {
            let _ = ui_tx.send(UiMsg::Error(format!("attach: {e:#}"))).await;
            return false;
        }
    }
    true
}
