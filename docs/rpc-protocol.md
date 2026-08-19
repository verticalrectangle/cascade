# omp `--mode rpc` Protocol Map

Source: `@oh-my-pi/pi-coding-agent` **v17.3.8** at `/home/alexis/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/modes/rpc/` (`rpc-types.ts`, `rpc-mode.ts`, `rpc-frame.ts`, `rpc-messages.ts`, `rpc-client.ts`, `host-tools.ts`, `host-uris.ts`, `rpc-subagents.ts`, `rpc-input.ts`), plus `src/main.ts`, `src/session/agent-session-events.ts`, `src/tools/ask.ts`. Runtime-verified 2026-08-19 against the installed `omp` binary (v17.3.8) with scripted stdin (`--mode rpc --no-session`; ready/negotiate/get_state/set_todos/get_messages/parse-error frames observed).

## 0. Transport

- JSON-lines (JSONL) over stdio: **one JSON object per line on stdin** (commands), **one JSON object per line on stdout** (responses + events). LF-delimited. No Content-Length framing (`src/jsonrpc/message-framing.ts` is LSP/DAP only).
- The **first** stdout frame is `ready`; then `extension_ui_request` (startup `setWidget`), then `available_commands_update`, then everything else. Wait for `ready` before sending commands (RpcClient uses a 30 s ready timeout).
- stdout is protocol-exclusive: `runRpcMode` forces `PI_NOTIFICATIONS=off`; nothing else may write there. Logs go to stderr.
- Malformed input lines do not kill the server: they produce `{"type":"response","command":"parse","success":false,"error":"Failed to parse command: <msg>"}` and the loop continues.
- EOF on stdin (client closed pipe) → server rejects pending side-channel requests, drains, disposes the session, `process.exit(0)`.
- Commands are processed **serially** in arrival order, except side-channel control frames (`extension_ui_response`, `host_tool_result`, `host_tool_update`, `host_uri_result`) which dispatch immediately, and `bash` which is background-dispatched so `abort_bash` can overtake. Response ordering across concurrent commands is not guaranteed — correlate with `id`.

## 1. Client → server frames

Every command: `{"id"?: string, "type": "<command>", ...}`. `id` is an optional opaque string echoed on the matching response (RpcClient sends `req_<n>`). Response envelope (always): `{"id"?: string, "type":"response", "command":"<command>", "success":boolean}` plus `"data"` on success or `"error": string` on failure; failures may also carry `"code"?: string` (machine-readable, e.g. `"session_busy"`, `"stale_cursor"`).

| type | fields | success data |
|---|---|---|
| `negotiate_protocol` | `protocolVersion`: number (**must be 2**) | `{protocolVersion: 2}` |
| `prompt` | `message`: string, `images?`: ImageContent[], `streamingBehavior?`: `"steer"\|"followUp"` | `{agentInvoked?: boolean}` (async — events follow) |
| `steer` | `message`, `images?` | — |
| `follow_up` | `message`, `images?` | — |
| `abort` | — | — |
| `abort_and_prompt` | `message`, `images?` | — |
| `new_session` | `parentSession?`: string | `{cancelled: boolean}` |
| `get_state` | — | `RpcSessionState` (below) |
| `set_fast_mode` | `enabled`: boolean | `{enabled, active}` |
| `get_available_commands` | — | `{commands: RpcAvailableSlashCommand[]}` |
| `set_todos` | `phases`: `TodoPhase[]` | `{todoPhases: TodoPhase[]}` |
| `set_host_tools` | `tools`: `RpcHostToolDefinition[]` | `{toolNames: string[]}` |
| `set_host_uri_schemes` | `schemes`: `RpcHostUriSchemeDefinition[]` | `{schemes: string[]}` |
| `set_subagent_subscription` | `level`: `"off"\|"progress"\|"events"` (default `"off"`) | `{level}` |
| `get_subagents` | — | `{subagents: RpcSubagentSnapshot[]}` |
| `get_subagent_messages` | `subagentId?` or `sessionFile?`, `fromByte?`: number | `RpcSubagentMessagesResult` |
| `set_model` | `provider`: string, `modelId`: string | `Model` |
| `cycle_model` | — | `{model, thinkingLevel, isScoped} \| null` |
| `get_available_models` | — | `{models: Model[]}` |
| `set_thinking_level` | `level`: `ThinkingLevel` | — |
| `cycle_thinking_level` | — | `{level} \| null` |
| `set_steering_mode` | `mode`: `"all"\|"one-at-a-time"` | — |
| `set_follow_up_mode` | `mode`: `"all"\|"one-at-a-time"` | — |
| `set_interrupt_mode` | `mode`: `"immediate"\|"wait"` | — |
| `compact` | `customInstructions?` | `CompactionResult` |
| `set_auto_compaction` | `enabled`: boolean | — |
| `set_auto_retry` | `enabled`: boolean | — |
| `abort_retry` | — | — |
| `bash` | `command`: string | `BashResult` (background-dispatched) |
| `abort_bash` | — | — |
| `get_session_stats` | — | `SessionStats` |
| `export_html` | `outputPath?` | `{path: string}` |
| `switch_session` | `sessionPath`: string (path to `session.jsonl`) | `{cancelled: boolean}` |
| `branch` | `entryId`: string | `{text: string, cancelled: boolean}` |
| `get_branch_messages` | — | `{messages: [{entryId, text}]}` |
| `get_last_assistant_text` | — | `{text: string \| null}` |
| `set_session_name` | `name`: string | — |
| `handoff` | `customInstructions?` | `{savedPath?: string} \| null` |
| `get_messages` | — | `{messages: AgentMessage[]}` |
| `get_messages_page` | `cursor?`: string, `limit?`: number (1–256, default 100) | `RpcMessagesPage` (below) |
| `get_login_providers` | — | `{providers: [{id, name, available, authenticated}]}` |
| `login` | `providerId`: string | `{providerId}` (emits `open_url`/`input` UI requests) |
| anything else | — | `success:false, error:"Unknown command: <type>"` (no `id`) |

