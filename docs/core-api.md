# cascade-core Public API Contract

All three crates code against this. `cascade-core` owns the types; cascaded and cascade-gtk consume only this surface.

## Deployment shapes (one binary, one protocol)

cascaded runs in two roles:
- **cloud** (wickrunner.com): hosts cloud sessions, central auth, machine registry, `/relay` WSS router.
- **desktop** (user machine, bundled with the app, auto-started user service): hosts local omp sessions, dials OUT to cloud `/relay` as a persistent WSS client, registers as "machine X owned by account Y". Cloud forwards client traffic over that outbound connection. No inbound ports, works behind NAT/CGNAT. Direct-connect (own port/VPN) is advanced config only, never default.

Clients (GTK app, phone later) always talk to the cloud endpoint; relay routing is invisible to them. `attach` to a session on a relayed machine has identical semantics to a cloud-hosted session.

```rust
// cascade-core lib.rs re-exports: rpc, session, registry, state, remote modules.

/// Spawn configuration for one omp session.
pub struct SpawnOptions {
    pub cwd: PathBuf,                    // working dir for omp (required)
    pub omp_bin: PathBuf,                // default: "omp" from PATH
    pub model: Option<String>,           // passed as --model
    pub resume: Option<String>,          // session id or path -> --resume
    pub session_dir: Option<PathBuf>,    // --session-dir
    pub no_session: bool,                // --no-session ephemeral
    pub extra_env: Vec<(String, String)>,
}
impl Default for SpawnOptions { /* cwd: ".", omp_bin: "omp", rest none/false */ }

/// One live omp process driven over `--mode rpc-ui` (protocol v2 negotiated).
pub struct OmpSession { /* opaque */ }   // cheap-clone (Arc inside)
impl OmpSession {
    pub async fn spawn(opts: SpawnOptions) -> anyhow::Result<OmpSession>;
    pub fn id(&self) -> &str;                       // cascade-internal uuid
    pub async fn omp_session_id(&self) -> Option<String>; // from get_state
    /// Subscribe to the event stream. Broadcast: late subscribers get only new events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SessionEvent>;
    /// Full transcript + plan snapshot built from events so far.
    pub async fn snapshot(&self) -> SessionSnapshot;
    pub async fn prompt(&self, message: String) -> anyhow::Result<()>;
    pub async fn steer(&self, message: String) -> anyhow::Result<()>;
    pub async fn abort(&self) -> anyhow::Result<()>;
    pub async fn new_session(&self) -> anyhow::Result<()>;
    pub async fn answer_ui(&self, request_id: String, response: UiAnswer) -> anyhow::Result<()>;
    pub async fn set_model(&self, provider: String, model_id: String) -> anyhow::Result<()>;
    pub async fn set_thinking_level(&self, level: String) -> anyhow::Result<()>;
    pub async fn get_state(&self) -> anyhow::Result<RpcSessionState>;
    /// Kill the child and close the event stream.
    pub async fn shutdown(&self) -> anyhow::Result<()>;
}

/// Everything the UI needs, pushed over the broadcast channel AND mirrored
/// verbatim to remote clients over WSS by cascaded.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Ready { protocol_versions: Vec<u32> },
    TurnStarted,
    /// Streaming assistant text delta (contentIndex-scoped).
    TextDelta { content_index: u32, delta: String },
    ThinkingDelta { content_index: u32, delta: String },
    MessageStart { role: String },
    MessageEnd { message: serde_json::Value },
    ToolStart { tool_call_id: String, tool_name: String, args: serde_json::Value, intent: Option<String> },
    ToolUpdate { tool_call_id: String, partial: serde_json::Value },
    ToolEnd { tool_call_id: String, tool_name: String, is_error: bool, result: serde_json::Value },
    AgentEnd,                                // turn fully complete
    TodoChanged { phases: Vec<TodoPhase> },  // plan state
    /// extension_ui_request surfaced to the user: select/confirm/input/editor/open_url/notify
    UiRequest(UiRequest),
    UiRequestCancelled { target_id: String },
    Notice { level: String, message: String },
    SessionInfo { title: String, session_id: String },
    StateChanged,                            // model/thinking/etc changed; re-pull get_state
    ProcessExited { code: Option<i32> },
    /// Any rpc frame not mapped above, forwarded raw.
    Raw(serde_json::Value),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiRequest {
    pub id: String,
    pub method: UiMethod,
    pub title: Option<String>,
    pub message: Option<String>,
    pub options: Vec<String>,              // select
    pub placeholder: Option<String>,       // input
    pub prefill: Option<String>,           // editor
    pub url: Option<String>,               // open_url
    pub timeout_secs: Option<u64>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMethod { Select, Confirm, Input, Editor, OpenUrl, Notify, SetStatus, SetWidget, SetTitle, Other }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiAnswer {
    Value(String),                 // select (chosen option text), input, editor
    Confirmed(bool),               // confirm
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoPhase { pub name: String, pub tasks: Vec<TodoItem> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoItem { pub content: String, pub status: TodoStatus }
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus { Pending, InProgress, Completed, Abandoned, Blocked }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub messages: Vec<serde_json::Value>,  // AgentMessage verbatim
    pub todos: Vec<TodoPhase>,
    pub streaming: bool,
    pub pending_ui: Vec<UiRequest>,
}

/// serde-translated RpcSessionState (get_state data) — camelCase passthrough.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RpcSessionState {
    pub session_id: String,
    pub session_file: Option<String>,
    pub session_name: Option<String>,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub message_count: u32,
    pub todo_phases: Vec<TodoPhase>,
    pub model: Option<serde_json::Value>,
    pub thinking_level: Option<String>,
}

/// SQLite-backed registry of known sessions (local or cloud-spawned).
pub struct SessionRegistry { /* sqlite */ }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,               // cascade uuid
    pub omp_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub session_file: Option<String>,
    pub machine: String,          // machine id ("cloud" for wickrunner-hosted)
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_active: chrono::DateTime<chrono::Utc>,
}
impl SessionRegistry {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self>;
    pub fn upsert(&self, meta: &SessionMeta) -> anyhow::Result<()>;
    pub fn list(&self) -> anyhow::Result<Vec<SessionMeta>>;
    pub fn touch(&self, id: &str) -> anyhow::Result<()>;
    pub fn remove(&self, id: &str) -> anyhow::Result<()>;
}

/// Owns N OmpSessions. Used by cascaded (both roles) and cascade-gtk (local).
pub struct SessionManager { /* opaque */ }  // cheap-clone
impl SessionManager {
    pub fn new(registry: SessionRegistry) -> Self;
    pub async fn spawn(&self, opts: SpawnOptions) -> anyhow::Result<String>; // returns cascade id
    pub async fn get(&self, id: &str) -> Option<OmpSession>;
    pub async fn list(&self) -> Vec<SessionMeta>;
    pub async fn shutdown(&self, id: &str) -> anyhow::Result<()>;
    pub async fn shutdown_all(&self) -> ();
}

/// Client for the cloud API (used by cascade-gtk). Works identically for
/// cloud-hosted and relayed-machine sessions — routing is the server's job.
pub struct CloudClient { /* opaque */ }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudCommand {   // client -> daemon over session WS
    Prompt { message: String },
    Abort,
    AnswerUi { request_id: String, response: UiAnswer },
}
/// A registered machine as seen from the cloud.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineInfo { pub id: String, pub name: String, pub online: bool, pub is_cloud: bool }
impl CloudClient {
    pub async fn login(base_url: &str, email: &str, password: &str) -> anyhow::Result<String>; // returns bearer token
    pub async fn connect(base_url: &str, token: &str) -> anyhow::Result<Self>; // base_url https://host
    pub async fn list_machines(&self) -> anyhow::Result<Vec<MachineInfo>>;
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionMeta>>;
    /// machine None = cloud-hosted.
    pub async fn create_session(&self, machine: Option<&str>, cwd: &str, model: Option<String>) -> anyhow::Result<String>;
    pub async fn delete_session(&self, id: &str) -> anyhow::Result<()>;
    /// Attach to event stream; send commands via returned sender.
    pub async fn attach(&self, session_id: &str)
        -> anyhow::Result<(tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
                           tokio::sync::mpsc::UnboundedSender<CloudCommand>)>;
}
```

## Conventions
- Edition 2021, tokio runtime everywhere. No blocking calls in async fns (rusqlite via `tokio::task::spawn_blocking`).
- OmpSession/SessionManager are cheap-clone (`Arc` internals); broadcast channel capacity 1024.
- All rpc commands use 30 s timeout; `ready` wait 30 s; negotiate v2 on spawn; rpc_chunk reassembly required.
- Spawn uses `--mode rpc-ui` (ask tool + approvals required) and env `PI_RPC_EMIT_TITLE=1`.
- Errors: anyhow at API boundary.
- SessionEvent serialization is THE wire format: cascaded relays these frames verbatim to WS clients; changing it is a protocol break.
