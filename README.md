# cascade

A native GUI for [omp](https://github.com/oh-my-pi) (Oh My Pi) sessions — Linux-first GTK4 app, Rust core, cloud daemon. Open and drive omp sessions from a GUI or (soon) phone: real-time transcript streaming, plan panel, questionnaire/approval UI, and attach to terminal-started sessions remotely.

## Components

| Crate | Role |
|---|---|
| `cascade-core` | Library: drives `omp --mode rpc-ui` (protocol v2), session registry (SQLite), cloud/relay client. The multiplatform core. |
| `cascaded` | One daemon binary, two roles: **cloud** (auth, machine registry, cloud-hosted omp sessions, `/relay` router) and **desktop** (hosts local sessions, dials out to cloud — works behind NAT). |
| `cascade-gtk` | GTK4 app. Local + cloud + terminal-attach sessions in one UI. |
| `cascade-relay` | omp-collab-compatible WebSocket relay + native Rust collab **guest** (`CollabAttach`) so the app can attach to any collab room without a PTY. |
| `cascade-omp-plugin` | omp extension: auto-exposes every interactive terminal omp session via the relay and registers it with cascaded. |

## Docs

- `docs/architecture.md` — deployment shapes, relay design, milestones
- `docs/core-api.md` — cascade-core public API contract
- `docs/rpc-protocol.md` — omp rpc/rpc-ui protocol map (v1/v2, events, UI requests)
- `docs/theme.md` — visual design tokens (Rosé Pine Dawn variant, Archivo Black + Fira Sans)

## Run

```bash
# daemon (cloud role)
CASCADE_ROLE=cloud CASCADE_BIND=127.0.0.1:7700 CASCADE_DB=/var/lib/cascade/cascade.db \
  CASCADE_JWT_SECRET=... CASCADE_TERMINAL_TOKEN=... \
  CASCADE_ALLOW_PASSWORDS=you@example.com:password ./cascaded

# relay
CASCADE_RELAY_BIND=127.0.0.1:8788 ./cascade-relay

# app
cargo run -p cascade-gtk

# plugin (auto-register terminal sessions)
omp plugin link ./cascade-omp-plugin
export CASCADE_URL=https://host:7701 CASCADE_TOKEN=... CASCADE_RELAY=wss://host:8789
```

Headless UI testing: `CASCADE_AUTOTEST="wait:3;open-cloud:0;prompt:..."` drives the app deterministically (see main.rs).

Production deploy notes: static musl builds (`--target x86_64-unknown-linux-musl`, CC=musl-gcc) run anywhere; systemd units + Caddy TLS reverse proxy per docs/architecture.md.
