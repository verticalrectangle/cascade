# Cascade Architecture

One Rust core, one daemon binary, three deployment shapes.

```
                 ┌──────────────────────────────┐
                 │ wickrunner.com (cloud role)  │
   sign in once  │  cascaded:                   │
 ┌───────────┐   │   • auth (accounts, JWT)     │
 │ GTK app   │──▶│   • machine registry         │
 │ (desktop) │WSS│   • cloud omp sessions       │
 └───────────┘   │   • /relay router            │
      │          └───────┬───────────┬──────────┘
      │ bundled          │relay WSS  │relay WSS
      │ auto-start       │(outbound) │
 ┌────▼───────────┐   ┌──▼────────┐  ┌▼──────────┐
 │ cascaded       │   │ desktop A │  │ laptop B  │
 │ (desktop role) │   │ cascaded  │  │ cascaded  │
 │ local omp      │   │ local omp │  │ local omp │
 └────────────────┘   └───────────┘  └───────────┘
```

## Roles

- **cascade-core** — library: omp rpc-ui session driver, session registry (SQLite), cloud/relay client. Multiplatform foundation (mobile via FFI later).
- **cascaded** — one binary, two roles (`--role cloud|desktop`, config file):
  - *cloud*: account auth (email+password → JWT now, magic-link later), machine registry table, cloud session hosting, `/relay` WSS router.
  - *desktop*: hosts local omp sessions, keeps an outbound WSS to cloud `/relay`, registers machine (name, account), executes relayed commands locally, streams events back. Auto-started by the GTK app (systemd user unit / launchd later).
- **cascade-gtk** — GTK4 client: talks to cloud API only; cloud-hosted and relayed sessions look identical. Ships cascaded bundled.

## Wire protocol (client ↔ cloud ↔ machine)

- REST: `POST /auth/login`, `GET /machines`, `GET /sessions`, `POST /sessions` (optional `machine`), `DELETE /sessions/:id`.
- WS: `GET /sessions/:id/stream` — server→client: `SessionEvent` JSON (the cascade-core enum, verbatim); client→server: `CloudCommand` JSON.
- WS: `/relay` — desktop role connects out; cloud multiplexes client session traffic over it; frames carry `{machine_id, session_id, payload}` envelopes.
- Auth: bearer JWT on every REST/WS call. Cloud checks account owns the target machine before relaying.

## Milestones

1. **Thin slice (now)**: cloud role on wickrunner, GTK app, cloud sessions, live transcript, plan view, questionnaire/approval UI. Desktop role code exists but relay routing can be stubbed.
2. **Machine relay**: `/relay` routing live, desktop auto-registration, phone-visible machines.
3. **Multiplatform**: core FFI (uniffi), mobile client.
