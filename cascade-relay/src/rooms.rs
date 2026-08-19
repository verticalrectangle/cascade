//! In-memory rooms plus optional JSON persistence (`~/.omp/collab/rooms/*.json` shape).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use crate::protocol::valid_room_id;

/// On-disk / listing record. Secrets in `link`/`token` are host-supplied metadata;
/// the relay never derives them from the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoomRecord {
    pub room_id: String,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub view_link: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

pub enum PeerKind {
    Host,
    Guest { peer_id: u32 },
}

pub struct PeerTx {
    pub kind: PeerKind,
    pub tx: mpsc::UnboundedSender<PeerMsg>,
}

pub enum PeerMsg {
    Binary(Vec<u8>),
    Text(String),
    Close { code: u16, reason: String },
}

pub struct Room {
    pub record: RoomRecord,
    pub host: Option<mpsc::UnboundedSender<PeerMsg>>,
    pub guests: HashMap<u32, mpsc::UnboundedSender<PeerMsg>>,
    pub next_peer_id: u32,
}

impl Room {
    fn new(record: RoomRecord) -> Self {
        Self {
            record,
            host: None,
            guests: HashMap::new(),
            next_peer_id: 1,
        }
    }

    fn guest_count(&self) -> usize {
        self.guests.len()
    }
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<HashMap<String, Room>>>,
    data_dir: Option<PathBuf>,
    max_guests: usize,
    public_relay_url: String,
}

impl Hub {
    pub fn new(data_dir: Option<PathBuf>, max_guests: usize, public_relay_url: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            data_dir,
            max_guests,
            public_relay_url,
        }
    }

    pub fn max_guests(&self) -> usize {
        self.max_guests
    }

    pub async fn persist_record(&self, rec: &RoomRecord) -> Result<()> {
        let Some(dir) = &self.data_dir else {
            return Ok(());
        };
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("create {}", dir.display()))?;
        let path = room_path(dir, &rec.room_id);
        let body = serde_json::to_vec_pretty(rec)?;
        tokio::fs::write(&path, body)
            .await
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub async fn join_host(
        &self,
        room_id: String,
        tx: mpsc::UnboundedSender<PeerMsg>,
    ) -> Result<JoinHost, JoinErr> {
        if !valid_room_id(&room_id) {
            return Err(JoinErr::BadRoomId);
        }
        let mut map = self.inner.lock().await;
        let room = map.entry(room_id.clone()).or_insert_with(|| {
            Room::new(RoomRecord {
                room_id: room_id.clone(),
                relay_url: Some(self.public_relay_url.clone()),
                ..Default::default()
            })
        });
        if room.host.is_some() {
            return Err(JoinErr::HostConflict);
        }
        room.host = Some(tx);
        info!(room_id = %room_id, "host joined");
        let rec = room.record.clone();
        drop(map);
        let _ = self.persist_record(&rec).await;
        Ok(JoinHost { room_id })
    }

    pub async fn join_guest(
        &self,
        room_id: &str,
        tx: mpsc::UnboundedSender<PeerMsg>,
    ) -> Result<JoinGuest, JoinErr> {
        if !valid_room_id(room_id) {
            return Err(JoinErr::BadRoomId);
        }
        let mut map = self.inner.lock().await;
        let Some(room) = map.get_mut(room_id) else {
            return Err(JoinErr::NoSuchRoom);
        };
        if room.host.is_none() {
            return Err(JoinErr::NoSuchRoom);
        }
        if room.guest_count() >= self.max_guests {
            return Err(JoinErr::RoomFull);
        }
        let peer_id = room.next_peer_id;
        room.next_peer_id = room.next_peer_id.saturating_add(1);
        room.guests.insert(peer_id, tx);
        if let Some(host) = &room.host {
            let msg = serde_json::json!({"t":"peer-joined","peer": peer_id}).to_string();
            let _ = host.send(PeerMsg::Text(msg));
        }
        info!(room_id = %room_id, peer_id, "guest joined");
        Ok(JoinGuest {
            room_id: room_id.to_string(),
            peer_id,
        })
    }

    /// Host → guests: `peerId == 0` broadcasts; otherwise target that guest.
    pub async fn route_from_host(&self, room_id: &str, envelope: Vec<u8>) {
        let peer_id = match crate::protocol::unpack_envelope(&envelope) {
            Some((id, _)) => id,
            None => return,
        };
        let map = self.inner.lock().await;
        let Some(room) = map.get(room_id) else {
            return;
        };
        if peer_id == 0 {
            for tx in room.guests.values() {
                let _ = tx.send(PeerMsg::Binary(envelope.clone()));
            }
        } else if let Some(tx) = room.guests.get(&peer_id) {
            let _ = tx.send(PeerMsg::Binary(envelope));
        }
    }

    /// Guest → host: rewrite envelope peerId to the sender, never fan-out to other guests.
    pub async fn route_from_guest(&self, room_id: &str, peer_id: u32, mut envelope: Vec<u8>) {
        if !crate::protocol::rewrite_envelope_peer(&mut envelope, peer_id) {
            return;
        }
        let map = self.inner.lock().await;
        if let Some(room) = map.get(room_id) {
            if let Some(host) = &room.host {
                let _ = host.send(PeerMsg::Binary(envelope));
            }
        }
    }

    pub async fn update_meta(&self, room_id: &str, patch: RoomRecord) {
        let mut map = self.inner.lock().await;
        if let Some(room) = map.get_mut(room_id) {
            if let Some(v) = patch.relay_url {
                room.record.relay_url = Some(v);
            }
            if let Some(v) = patch.link {
                room.record.link = Some(v);
            }
            if let Some(v) = patch.view_link {
                room.record.view_link = Some(v);
            }
            if let Some(v) = patch.token {
                room.record.token = Some(v);
            }
            let rec = room.record.clone();
            drop(map);
            let _ = self.persist_record(&rec).await;
        }
    }

    pub async fn guest_leave(&self, room_id: &str, peer_id: u32) {
        let mut map = self.inner.lock().await;
        let Some(room) = map.get_mut(room_id) else {
            return;
        };
        room.guests.remove(&peer_id);
        if let Some(host) = &room.host {
            let msg = serde_json::json!({"t":"peer-left","peer": peer_id}).to_string();
            let _ = host.send(PeerMsg::Text(msg));
        }
        info!(room_id = %room_id, peer_id, "guest left");
    }

    /// Host disconnect: notify guests (`room-closed` + close 4001) and drop the in-memory room.
    pub async fn host_leave(&self, room_id: &str) {
        let mut map = self.inner.lock().await;
        let Some(mut room) = map.remove(room_id) else {
            return;
        };
        room.host = None;
        let closed = serde_json::json!({"t":"room-closed"}).to_string();
        for tx in room.guests.values() {
            let _ = tx.send(PeerMsg::Text(closed.clone()));
            let _ = tx.send(PeerMsg::Close {
                code: crate::protocol::CLOSE_ROOM_CLOSED,
                reason: "room closed".into(),
            });
        }
        info!(room_id = %room_id, "host left; room closed");
    }
}

pub struct JoinHost {
    pub room_id: String,
}

pub struct JoinGuest {
    pub room_id: String,
    pub peer_id: u32,
}

#[derive(Debug)]
pub enum JoinErr {
    HostConflict,
    NoSuchRoom,
    RoomFull,
    BadRoomId,
}

fn room_path(dir: &Path, room_id: &str) -> PathBuf {
    dir.join(format!("{room_id}.json"))
}