### Side-channel client → server frames (dispatch immediately, overtake the queue)

```jsonc
{"type":"extension_ui_response","id":"<req-id>","value":"<string>"}              // select/input/editor
{"type":"extension_ui_response","id":"<req-id>","confirmed":true}                 // confirm
{"type":"extension_ui_response","id":"<req-id>","cancelled":true,"timedOut"?:bool}
{"type":"host_tool_result","id":"<req-id>","result":{"content":[{"type":"text","text":"…"}],"details":{}},"isError"?:bool}
{"type":"host_tool_update","id":"<req-id>","partialResult": AgentToolResult}
{"type":"host_uri_result","id":"<req-id>","content"?:string,"contentType"?:"text/markdown"|"application/json"|"text/plain","notes"?:string[],"immutable"?:bool,"isError"?:bool,"error"?:string}
```

### RpcSessionState (get_state data)

```jsonc
{
  "model"?: Model,                    // provider, id, api, baseUrl, contextWindow, maxTokens, thinking, compat…
  "thinkingLevel"?: ThinkingLevel,    // e.g. "low" | "high" | "auto" | …
  "isStreaming": boolean, "isCompacting": boolean,
  "steeringMode": "all" | "one-at-a-time",
  "followUpMode": "all" | "one-at-a-time",
  "interruptMode": "immediate" | "wait",
  "sessionFile"?: string,             // abs path to session.jsonl (absent with --no-session)
  "sessionId": string,                // e.g. "01a01b63-ea8d-7000-9c11-6e595dafa113"
  "sessionName"?: string,
  "autoCompactionEnabled": boolean, "fastModeEnabled": boolean, "fastModeActive": boolean,
  "tokensPerSecond": number | null, "messageCount": number, "queuedMessageCount": number,
  "todoPhases": TodoPhase[],
  "systemPrompt"?: string[],
  "dumpTools"?: [{name, description, parameters, examples?}],
  "contextUsage"?: {tokens:number, contextWindow:number, percent:number}
}
```

### RpcMessagesPage (get_messages_page data)

```jsonc
{ "messages": AgentMessage[], "nextCursor"?: string, "totalMessages": number }
```
Cursor is base64url JSON `{version:1, sessionId, leafId, messageCount, offset}`. Fails `code:"session_busy"` while streaming/compacting, `code:"stale_cursor"` if the session changed. Page budget ≤ 768 KiB.

## 2. Server → client events

### Ready frame (first stdout line — the only handshake)

```jsonc
{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}
```

### Session events — streamed verbatim as `AgentSessionEvent` (src/session/agent-session-events.ts + pi-agent-core `AgentEvent`)

