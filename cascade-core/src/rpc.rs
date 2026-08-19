//! JSONL frame codec for omp RPC v2 (stdio).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
pub const CHUNK_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
    pub code: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct ReadyInfo {
    pub protocol_version: u32,
    pub supported_protocol_versions: Vec<u32>,
    pub max_frame_bytes: usize,
    pub max_reassembled_frame_bytes: usize,
    pub raw: Value,
}

struct PendingChunks {
    chunk_id: String,
    count: usize,
    next_index: usize,
    buf: Vec<u8>,
}

struct RpcInner {
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>,
    next_id: AtomicU64,
    next_chunk_id: AtomicU64,
    v2: AtomicBool,
    max_frame_bytes: AtomicUsize,
}

#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<RpcInner>,
}

impl RpcClient {
    pub fn new(stdin: ChildStdin) -> Self {
        Self {
            inner: Arc::new(RpcInner {
                stdin: Mutex::new(stdin),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                next_chunk_id: AtomicU64::new(1),
                v2: AtomicBool::new(false),
                max_frame_bytes: AtomicUsize::new(DEFAULT_MAX_FRAME_BYTES),
            }),
        }
    }

    pub fn enable_v2(&self, max_frame_bytes: usize) {
        self.inner.v2.store(true, Ordering::SeqCst);
        if max_frame_bytes > 0 {
            self.inner
                .max_frame_bytes
                .store(max_frame_bytes, Ordering::SeqCst);
        }
    }

    pub fn alloc_id(&self) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("req_{n}")
    }

    pub async fn write_value(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let v2 = self.inner.v2.load(Ordering::SeqCst);
        let max_frame = self.inner.max_frame_bytes.load(Ordering::SeqCst);
        if v2 && line.len() > max_frame {
            self.write_chunks(&line[..line.len() - 1]).await
        } else {
            let mut stdin = self.inner.stdin.lock().await;
            stdin.write_all(&line).await?;
            stdin.flush().await?;
            Ok(())
        }
    }

    async fn write_chunks(&self, logical: &[u8]) -> Result<()> {
        if logical.len() > MAX_REASSEMBLED_BYTES {
            anyhow::bail!("RPC frame exceeds 64MiB reassembly limit");
        }
        let n = self.inner.next_chunk_id.fetch_add(1, Ordering::Relaxed);
        let chunk_id = format!("rpc-{n}");
        let count = logical.len().div_ceil(CHUNK_PAYLOAD_BYTES).max(1);
        let mut stdin = self.inner.stdin.lock().await;
        for index in 0..count {
            let start = index * CHUNK_PAYLOAD_BYTES;
            let end = (start + CHUNK_PAYLOAD_BYTES).min(logical.len());
            let payload = &logical[start..end];
            let frame = json!({
                "type": "rpc_chunk",
                "chunkId": chunk_id,
                "index": index,
                "count": count,
                "byteLength": payload.len(),
                "data": B64.encode(payload),
            });
            let mut line = serde_json::to_vec(&frame)?;
            line.push(b'\n');
            stdin.write_all(&line).await?;
        }
        stdin.flush().await?;
        Ok(())
    }

    pub async fn send_raw(&self, value: Value) -> Result<()> {
        self.write_value(&value).await
    }

    pub async fn command(&self, mut body: Value) -> Result<RpcResponse> {
        let id = self.alloc_id();
        body["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.clone(), tx);
        self.write_value(&body).await?;
        match timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if !resp.success {
                    let err = resp
                        .error
                        .clone()
                        .unwrap_or_else(|| "rpc command failed".into());
                    return Err(anyhow!(err).context(format!(
                        "command {} id={id}",
                        resp.command
                    )));
                }
                Ok(resp)
            }
            Ok(Err(_)) => Err(anyhow!("rpc response channel closed for {id}")),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(anyhow!("rpc command {id} timed out after 30s"))
            }
        }
    }

    pub async fn dispatch_incoming(&self, frame: Value) -> Option<Value> {
        let ty = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "response" {
            let resp = parse_response(frame);
            if let Some(id) = resp.id.clone() {
                if let Some(tx) = self.inner.pending.lock().await.remove(&id) {
                    let _ = tx.send(resp);
                }
            }
            return None;
        }
        Some(frame)
    }
}

fn parse_response(raw: Value) -> RpcResponse {
    RpcResponse {
        id: raw
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        command: raw
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        success: raw
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        data: raw.get("data").cloned(),
        error: raw
            .get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        code: raw
            .get("code")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        raw,
    }
}

