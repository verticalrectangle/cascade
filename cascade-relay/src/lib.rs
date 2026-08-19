//! omp-collab-compatible WebSocket relay and native guest host-bridge.
//!
//! See `README.md` for protocol summary, close codes, and ambiguities.

pub mod attach;
pub mod crypto;
pub mod link;
pub mod protocol;
pub mod rooms;
pub mod server;
pub mod socket;

pub use attach::{CollabAttach, GuestCommand};
pub use link::{parse_collab_link, ParsedCollabLink};
pub use protocol::{COLLAB_PROTO, DEFAULT_RELAY_URL, ENVELOPE_HEADER_LENGTH};
pub use server::{serve, RelayConfig};