- `{"type":"agent_start"}`
- `{"type":"agent_end","messages":AgentMessage[],"messageCount"?:number,"telemetry"?:…,"coverage"?:…,"isTerminal"?:boolean}` — **turn completion marker**; `messageCount` appears under v2 compaction when earlier messages were elided.
- `{"type":"turn_start"}` / `{"type":"turn_end","message":AgentMessage,"toolResults":ToolResultMessage[]}`
- `{"type":"message_start","message":AgentMessage}` (user + assistant + toolResult)
- `{"type":"message_update","message":AgentMessage,"assistantMessageEvent":AssistantMessageEvent}` — **streaming deltas**
- `{"type":"message_end","message":AgentMessage}`
- `{"type":"tool_execution_start","toolCallId":string,"toolName":string,"args":any,"intent"?:string}`
- `{"type":"tool_execution_update","toolCallId":string,"toolName":string,"args":any,"partialResult":any}`
- `{"type":"tool_execution_end","toolCallId":string,"toolName":string,"result":any,"isError"?:boolean}`
- `{"type":"auto_compaction_start","reason":"threshold"|"overflow"|"idle"|"incomplete","action":"context-full"|"handoff"|"shake"|"snapcompact"}`
- `{"type":"auto_compaction_end","action":…,"result":CompactionResult|undefined,"aborted":boolean,"willRetry":boolean,"errorMessage"?:string,"skipped"?:boolean}`
- `{"type":"auto_retry_start","attempt":number,"maxAttempts":number,"delayMs":number,"errorMessage":string,"errorId"?:number}`
- `{"type":"auto_retry_end","success":boolean,"attempt":number,"finalError"?:string,"retryErrors"?:…}`
- `{"type":"retry_fallback_applied","from":string,"to":string,"role":string}` / `{"type":"retry_fallback_succeeded","model":string,"role":string}`
- `{"type":"model_changed"}` / `{"type":"ttsr_triggered","rules":Rule[]}`
- `{"type":"todo_reminder","todos":TodoItem[],"attempt":number,"maxAttempts":number}` / `{"type":"todo_auto_clear"}`
- `{"type":"irc_message","message":CustomMessage}`
- `{"type":"notice","level":"info"|"warning"|"error","message":string,"source"?:string}`
- `{"type":"thinking_level_changed","thinkingLevel":ThinkingLevel|undefined,"configured"?:…,"resolved"?:Effort}`
- `{"type":"goal_updated","goal":Goal|null,"state"?:GoalModeState}`

### Streaming text/thinking/tool-call deltas

`message_update.assistantMessageEvent` (`AssistantMessageEvent` from pi-ai): `start`, `text_start{contentIndex,partial}`, **`text_delta{contentIndex,delta,partial}`**, `text_end{contentIndex,content,partial}`, **`thinking_delta{contentIndex,delta,partial}`**, `thinking_start`/`thinking_end`, `toolcall_start`/`toolcall_delta`/`toolcall_end{toolCall}`, `image_end{content}`, `done{reason:"stop"|"length"|"toolUse",message}`, `error{reason:"aborted"|"error",error}`. `partial` is the cumulative `AssistantMessage` (`{role:"assistant", content:(TextContent|ThinkingContent|ToolCall|…)[]}`).

### Question / approval / UI frames (ask tool, tool approvals, extensions, login)

Server emits `extension_ui_request`; host answers with `extension_ui_response`:

```jsonc
{"type":"extension_ui_request","id":"<snowflake>","method":"select","title":string,"options":string[],"timeout"?:number}
{"type":"extension_ui_request","id":"…","method":"confirm","title":string,"message":string,"timeout"?:number}
{"type":"extension_ui_request","id":"…","method":"input","title":string,"placeholder"?:string,"timeout"?:number}
{"type":"extension_ui_request","id":"…","method":"editor","title":string,"prefill"?:string,"promptStyle"?:boolean}
{"type":"extension_ui_request","id":"…","method":"cancel","targetId":string}
{"type":"extension_ui_request","id":"…","method":"notify","message":string,"notifyType"?:"info"|"warning"|"error"}   // fire-and-forget
{"type":"extension_ui_request","id":"…","method":"setStatus","statusKey":string,"statusText":string|undefined}  // fire-and-forget
{"type":"extension_ui_request","id":"…","method":"setWidget","widgetKey":string,"widgetLines"?:string[],"widgetPlacement"?:"aboveEditor"|"belowEditor"}
{"type":"extension_ui_request","id":"…","method":"setTitle","title":string}   // only with PI_RPC_EMIT_TITLE=1
{"type":"extension_ui_request","id":"…","method":"set_editor_text","text":string}  // fire-and-forget
{"type":"extension_ui_request","id":"…","method":"open_url","url":string,"launchUrl"?:string,"instructions"?:string}  // OAuth login
```

