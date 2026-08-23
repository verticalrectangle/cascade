//! Replay omp JSONL transcripts into [`SessionSnapshot`] / [`SessionEvent`].

use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::session::{SessionEvent, SessionSnapshot, TodoPhase};

/// Skip a single JSONL line larger than this.
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Snapshot reads at most this many trailing bytes of a transcript.
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// Parse a transcript file into a late-joiner snapshot.
///
/// Missing or empty files yield an empty snapshot. Files larger than 64MiB
/// are read from the tail; a leading partial line is dropped. Lines over 1MiB
/// and a truncated final line are skipped.
pub fn parse_snapshot(path: &Path) -> Result<SessionSnapshot> {
    let bytes = read_file_window(path, MAX_SNAPSHOT_BYTES)?;
    Ok(snapshot_from_bytes(&bytes))
}

/// Map one JSONL transcript line to a live event.
///
/// `session` entries are skipped. `message` becomes [`SessionEvent::MessageEnd`],
/// `title` becomes [`SessionEvent::SessionInfo`] with an empty `session_id`
/// (caller patches). Everything else that parses is [`SessionEvent::Raw`].
pub fn parse_entry_event(line: &str) -> Option<SessionEvent> {
    let entry: Value = serde_json::from_str(line).ok()?;
    match entry.get("type").and_then(|t| t.as_str())? {
        "session" => None,
        "message" => {
            let message = entry.get("message").cloned().unwrap_or(Value::Null);
            Some(SessionEvent::MessageEnd { message })
        }
        "title" => {
            let title = entry
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Some(SessionEvent::SessionInfo {
                title,
                session_id: String::new(),
            })
        }
        _ => Some(SessionEvent::Raw(entry)),
    }
}

/// Incremental JSONL tailer. Tracks a byte offset and a partial last line.
pub struct FileTailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl FileTailer {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// Read bytes appended since the last call and map complete lines to events.
    ///
    /// If the file shrank (rotation/rewrite), the offset resets to the start.
    /// A trailing partial line is buffered until a newline arrives.
    pub async fn next_events(&mut self) -> Result<Vec<SessionEvent>> {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("open transcript tail {}", self.path.display())
                });
            }
        };
        let size = file.metadata().await?.len();
        if size < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if size == self.offset {
            return Ok(drain_complete_lines(&mut self.partial));
        }

        file.seek(SeekFrom::Start(self.offset)).await?;
        let mut chunk = Vec::new();
        file.read_to_end(&mut chunk).await?;
        self.offset += chunk.len() as u64;
        self.partial.extend_from_slice(&chunk);
        Ok(drain_complete_lines(&mut self.partial))
    }
}

fn read_file_window(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("open transcript {}", path.display()));
        }
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    let (start, drop_partial) = if len > max_bytes {
        (len - max_bytes, true)
    } else {
        (0, false)
    };
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if drop_partial {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => buf = buf[i + 1..].to_vec(),
            None => buf.clear(),
        }
    }
    Ok(buf)
}

fn snapshot_from_bytes(bytes: &[u8]) -> SessionSnapshot {
    let mut snap = SessionSnapshot::default();
    for raw in bytes.split(|&b| b == b'\n') {
        let line = trim_cr(raw);
        if line.is_empty() || line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if entry.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let message = entry.get("message").cloned().unwrap_or(Value::Null);
        if let Some(phases) = todo_phases_from_message(&message) {
            snap.todos = phases;
        }
        snap.messages.push(message);
    }
    snap.streaming = false;
    snap.pending_ui.clear();
    snap
}

fn todo_phases_from_message(message: &Value) -> Option<Vec<TodoPhase>> {
    let is_todo = message.get("role").and_then(|r| r.as_str()) == Some("toolResult")
        && message.get("toolName").and_then(|n| n.as_str()) == Some("todo");
    if !is_todo {
        return None;
    }
    let phases = message
        .get("details")
        .and_then(|d| d.get("phases"))
        .or_else(|| message.get("phases"))?;
    serde_json::from_value(phases.clone()).ok()
}

fn drain_complete_lines(buf: &mut Vec<u8>) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let mut consumed = 0usize;
    for (i, &b) in buf.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        let line = trim_cr(&buf[consumed..i]);
        consumed = i + 1;
        if line.is_empty() || line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        if let Some(ev) = parse_entry_event(text) {
            events.push(ev);
        }
    }
    if consumed > 0 {
        buf.drain(..consumed);
    }
    if buf.len() > MAX_LINE_BYTES {
        buf.clear();
    }
    events
}