pub fn parse_ready(frame: &Value) -> Option<ReadyInfo> {
    if frame.get("type").and_then(|t| t.as_str()) != Some("ready") {
        return None;
    }
    let supported = frame
        .get("supportedProtocolVersions")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    Some(ReadyInfo {
        protocol_version: frame
            .get("protocolVersion")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32,
        supported_protocol_versions: supported,
        max_frame_bytes: frame
            .get("maxFrameBytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_FRAME_BYTES as u64) as usize,
        max_reassembled_frame_bytes: frame
            .get("maxReassembledFrameBytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_REASSEMBLED_BYTES as u64) as usize,
        raw: frame.clone(),
    })
}

/// Read JSONL from child stdout, reassemble `rpc_chunk` sequences, forward logical frames.
pub async fn read_frames(
    stdout: ChildStdout,
    v2_enabled: Arc<AtomicBool>,
    mut on_frame: impl FnMut(Value),
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut pending: Option<PendingChunks> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "rpc stdout read error");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let physical: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "malformed rpc jsonl line");
                continue;
            }
        };
        let ty = physical
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if ty == "rpc_chunk" {
            if !v2_enabled.load(Ordering::SeqCst) {
                tracing::warn!("rpc_chunk received before v2 negotiation");
                pending = None;
                continue;
            }
            match ingest_chunk(&mut pending, &physical) {
                Ok(Some(logical)) => match serde_json::from_slice::<Value>(&logical) {
                    Ok(v) => on_frame(v),
                    Err(e) => tracing::warn!(error = %e, "failed to parse reassembled rpc frame"),
                },
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "rpc_chunk reassembly failed");
                    pending = None;
                }
            }
            continue;
        }
        if pending.is_some() {
            tracing::warn!("non-chunk frame interrupted contiguous rpc_chunk sequence");
            pending = None;
        }
        on_frame(physical);
    }
}

fn ingest_chunk(pending: &mut Option<PendingChunks>, frame: &Value) -> Result<Option<Vec<u8>>> {
    let chunk_id = frame
        .get("chunkId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("rpc_chunk missing chunkId"))?
        .to_string();
    let index = frame
        .get("index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("rpc_chunk missing index"))? as usize;
    let count = frame
        .get("count")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("rpc_chunk missing count"))? as usize;
    let byte_length = frame
        .get("byteLength")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let data = frame
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("rpc_chunk missing data"))?;
    let decoded = B64
        .decode(data)
        .context("rpc_chunk base64 decode")?;
    if byte_length != 0 && decoded.len() != byte_length {
        anyhow::bail!(
            "rpc_chunk byteLength mismatch: declared {byte_length} got {}",
            decoded.len()
        );
    }
    if decoded.len() > CHUNK_PAYLOAD_BYTES {
        anyhow::bail!("rpc_chunk payload exceeds 256KiB");
    }
    if count == 0 {
        anyhow::bail!("rpc_chunk count is 0");
    }

    if pending.is_none() {
        if index != 0 {
            anyhow::bail!("rpc_chunk sequence did not start at index 0");
        }
        *pending = Some(PendingChunks {
            chunk_id: chunk_id.clone(),
            count,
            next_index: 0,
            buf: Vec::new(),
        });
    }

    let p = pending.as_mut().unwrap();
    if p.chunk_id != chunk_id {
        anyhow::bail!("interleaved rpc_chunk id {} vs {}", p.chunk_id, chunk_id);
    }
    if p.count != count {
        anyhow::bail!("rpc_chunk count changed mid-sequence");
    }
    if index != p.next_index {
        anyhow::bail!(
            "rpc_chunk not contiguous: expected index {} got {index}",
            p.next_index
        );
    }
    if p.buf.len().saturating_add(decoded.len()) > MAX_REASSEMBLED_BYTES {
        anyhow::bail!("reassembled rpc frame exceeds 64MiB");
    }
    p.buf.extend_from_slice(&decoded);
    p.next_index += 1;
    if p.next_index == p.count {
        let out = pending.take().unwrap().buf;
        Ok(Some(out))
    } else {
        Ok(None)
    }
}

pub async fn wait_ready(rx: oneshot::Receiver<ReadyInfo>) -> Result<ReadyInfo> {
    timeout(READY_TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("timed out waiting 30s for rpc ready frame"))?
        .map_err(|_| anyhow!("ready channel closed before ready frame"))
}