**Ask tool / questionnaire**: the `ask` tool is **only available in `--mode rpc-ui`** (`AskTool.createIf` requires `session.hasUI`, which is `isInteractive || mode === "rpc-ui"`; in plain `rpc` mode `hasUI=false` and tool-context `ui` is unset). In `rpc-ui` the ask tool's `select`/`editor` calls surface as `extension_ui_request` (select/input/editor) answered with `extension_ui_response`. Tool approval prompts likewise surface as `confirm` requests in `rpc-ui` (approve = `{"confirmed":true}`, deny = `{"confirmed":false}` or `{"cancelled":true}`).

### Host tool / host URI frames (bidirectional)

```jsonc
{"type":"host_tool_call","id":string,"toolCallId":string,"toolName":string,"arguments":Record<string,unknown>}   // → host_tool_result
{"type":"host_tool_cancel","id":string,"targetId":string}
{"type":"host_uri_request","id":string,"operation":"read"|"write","url":string,"content"?:string}   // → host_uri_result
{"type":"host_uri_cancel","id":string,"targetId":string}
```
Host tools are registered via `set_host_tools` (`{name,label?,description,parameters:JSON-Schema,hidden?,loadMode?}`), URI schemes via `set_host_uri_schemes` (`{scheme,description?,writable?,immutable?}`).

### Subagent frames (opt-in via `set_subagent_subscription`)

```jsonc
{"type":"subagent_lifecycle","payload":{"id":string,"agent":string,"agentSource":…,"description"?:string,"status":"started"|"completed"|"failed"|"aborted","sessionFile"?:string,"parentToolCallId"?:string,"index":number,"detached"?:boolean}}
{"type":"subagent_progress","payload":{"index":number,"agent":string,"agentSource":…,"task":string,"parentToolCallId"?:string,"assignment"?:string,"progress":AgentProgress,"sessionFile"?:string,"detached"?:boolean}}
{"type":"subagent_event","payload":{"id":string,"event":AgentSessionEvent}}   // level "events" only
```

### Other server frames

```jsonc
{"type":"prompt_result","id"?:string,"agentInvoked":false}   // prompt resolved locally (slash command)
{"type":"available_commands_update","commands":RpcAvailableSlashCommand[]}   // startup + on command-metadata change
{"type":"command_output","text":string}
{"type":"session_info_update","title":string,"sessionId":string}
{"type":"config_update","model":Model,"thinkingLevel":ThinkingLevel}
{"type":"extension_error","extensionPath":string,"event":string,"error":string}
{"type":"rpc_frame_error","originalType"?:string,"error":"RPC frame exceeded the transport limit"}
```

## 3. Protocol v1 vs v2 and negotiation

- Server always announces `ready` with `protocolVersion: 1`, `supportedProtocolVersions: [1, 2]`.
- **Negotiate**: send `{"type":"negotiate_protocol","protocolVersion":2}`. Server replies `{"type":"response","command":"negotiate_protocol","success":true,"data":{"protocolVersion":2}}` and switches its encoder to v2. Sending v1 after that fails with `"Unsupported RPC protocol version: 1"` (verified). No negotiation → connection stays on v1.
- **v2 adds**: (a) **chunking** for logical frames > `maxFrameBytes` (1 MiB incl. newline): physical `rpc_chunk` lines `{"type":"rpc_chunk","chunkId":"rpc-<n>","index":0..,"count":N,"byteLength":<utf8 bytes>,"data":"<base64 256KiB payload>"}` reassembled client-side (≤ `maxReassembledFrameBytes` = 64 MiB); contiguous sequences only; `rpc_chunk` before negotiation is a client-side protocol error. (b) **response compaction** for oversized `response` frames → `success:false,error:"RPC response exceeded the transport limit"`; `agent_end` gets already-streamed messages elided and gains `messageCount`. (c) **`get_messages_page`** (byte-bounded paging; v1 clients use `get_messages`).
- v1 oversized-frame fallback: shrink passes (stringCap 256 KiB → 64 B, array/object limits 512 → 1/8), then `rpc_frame_error`/overflow error frames.

## 4. Plan mode representation

No dedicated "plan" event type; plan/todo state is the todo list:
- `get_state` returns `todoPhases: TodoPhase[]`; `TodoPhase = {name: string, tasks: TodoItem[]}`, `TodoItem = {content: string, status: "pending"|"in_progress"|"completed"|"abandoned"|"blocked"}`.
- Host sets/replaces the plan with `set_todos {phases}` (response echoes `{todoPhases}`).
- The agent's `todo` tool calls stream as `tool_execution_start/update/end`; result `details` carry `{op:"init"|"start"|"done"|"rm"|"drop"|"block"|"unblock"|"append"|"view", phases, storage:"session"|"memory", completedTasks?}`.
- Session events `todo_reminder {todos, attempt, maxAttempts}` and `todo_auto_clear` fire from the todo tracker.
- Plan prose lives in the assistant `message`/`agent_end.messages`. Plan-mode add-ons (`plan-mode/`, `/plan`, `--plan`) operate on this same todo state.

