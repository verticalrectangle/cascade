# cascade-ios — SwiftUI client for cascade

Native iOS app ported from the Enclave SwiftUI GUI. Talks to a `cascaded`
cloud daemon over its REST + WebSocket API — not the omp collab relay.

## Architecture

| Layer | File | Role |
|---|---|---|
| Transport | `Sources/CascadeBridge.swift` | `CascadeClient`: login/JWT, `GET /sessions`, WS `/sessions/{id}/stream`; projects cascade `SessionEvent` JSON onto `[UITurn]` |
| View model | `Sources/SessionVM.swift` | mirrors turns/plan/status into the editor |
| App state | `Sources/RootView.swift` | `AppModel`: account, session directory, background watchers per card |
| Views | EditorView / TranscriptViews / SessionsView / TrustView / Screens / ComposerParts | ported near-verbatim from Enclave |
| Live Activity | `Shared/CascadeActivity.swift` + `WidgetSources/CascadeWidgets.swift` | lock screen + Dynamic Island |

## Wire contract (cascade-core)

- Events are `{kind: <snake_case variant>, ...}`; first stream frame is
  `snapshot`. Commands: `{kind:"prompt",message}`, `{kind:"abort"}`,
  `{kind:"answer_ui",request_id,response:{value|confirmed|"cancelled"}}`.
- REST: `POST /auth/login {email,password} → {token}`, `GET/POST/DELETE /sessions`
  with `Authorization: Bearer`. Spawns run `omp --mode rpc-ui` on the daemon host,
  so that machine needs `omp` on PATH and reachable model config.

## Run

```bash
cascaded --role cloud --bind 127.0.0.1:7700 --db ./cascade.db \
  --jwt-secret <secret> --allow-passwords you@host:pass
# iOS simulator shares Mac loopback:
SIMCTL_CHILD_CASCADE_HOST=http://127.0.0.1:7700 \
SIMCTL_CHILD_CASCADE_EMAIL=you@host SIMCTL_CHILD_CASCADE_PASSWORD=pass \
xcrun simctl launch booted xyz.epsilver.cascade
```

Env seams (all `SIMCTL_CHILD_`-prefixed at launch): `CASCADE_HOST`,
`CASCADE_EMAIL`, `CASCADE_PASSWORD` (auto sign-in), `CASCADE_SESSION`
(auto-attach), `CASCADE_SCREENSHOT=1` (skip notification prompts),
`CASCADE_PROMPT` (auto-send), `CASCADE_TAB`, `CASCADE_SHOW_PAIR`.

## Cut from Enclave (not reachable over cascade cloud)

Link pairing/QR, collab sealed frames, subagent fan-out/drill-in, slash
palette, model-routing editor, rewind/edit-replace, image send, view links.