fn trim_cr(bytes: &[u8]) -> &[u8] {
    match bytes.last() {
        Some(&b'\r') => &bytes[..bytes.len() - 1],
        _ => bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TodoStatus;
    use serde_json::json;
    use uuid::Uuid;

    const REAL_SAMPLE: &str = "/home/alexis/.omp/agent/sessions/-dev-cascade/2026-08-19T21-16-39-923Z_01a01be2-2273-7000-8a9d-78277d71b1ae.jsonl";

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cascade-replay-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(path, body).unwrap();
    }

    fn user_msg(text: &str) -> String {
        json!({
            "type": "message",
            "id": "m1",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        })
        .to_string()
    }

    fn assistant_msg(text: &str) -> String {
        json!({
            "type": "message",
            "id": "m2",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
        .to_string()
    }

    fn todo_result(phase: &str, task: &str, status: &str) -> String {
        json!({
            "type": "message",
            "id": "todo1",
            "message": {
                "role": "toolResult",
                "toolName": "todo",
                "toolCallId": "call_todo",
                "content": [{"type": "text", "text": "todos"}],
                "details": {
                    "op": "init",
                    "phases": [{
                        "name": phase,
                        "tasks": [{"content": task, "status": status}]
                    }]
                }
            }
        })
        .to_string()
    }

    #[test]
    fn parse_snapshot_missing_and_empty() {
        let path = temp_path("missing.jsonl");
        let snap = parse_snapshot(&path).unwrap();
        assert!(snap.messages.is_empty());
        assert!(snap.todos.is_empty());
        assert!(!snap.streaming);
        assert!(snap.pending_ui.is_empty());

        std::fs::write(&path, "").unwrap();
        let snap = parse_snapshot(&path).unwrap();
        assert!(snap.messages.is_empty());
    }

    #[test]
    fn parse_snapshot_messages_and_latest_todos() {
        let path = temp_path("synth.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"title","v":1,"title":"Hello"}"#,
                r#"{"type":"session","version":3,"id":"01abc","cwd":"/tmp"}"#,
                &user_msg("hi"),
                &todo_result("Research", "Map code", "in_progress"),
                &assistant_msg("ok"),
                &todo_result("Research", "Map code", "completed"),
            ],
        );
        let snap = parse_snapshot(&path).unwrap();
        assert_eq!(snap.messages.len(), 4);
        assert_eq!(snap.messages[0]["role"], "user");
        assert_eq!(
            snap.messages[0]["content"][0]["text"],
            json!("hi")
        );
        assert_eq!(snap.messages[2]["role"], "assistant");
        assert_eq!(snap.todos.len(), 1);
        assert_eq!(snap.todos[0].name, "Research");
        assert_eq!(snap.todos[0].tasks[0].content, "Map code");
        assert!(matches!(
            snap.todos[0].tasks[0].status,
            TodoStatus::Completed
        ));
        assert!(!snap.streaming);
        assert!(snap.pending_ui.is_empty());
    }

    #[test]
    fn parse_snapshot_real_sample() {
        let src = Path::new(REAL_SAMPLE);
        if !src.is_file() {
            return;
        }
        let path = temp_path("real.jsonl");
        std::fs::copy(src, &path).unwrap();
        let snap = parse_snapshot(&path).unwrap();
        assert_eq!(snap.messages.len(), 2);
        assert_eq!(snap.messages[0]["role"], "user");
        assert_eq!(
            snap.messages[0]["content"][0]["text"],
            json!("Reply with exactly: polish-check")
        );
        assert_eq!(snap.messages[1]["role"], "assistant");
        assert_eq!(snap.messages[1]["content"][1]["text"], json!("polish-check"));
        assert!(!snap.streaming);
        assert!(snap.pending_ui.is_empty());
    }

    #[test]
    fn parse_snapshot_tolerates_truncated_last_line() {
        let path = temp_path("trunc.jsonl");
        let mut body = user_msg("keep");
        body.push('\n');
        body.push_str(r#"{"type":"message","message":{"role":"assistant""#);
        std::fs::write(&path, body).unwrap();
        let snap = parse_snapshot(&path).unwrap();
        assert_eq!(snap.messages.len(), 1);
        assert_eq!(snap.messages[0]["role"], "user");
    }

    #[test]
    fn parse_snapshot_tail_drops_leading_partial_line() {
        let path = temp_path("window.jsonl");
        // 12 bytes: "hello\nworld\n". Last 8 → "lo\nworld\n", drop partial → "world\n".
        std::fs::write(&path, b"hello\nworld\n").unwrap();
        let bytes = read_file_window(&path, 8).unwrap();
        assert_eq!(bytes, b"world\n");
    }

    #[test]
    fn parse_entry_event_maps_known_types() {
        let title = parse_entry_event(r#"{"type":"title","title":"Fix it"}"#).unwrap();
        match title {
            SessionEvent::SessionInfo {
                title,
                session_id,
            } => {
                assert_eq!(title, "Fix it");
                assert_eq!(session_id, "");
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }

        assert!(parse_entry_event(r#"{"type":"session","id":"01abc"}"#).is_none());

        let msg = parse_entry_event(&user_msg("hello")).unwrap();
        match msg {
            SessionEvent::MessageEnd { message } => {
                assert_eq!(message["role"], "user");
            }
            other => panic!("expected MessageEnd, got {other:?}"),
        }

        for line in [
            r#"{"type":"model_change","model":"x"}"#,
            r#"{"type":"thinking_level_change","thinkingLevel":"low"}"#,
            r#"{"type":"mode_change","mode":"plan"}"#,
            r#"{"type":"compaction"}"#,
            r#"{"type":"branch_summary","summary":"s"}"#,
            r#"{"type":"custom_message","customType":"nudge"}"#,
        ] {
            match parse_entry_event(line) {
                Some(SessionEvent::Raw(v)) => {
                    assert!(v.get("type").is_some());
                }
                other => panic!("expected Raw for {line}, got {other:?}"),
            }
        }

        assert!(parse_entry_event("{not json").is_none());
        assert!(parse_entry_event("").is_none());
    }

    #[tokio::test]
    async fn tailer_emits_only_new_events() {
        let path = temp_path("tail.jsonl");
        write_lines(&path, &[&user_msg("one"), &assistant_msg("two")]);
        let mut tailer = FileTailer::new(path.clone());
        let first = tailer.next_events().await.unwrap();
        assert_eq!(first.len(), 2);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(file, "{}", user_msg("three")).unwrap();
        drop(file);

        let second = tailer.next_events().await.unwrap();
        assert_eq!(second.len(), 1);
        match &second[0] {
            SessionEvent::MessageEnd { message } => {
                assert_eq!(message["content"][0]["text"], json!("three"));
            }
            other => panic!("expected MessageEnd, got {other:?}"),
        }

        let third = tailer.next_events().await.unwrap();
        assert!(third.is_empty());
    }

    #[tokio::test]
    async fn tailer_buffers_truncated_line_until_complete() {
        let path = temp_path("partial.jsonl");
        let complete = user_msg("ok");
        let mut body = complete.clone();
        body.push('\n');
        body.push_str(r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"lat"#);
        std::fs::write(&path, &body).unwrap();

        let mut tailer = FileTailer::new(path.clone());
        let first = tailer.next_events().await.unwrap();
        assert_eq!(first.len(), 1);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        file.write_all(br#"er"}]}}"#).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let second = tailer.next_events().await.unwrap();
        assert_eq!(second.len(), 1);
        match &second[0] {
            SessionEvent::MessageEnd { message } => {
                assert_eq!(message["content"][0]["text"], json!("later"));
            }
            other => panic!("expected MessageEnd, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tailer_resets_on_rotation() {
        let path = temp_path("rotate.jsonl");
        write_lines(&path, &[&user_msg("old-a"), &user_msg("old-b")]);
        let mut tailer = FileTailer::new(path.clone());
        let first = tailer.next_events().await.unwrap();
        assert_eq!(first.len(), 2);

        write_lines(&path, &[&user_msg("new")]);
        let second = tailer.next_events().await.unwrap();
        assert_eq!(second.len(), 1);
        match &second[0] {
            SessionEvent::MessageEnd { message } => {
                assert_eq!(message["content"][0]["text"], json!("new"));
            }
            other => panic!("expected MessageEnd, got {other:?}"),
        }
    }
}
