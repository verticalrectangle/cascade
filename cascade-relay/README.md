# cascade-relay

omp-collab-compatible **WebSocket relay** plus a native Rust **guest host-bridge**.

This crate self-hosts the collab relay role that public deployments (historically `wss://wickrunner.com:8443`, currently `wss://my.omp.sh` in `@oh-my-pi/pi-wire`) provide. Point `omp` / cascade at this process instead of the hosted relay.

## Protocol summary

Sources (pi-coding-agent 17.3.x / `@oh-my-pi/pi-wire`):

- `src/collab/protocol.ts` — envelope, links, `CollabFrame`
- `src/collab/relay-client.ts` — client WebSocket (`?role=`, fatal codes, backoff)
- `src/collab/crypto.ts` — AES-256-GCM `[12B IV][ct+tag]`
- `src/collab/host.ts` / `guest.ts` — hello, welcome, routing usage
- `pi-wire` `RelayControlMessage`, `COLLAB_PROTO = 3`, `DEFAULT_RELAY_URL`

### Transport

- Endpoint: `GET /r/<roomId>?role=host|guest` upgrades to WebSocket.
- **TEXT** JSON: relay control only (`peer-joined`, `peer-left`, `room-closed`). No session bodies.
- **BINARY** envelopes: `[4B uint32 BE peerId][sealed payload]`. The relay **never decrypts**.

Envelope addressing (`protocol.ts` ~108–111):

| Sender | `peerId` on the envelope | Relay action |
| --- | --- | --- |
| Host | `0` | Broadcast sealed bytes to every guest |
| Host | `N ≠ 0` | Unicast to guest `N` |
| Guest | always `0` on send | Rewrite header to the sender’s assigned id, forward **only to the host** |

Guests never peer. Guest ids start at `1` and increase for the life of the in-memory room.

### Roles and close codes (`relay-client.ts` `FATAL_CLOSE_REASONS`)

Role comes from the **query string** (`CollabSocket` opens `` `${wsUrl}?role=${role}` ``). The encrypted `{ t: "hello", proto, name, writeToken }` frame is **not** visible to the relay.

| Condition | Code | Notes |
| --- | --- | --- |
| Host disconnect | `4001` | Guests also get TEXT `{t:"room-closed"}`; in-memory room is dropped |
| Guest hello / connect to unknown room | `4004` | Room exists only while a host is connected |
| Second host | `4009` | First `role=host` wins |
| Room full | `4029` | Guest cap (default 64) |

Fatal codes never reconnect on the client. Transient drops back off **1s…30s** with jitter (`BACKOFF_BASE_MS` / `BACKOFF_MAX_MS`).

### Persistence

If `CASCADE_RELAY_DATA_DIR` is set, each live host join writes `<dataDir>/<roomId>.json`:

```json
{ "roomId": "…", "relayUrl": "ws://127.0.0.1:8788", "link": null, "viewLink": null, "token": null }
```

This mirrors `~/.omp/collab/rooms/*.json` field names. The official host does **not** send link/token to the relay (those live in the share URL fragment). Optional TEXT `{ "t": "room-meta", "link", "viewLink", "token", "relayUrl" }` from the **host** fills those fields.

## Host bridge (library)

`CollabAttach::connect(link)` is a collab **guest**:

- Parses full `wss://…/r/<roomId>.<secret>`, compact `roomId.secret`, scheme-less `host/r/…`, http(s) `#fragment` deep links, legacy `#` / `%23` secrets.
- 32-byte view key or 48-byte `key∥writeToken` (base64url).
- Connects `?role=guest`, sends sealed `hello` (`proto: 3`), reconnects 1–30s, stops on fatal codes / decrypt failure.
- Maps `event` / `entry` / `state` / `welcome` / `ui-request*` onto `cascade_core::SessionEvent`; everything else is `Raw`.
- Outgoing: `GuestCommand::Prompt` / `Abort` (`{t:"prompt"}` / `{t:"abort"}`).

Shape matches `OmpSession::subscribe` + a command sender so GTK can treat collab as a third backend (no PTY).

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `CASCADE_RELAY_BIND` | `127.0.0.1:8788` | Listen address |
| `CASCADE_RELAY_PUBLIC_URL` | `ws://` or `wss://` + bind | Written into room JSON `relayUrl` |
| `CASCADE_RELAY_DATA_DIR` | unset | Optional JSON persistence |
| `CASCADE_RELAY_MAX_GUESTS` | `64` | Room full → 4029 |
| `CASCADE_RELAY_TLS_CERT` / `CASCADE_RELAY_TLS_KEY` | unset | PEM files; when both set, serve TLS (WSS) |
| `CASCADE_DEFAULT_RELAY_URL` | `wss://my.omp.sh` | Compact `roomId.secret` origin (omp-compatible) |
| `CASCADE_COLLAB_NAME` | OS username | Guest `hello.name` |

Local omp clients allow **plain `ws://` only for localhost**; non-local relays must be `wss://` (`normalizeRelayOrigin`).

Example:

```bash
CASCADE_RELAY_BIND=127.0.0.1:8788 cargo run -p cascade-relay
# omp /collab with relay URL ws://127.0.0.1:8788
```

Public WSS (this replacing `wickrunner.com:8443`): terminate TLS here or put Caddy/nginx in front.

```bash
CASCADE_RELAY_BIND=0.0.0.0:8443 \
CASCADE_RELAY_PUBLIC_URL=wss://wickrunner.com:8443 \
CASCADE_RELAY_TLS_CERT=/path/cert.pem \
CASCADE_RELAY_TLS_KEY=/path/key.pem \
CASCADE_RELAY_DATA_DIR=$HOME/.omp/collab/rooms \
  cascade-relay
```

Compact links still parse against `wss://my.omp.sh` unless `CASCADE_DEFAULT_RELAY_URL` is set — that matches stock omp. Rooms hosted on this binary need a full `host[:port]/r/<roomId>.<key>` (or `ws://127.0.0.1:8788/r/…`) link.

## Assumptions / ambiguities

1. **Role vs encrypted hello.** Assignment wording “first peer with host hello” maps to `?role=host` at upgrade time. The relay cannot read `{t:"hello"}` without the room key (`crypto.ts`: “the relay sees opaque bytes”). Cited: `relay-client.ts` `#openSocket`.
2. **Host drop vs host reconnect.** `CollabSocket` retries transient drops, but `4001` is fatal for guests. This relay closes the room immediately when the host socket ends (no grace window). A blip therefore ends the collab for guests; a reconnecting host creates a **new empty** in-memory room with the same id. Cited: `FATAL_CLOSE_REASONS[4001]`, assignment “host disconnect → close room”.
3. **Room full size** is not specified in the TS client. Default 64 guests, override `CASCADE_RELAY_MAX_GUESTS`. Host comments mention a relay `maxPayloadLength` (issue #3739); this relay does not enforce a byte cap.
4. **Guest reconnect** allocates a **new** `peerId`; the host sees `peer-left` then `peer-joined`.
5. **`DEFAULT_RELAY_URL`** in current pi-wire is `wss://my.omp.sh`, not `wickrunner.com:8443`. This binary is the self-hosted stand-in for that hosted relay role.
6. Optional `{t:"room-meta"}` is an extension so persistence can store `link`/`viewLink`/`token`; omp hosts do not send it.
