//! AES-256-GCM seal/open. Layout: `[12B IV][ciphertext+tag]` (`src/collab/crypto.ts`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Result};
use rand::RngCore;

use crate::protocol::ROOM_KEY_BYTES;

const IV_LENGTH: usize = 12;

pub fn seal(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != ROOM_KEY_BYTES {
        bail!("room key must be {ROOM_KEY_BYTES} bytes, got {}", key.len());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!(e))?;
    let mut iv = [0u8; IV_LENGTH];
    rand::thread_rng().fill_bytes(&mut iv);
    let nonce = Nonce::from_slice(&iv);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow!("seal failed: {e}"))?;
    let mut out = Vec::with_capacity(IV_LENGTH + ct.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if key.len() != ROOM_KEY_BYTES {
        bail!("room key must be {ROOM_KEY_BYTES} bytes, got {}", key.len());
    }
    if data.len() <= IV_LENGTH {
        bail!("sealed frame too short");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!(e))?;
    let nonce = Nonce::from_slice(&data[..IV_LENGTH]);
    cipher
        .decrypt(nonce, &data[IV_LENGTH..])
        .map_err(|_| anyhow!("bad key or corrupted frame"))
}