## 5. Resuming a session by id

**CLI flags** (with `--mode rpc`):
- `--resume <id-or-path>` — value containing `/`, `\`, or ending `.jsonl` → treated as session-file path; otherwise matched against recent session ids (`SESSION_ID_ARG_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i`). Bare `--resume` opens an interactive picker (not headless).
- `--continue` / `-c` — most recent session in project (or `--continue <id>` normalized to `--resume <id>`).
- `--fork <id>` — fork from a past session. `--session-dir <dir>` — session storage root (default `PI_CODING_AGENT_SESSION_DIR` or `~/.omp/agent/sessions/<munged-cwd>/<timestamp>_<id>/session.jsonl`). `--no-session` — in-memory.

**Over RPC**: `switch_session {sessionPath}` (swap to a session file; returns `{cancelled}`); `new_session {parentSession?}`; `get_state` → `sessionId` + `sessionFile` (abs path — use as `switch_session` argument to round-trip). **No list-sessions RPC command** — enumerate `session.jsonl` under `~/.omp/agent/sessions/` (or `SessionManager.list` in the TS SDK). `get_messages`/`get_messages_page` read the resumed transcript; `branch {entryId}` forks from a user message.

## 6. Frame size limits, keepalive

- `MAX_RPC_FRAME_BYTES = 1024*1024` (1 MiB, includes trailing newline) per physical JSONL line.
- `MAX_RPC_REASSEMBLED_BYTES = 64*1024*1024` (64 MiB) per logical v2 frame. v2 chunk payload 256 KiB base64.
- **No ping/heartbeat/keepalive frames exist.** Liveness is client-side: 30 s ready timeout, 30 s per-command timeout (600 s for `login`). Cancel in-flight work with `abort`/`abort_bash`/`host_tool_cancel`/`host_uri_cancel`.

## 7. Authentication / environment

- **No protocol-level auth**: stdio ownership is the trust boundary. Provider keys from the standard omp credential store (AuthStorage/keychain/config) selected by `--provider`/`--model` at launch; OAuth in-band via `get_login_providers` + `login` (server emits `open_url`, then possibly `input` for pasted codes; 600 s timeout).
- Env: `PI_NOTIFICATIONS=off` (forced by RPC), `PI_NO_TITLE=1` (set for rpc), `PI_RPC_EMIT_TITLE=1` (opt-in `setTitle` events), `PI_CODING_AGENT_SESSION_DIR` (default session dir), `PI_NO_PTY=1` (rpc-ui); launcher maps `OMP_*` → `PI_*`.
- RPC/ACP hosts get neutral default settings (task isolation, memory/advisor off) unless explicitly configured (main.ts `applyRpcDefaultSettingOverrides`).

## 8. `--mode rpc` vs `--mode rpc-ui`

Identical frame protocol; differences: `rpc-ui` sets `session.hasUI=true` and wires the tool UI context → **`ask` tool available** and tool approvals can prompt; `PI_NO_PTY=1`. Plain `rpc` is headless (`ask` unavailable — "Ask tool requires interactive mode"); extensions still get the RPC UI context, so extension `select`/`confirm`/`input`/`editor` and OAuth `open_url`/`input` still work and are answered via `extension_ui_response`.

## 9. Rust driver skeleton

1. Spawn `omp --mode rpc-ui [--provider P] [--model M] [--session-dir D] [--resume ID] [--no-session]` with stdin/stdout pipes.
2. Read JSONL from stdout until `type == "ready"`; record `supportedProtocolVersions`/`maxFrameBytes`.
3. Send `{"id":"n1","type":"negotiate_protocol","protocolVersion":2}`; await matching response; enable v2 chunk reassembly.
4. Send `{"id":"p1","type":"prompt","message":"…"}`; consume `agent_start`…`message_update` (text/thinking deltas via `assistantMessageEvent`)…`agent_end`; the `prompt` response is emitted immediately with `success:true` (+`data.agentInvoked`).
5. Answer `extension_ui_request` with `extension_ui_response`; execute `host_tool_call`/`host_uri_request` and reply with results.
6. On EOF/exit: reap; pending side-channel work is rejected server-side automatically.
