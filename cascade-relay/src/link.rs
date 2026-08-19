//! `parseCollabLink` equivalent (`src/collab/protocol.ts`).

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use url::Url;

use crate::protocol::{DEFAULT_RELAY_URL, ROOM_KEY_BYTES, WRITE_TOKEN_BYTES};

#[derive(Debug, Clone)]
pub struct ParsedCollabLink {
    /// `wss://host[:port]/r/<roomId>` — no query, no fragment.
    pub ws_url: String,
    pub room_id: String,
    pub key: Vec<u8>,
    pub write_token: Option<Vec<u8>>,
}

fn is_local_hostname(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

fn default_relay_url() -> String {
    std::env::var("CASCADE_DEFAULT_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string())
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    URL_SAFE_NO_PAD.decode(s).ok()
}

fn normalize_relay_origin(origin: &str) -> Result<String> {
    let url = Url::parse(origin).map_err(|_| anyhow!("Invalid relay URL: {origin}"))?;
    let scheme = match url.scheme() {
        "wss" | "https" => "wss",
        "ws" | "http" => "ws",
        other => bail!("Unsupported relay URL scheme: {other}:"),
    };
    let host = url.host_str().ok_or_else(|| anyhow!("Invalid relay URL: {origin}"))?;
    if scheme == "ws" && !is_local_hostname(host) {
        bail!("relay link must be wss:// (plain ws:// is only allowed for localhost)");
    }
    let port = match url.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let host_out = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{scheme}://{host_out}{port}"))
}

fn room_path_parts(pathname: &str) -> Option<(String, Option<String>)> {
    let rest = pathname.strip_prefix("/r/")?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('.') {
        Some((id, secret)) if (10..=64).contains(&id.len()) => {
            Some((id.to_string(), Some(secret.to_string())))
        }
        None if (10..=64).contains(&rest.len()) => Some((rest.to_string(), None)),
        _ => None,
    }
}

fn parse_once(link: &str) -> Result<ParsedCollabLink> {
    let mut text = link.trim().replace("%23", "#").replace("%23", "#");
    // Bare `<roomId>.<key>` or legacy `<roomId>#<key>` → default relay.
    let bare = {
        let sep = if let Some(i) = text.find('.') {
            Some((i, '.'))
        } else {
            text.find('#').map(|i| (i, '#'))
        };
        if let Some((i, _)) = sep {
            let (id, key) = text.split_at(i);
            let key = &key[1..];
            let id_ok = (10..=64).contains(&id.len())
                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            let key_ok = !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if id_ok && key_ok && !text.contains('/') && !text.contains("://") {
                Some(format!("{}/r/{id}.{key}", default_relay_url().trim_end_matches('/')))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(expanded) = bare {
        text = expanded;
    } else if !text.contains("://") {
        text = format!("wss://{text}");
    }

    let url = Url::parse(&text).map_err(|_| anyhow!("Invalid collab link: {link}"))?;

    if matches!(url.scheme(), "http" | "https") {
        if let Some(frag) = url.fragment() {
            if !frag.is_empty() {
                if let Ok(parsed) = parse_collab_link(frag) {
                    return Ok(parsed);
                }
            }
        }
    }

    let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    let origin = if let Some(p) = url.port() {
        format!("{origin}:{p}")
    } else {
        origin
    };
    let normalized = match normalize_relay_origin(&origin) {
        Ok(o) => o,
        Err(e) => {
            if !matches!(url.scheme(), "http" | "https") {
                if let Some(frag) = url.fragment() {
                    if !frag.is_empty() {
                        return parse_collab_link(frag);
                    }
                }
            }
            return Err(e);
        }
    };

    let (room_id, path_secret) = match room_path_parts(url.path()) {
        Some(p) => p,
        None => {
            if !matches!(url.scheme(), "http" | "https") {
                if let Some(frag) = url.fragment() {
                    if !frag.is_empty() {
                        return parse_collab_link(frag);
                    }
                }
            }
            bail!("Collab link must contain a /r/<roomId> path");
        }
    };

    let fragment = path_secret.or_else(|| url.fragment().map(|s| s.to_string()));
    let fragment = fragment.filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow!("Collab link is missing the <key> part")
    })?;

    let secret = b64url_decode(&fragment).ok_or_else(|| {
        anyhow!("Collab link key must be 32 (view) or 48 (full) base64url bytes")
    })?;
    if secret.len() != ROOM_KEY_BYTES && secret.len() != ROOM_KEY_BYTES + WRITE_TOKEN_BYTES {
        bail!("Collab link key must be 32 (view) or 48 (full) base64url bytes");
    }
    let key = secret[..ROOM_KEY_BYTES].to_vec();
    let write_token = if secret.len() > ROOM_KEY_BYTES {
        Some(secret[ROOM_KEY_BYTES..].to_vec())
    } else {
        None
    };
    Ok(ParsedCollabLink {
        ws_url: format!("{normalized}/r/{room_id}"),
        room_id,
        key,
        write_token,
    })
}

pub fn parse_collab_link(link: &str) -> Result<ParsedCollabLink> {
    parse_once(link)
}
