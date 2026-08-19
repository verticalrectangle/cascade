# cascade-omp-plugin

OMP extension that turns a local TUI session into a Cascade-visible collab host.

When an interactive (`tui`) omp session starts, the plugin:

1. Creates a collab room on `CASCADE_RELAY` and speaks the **host** side of the omp collab wire protocol.
2. `POST`s the session to `${CASCADE_URL}/register-terminal` so Cascade can show a join/view link.

## Install

From a machine that already has omp:

```bash
omp plugin link /home/alexis/dev/cascade/cascade-omp-plugin
```

That records the package in the user plugin root (`~/.omp/plugins` / `omp-plugins.lock.json`). The loader reads `package.json#omp.extensions` (`index.ts`).

## Environment

| Variable | Default | Role |
| --- | --- | --- |
| `CASCADE_URL` | `https://wickrunner.com:7701` | cascaded HTTP API |
| `CASCADE_TOKEN` | *(required)* | Shared bearer sent as `X-Cascade-Token` |
| `CASCADE_RELAY` | `wss://wickrunner.com:8443` | Collab relay origin (path `/r/<roomId>?role=host`) |
| `CASCADE_DISABLE` | unset | Opt out (`1` / `true` / `yes` / `on`) |

If `CASCADE_TOKEN` is unset, the plugin logs a warning **once** and does nothing. RPC/print/json modes are skipped (`ctx.mode === "tui"`).

## Protocol assumptions (verified source)

Verified against `@oh-my-pi/pi-coding-agent` / `@oh-my-pi/pi-wire` **v17.3.8** installed at:

- `/home/alexis/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/collab/{protocol,relay-client,crypto,host}.ts`
- `/home/alexis/.bun/install/global/node_modules/@oh-my-pi/pi-wire/dist/types/index.d.ts`

Plugin loader: `package.json` field `omp` (or legacy `pi`) with `extensions: string[]` — `src/extensibility/plugins/loader.ts` (`pluginPkg.omp || pluginPkg.pi`) and `PluginManifest.extensions` in `src/extensibility/plugins/types.ts`. Factory shape: `export default function (pi: ExtensionAPI)`.

### Handshake and envelope

- Room material: 32-byte AES-256-GCM room key, 16-byte write token, 16-byte room id (`ROOM_KEY_BYTES` / `WRITE_TOKEN_BYTES` / `ROOM_ID_BYTES` in `pi-wire`).
- Connect URL: `wss://host[:port]/r/<roomId>?role=host` (`CollabSocket.#openSocket` in `relay-client.ts`).
- Binary envelope: `[4B uint32 BE peerId][sealed payload]`. Host→all guests uses `peerId = 0`; host→one guest uses the relay-assigned guest id (`protocol.ts` comments + `packEnvelope`).
- Seal layout: `[12B IV][AES-256-GCM ciphertext+tag]` over `JSON.stringify(frame)` (`crypto.ts`).
- Guest hello: `{ t: "hello", proto, name, writeToken? }`. Host rejects `proto !== COLLAB_PROTO` (`COLLAB_PROTO = 3`) with a targeted `{ t: "error" }` (`host.ts` `#handleHello`).
- Write capability: `writeToken` is base64url of the 16-byte token; timing-safe compare; missing/wrong token → read-only (`host.ts` `#verifyWriteToken`).
- Host hello reply: targeted `{ t: "welcome", proto, header, state, agents, entryCount, readOnly? }` followed by `{ t: "snapshot-chunk", entries, final }` (last chunk `final: true`). Empty transcript still sends one empty `final` chunk (`host.ts` `#sendSnapshotChunks`).
- Guest mutating frames: `{ t: "prompt", text, images? }`, `{ t: "abort" }`.
- Relay fatal close codes that must not reconnect: **4001, 4004, 4009, 4029** (`FATAL_CLOSE_REASONS` in `relay-client.ts`). Transient closes retry with 1–30s exponential backoff + jitter (`BACKOFF_BASE_MS` / `BACKOFF_MAX_MS`).
- Shareable links (non-default relay): `host[:port]/r/<roomId>.<base64url(key ∥ writeToken)>` for write, bare 32-byte key for view (`formatCollabLink` in `protocol.ts`).

### Plugin factory / events

- Guard: `ExtensionContext.mode === "tui"` (`extensions/types.ts`).
- Guest prompt → `pi.sendUserMessage(text)`; guest abort → `ctx.abort()`.
- Host event frames: `{ t: "event", event }` where `event.type` is one of the wire `AgentEvent` variants (`pi-wire` `HostFrame` / `host.ts` `WIRE_AGENT_EVENT_TYPES`).
- Registration: `POST ${CASCADE_URL}/register-terminal` with header `X-Cascade-Token` and JSON body `{ machine, session_id, join_handle, view_handle, cwd, title, pid }`; retry once. Shutdown: `DELETE` with `{ session_id }`.

## Remaining gaps (reduced host)

Full `CollabHost` in `host.ts` also:

- Replays the live transcript (`sessionManager.snapshotForReplication()`, image stripping, 512 KiB snapshot chunks).
- Broadcasts `{ t: "entry" }` on append, `{ t: "state" }` (debounced footer), `{ t: "agents" }`, `{ t: "bus" }` (task subagent EventBus).
- Handles `{ t: "agent-cmd" }`, `{ t: "fetch-transcript" }`, `{ t: "ui-request" }` / `{ t: "ui-response" }`.
- Injects guest prompts as `customType: "collab-prompt"` via `session.promptCustomMessage` instead of `sendUserMessage`.
- Aborts via `session.abort({ reason })` rather than `ctx.abort()`.

This plugin implements the **reduced host** scoped as: hello/welcome + empty snapshot terminator, live event fan-out, prompt injection, abort, reconnect, and Cascade registration. Guests that require a full replica session (history, Agent Hub, ask dialogs) will not match native `/share` host behavior.

All network/WebSocket work is isolated: handler failures are caught and logged with `pi.logger` and never abort the TUI session.
