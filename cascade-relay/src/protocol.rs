//! Close codes, envelope helpers, and TEXT JSON control types.
//!
//! Citations:
//! - Envelope addressing: `pi-coding-agent` `src/collab/protocol.ts` lines 108–111
//! - Control types: `@oh-my-pi/pi-wire` `RelayControlToHost` / `RelayControlToGuest`
//! - Fatal close codes: `src/collab/relay-client.ts` `FATAL_CLOSE_REASONS`

use serde::{Deserialize, Serialize};

/// AES-GCM room key length (view-link secret).
pub const ROOM_KEY_BYTES: usize = 32;
/// Write token appended to the key in full (read-write) links.
pub const WRITE_TOKEN_BYTES: usize = 16;
pub const ENVELOPE_HEADER_LENGTH: usize = 4;
/// Wire protocol version sent in encrypted `hello` (`pi-wire` `COLLAB_PROTO`).
pub const COLLAB_PROTO: u32 = 3;

/// Compact `<roomId>.<key>` links resolve against this origin (`pi-wire` `DEFAULT_RELAY_URL`).
pub const DEFAULT_RELAY_URL: &str = "wss://my.omp.sh";

/// Historical/public cascade relay this crate can self-host (see README).
pub const WICKRUNNER_RELAY_URL: &str = "wss://wickrunner.com:8443";

pub const CLOSE_ROOM_CLOSED: u16 = 4001;
pub const CLOSE_NO_SUCH_ROOM: u16 = 4004;
pub const CLOSE_HOST_CONFLICT: u16 = 4009;
pub const CLOSE_ROOM_FULL: u16 = 4029;

pub fn is_fatal_close(code: u16) -> bool {
    matches!(
        code,
        CLOSE_ROOM_CLOSED | CLOSE_NO_SUCH_ROOM | CLOSE_HOST_CONFLICT | CLOSE_ROOM_FULL
    )
}

pub fn fatal_reason(code: u16) -> Option<&'static str> {
    match code {
        CLOSE_ROOM_CLOSED => Some("room closed"),
        CLOSE_NO_SUCH_ROOM => Some("no such room"),
        CLOSE_HOST_CONFLICT => Some("a host is already connected for this room"),
        CLOSE_ROOM_FULL => Some("room is full"),
        _ => None,
    }
}

/// Relay → peer TEXT JSON. Extra variants are tolerated by clients (JSON parse only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum RelayControlMessage {
    #[serde(rename = "peer-joined")]
    PeerJoined { peer: u32 },
    #[serde(rename = "peer-left")]
    PeerLeft { peer: u32 },
    #[serde(rename = "room-closed")]
    RoomClosed,
}

pub fn pack_envelope(peer_id: u32, sealed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_HEADER_LENGTH + sealed.len());
    out.extend_from_slice(&peer_id.to_be_bytes());
    out.extend_from_slice(sealed);
    out
}

pub fn unpack_envelope(data: &[u8]) -> Option<(u32, &[u8])> {
    if data.len() < ENVELOPE_HEADER_LENGTH {
        return None;
    }
    let peer_id = u32::from_be_bytes(data[..ENVELOPE_HEADER_LENGTH].try_into().ok()?);
    Some((peer_id, &data[ENVELOPE_HEADER_LENGTH..]))
}

pub fn rewrite_envelope_peer(data: &mut [u8], peer_id: u32) -> bool {
    if data.len() < ENVELOPE_HEADER_LENGTH {
        return false;
    }
    data[..ENVELOPE_HEADER_LENGTH].copy_from_slice(&peer_id.to_be_bytes());
    true
}

pub fn valid_room_id(id: &str) -> bool {
    let n = id.len();
    (10..=64).contains(&n) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
