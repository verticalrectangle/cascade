use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

/// First bytes of a transcript used for title/session metadata.
const HEAD_WINDOW: u64 = 128 * 1024;
/// Hard cap on bytes read from any transcript file.
const FILE_READ_CAP: u64 = 64 * 1024 * 1024;
/// Lines longer than this are skipped.
const MAX_LINE: usize = 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub path: PathBuf,
    pub updated_at: DateTime<Utc>,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Changed(DiscoveredSession),
    Removed(PathBuf),
}

struct CacheEntry {
    size: u64,
    mtime: SystemTime,
    parsed: DiscoveredSession,
}

/// Process-independent discovery of omp JSONL sessions under store roots.
pub struct SessionWatcher {
    roots: Vec<PathBuf>,
    cache: Arc<Mutex<HashMap<PathBuf, CacheEntry>>>,
    parse_count: Arc<AtomicU64>,
}

impl SessionWatcher {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let roots = roots
            .into_iter()
            .map(|root| fs::canonicalize(&root).unwrap_or(root))
            .collect();
        Self {
            roots,
            cache: Arc::new(Mutex::new(HashMap::new())),
            parse_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn scan_once(&self) -> Vec<DiscoveredSession> {
        let roots = self.roots.clone();
        let cache = Arc::clone(&self.cache);
        let parse_count = Arc::clone(&self.parse_count);
        match tokio::task::spawn_blocking(move || scan_roots(&roots, &cache, &parse_count)).await {
            Ok(sessions) => sessions,
            Err(_) => Vec::new(),
        }
    }

    pub async fn watch(&self) -> impl Stream<Item = WatchEvent> {
        let (tx, rx) = mpsc::channel(256);
        let roots = self.roots.clone();
        let cache = Arc::clone(&self.cache);
        let parse_count = Arc::clone(&self.parse_count);
        tokio::spawn(async move {
            if let Err(err) = run_watch(roots, cache, parse_count, tx).await {
                tracing::debug!(error = %err, "session watcher stopped");
            }
        });
        ReceiverStream::new(rx)
    }

    #[cfg(test)]
    fn parse_count(&self) -> u64 {
        self.parse_count.load(Ordering::SeqCst)
    }
}

fn cache_lock(
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
) -> std::sync::MutexGuard<'_, HashMap<PathBuf, CacheEntry>> {
    cache.lock().unwrap_or_else(|err| err.into_inner())
}

fn scan_roots(
    roots: &[PathBuf],
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
    parse_count: &AtomicU64,
) -> Vec<DiscoveredSession> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(projects) = fs::read_dir(root) else {
            continue;
        };
        for project in projects.flatten() {
            let Ok(ft) = project.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() || !ft.is_file() {
                    continue;
                }
                let path = entry.path();
                if !is_jsonl(&path) {
                    continue;
                }
                if let Some(disc) = lookup_or_parse(cache, parse_count, &path) {
                    out.push(disc);
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn lookup_or_parse(
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
    parse_count: &AtomicU64,
    path: &Path,
) -> Option<DiscoveredSession> {
    let meta = fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let key = normalize_path(path);
    {
        let cache = cache_lock(cache);
        if let Some(entry) = cache.get(&key) {
            if entry.size == size && entry.mtime == mtime {
                return Some(entry.parsed.clone());
            }
        }
    }
    parse_and_store(cache, parse_count, &key)
}

fn parse_and_store(
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
    parse_count: &AtomicU64,
    path: &Path,
) -> Option<DiscoveredSession> {
    parse_count.fetch_add(1, Ordering::SeqCst);
    let parsed = parse_metadata(path)?;
    let meta = fs::metadata(path).ok()?;
    let entry = CacheEntry {
        size: meta.len(),
        mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        parsed: parsed.clone(),
    };
    cache_lock(cache).insert(normalize_path(path), entry);
    Some(parsed)
}

fn parse_metadata(path: &Path) -> Option<DiscoveredSession> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let size_bytes = meta.len();
    let updated_at = DateTime::<Utc>::from(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    let file = File::open(path).ok()?;
    let take = HEAD_WINDOW.min(FILE_READ_CAP);
    let mut reader = io::BufReader::new(file.take(take));
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).ok()?;

    let mut session_id = String::new();
    let mut cwd = String::new();
    let mut title = None;

    for raw in buf.split(|b| *b == b'\n') {
        let line = strip_cr(raw);
        if line.is_empty() || line.len() > MAX_LINE {
            continue;
        }
        let v: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(ty) = v.get("type").and_then(Value::as_str) else {
            continue;
        };
        match ty {
            "session" => {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    session_id = id.to_string();
                }
                if let Some(dir) = v.get("cwd").and_then(Value::as_str) {
                    cwd = dir.to_string();
                }
            }
            "title" => {
                if let Some(t) = v.get("title").and_then(Value::as_str) {
                    title = Some(t.to_string());
                }
            }
            _ => {}
        }
    }

    if session_id.is_empty() {
        return None;
    }
    Some(DiscoveredSession {
        session_id,
        title,
        cwd,
        path: normalize_path(path),
        updated_at,
        size_bytes,
    })
}

fn strip_cr(line: &[u8]) -> &[u8] {
    match line.strip_suffix(b"\r") {
        Some(stripped) => stripped,
        None => line,
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_session_jsonl(path: &Path, roots: &[PathBuf]) -> bool {
    if !is_jsonl(path) {
        return false;
    }
    for root in roots {
        if rel_depth(path, root) == Some(2) {
            return true;
        }
        if let Ok(canon) = fs::canonicalize(root) {
            if rel_depth(path, &canon) == Some(2) {
                return true;
            }
        }
    }
    false
}

fn rel_depth(path: &Path, root: &Path) -> Option<usize> {
    path.strip_prefix(root)
        .ok()
        .map(|rel| rel.components().count())
}

#[derive(Clone, Copy)]
enum Op {
    Change,
    Remove,
}

async fn run_watch(
    roots: Vec<PathBuf>,
    cache: Arc<Mutex<HashMap<PathBuf, CacheEntry>>>,
    parse_count: Arc<AtomicU64>,
    tx: mpsc::Sender<WatchEvent>,
) -> anyhow::Result<()> {
    let (n_tx, mut n_rx) = mpsc::unbounded_channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = n_tx.send(res);
    })?;

    let mut watched = HashSet::new();
    try_watch_roots(&mut watcher, &roots, &mut watched);

    let mut pending: HashMap<PathBuf, Op> = HashMap::new();
    let debounce = sleep(DEBOUNCE);
    tokio::pin!(debounce);
    // Park until the first event; the guard keeps this from firing empty.
    debounce
        .as_mut()
        .reset(Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));

    loop {
        tokio::select! {
            msg = n_rx.recv() => {
                let Some(msg) = msg else {
                    break;
                };
                if let Ok(event) = msg {
                    fold_event(&mut pending, event);
                    if !pending.is_empty() {
                        debounce.as_mut().reset(Instant::now() + DEBOUNCE);
                    }
                }
                let newly = try_watch_roots(&mut watcher, &roots, &mut watched);
                if !emit_new_roots(&newly, &cache, &parse_count, &tx).await {
                    break;
                }
            }
            _ = &mut debounce, if !pending.is_empty() => {
                let newly = try_watch_roots(&mut watcher, &roots, &mut watched);
                if !emit_new_roots(&newly, &cache, &parse_count, &tx).await {
                    break;
                }
                let batch = std::mem::take(&mut pending);
                if !flush(batch, &roots, &cache, &parse_count, &tx).await {
                    break;
                }
                debounce
                    .as_mut()
                    .reset(Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));
            }
        }
    }
    Ok(())
}

fn try_watch_roots(
    watcher: &mut RecommendedWatcher,
    roots: &[PathBuf],
    watched: &mut HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut newly = Vec::new();
    for root in roots {
        if watched.contains(root) {
            continue;
        }
        if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
            watched.insert(root.clone());
            newly.push(root.clone());
            continue;
        }
        if let Some(parent) = root.parent() {
            if parent.as_os_str().is_empty() {
                continue;
            }
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
        }
    }
    newly
}

async fn emit_new_roots(
    newly: &[PathBuf],
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
    parse_count: &AtomicU64,
    tx: &mpsc::Sender<WatchEvent>,
) -> bool {
    for root in newly {
        for disc in scan_roots(&[root.clone()], cache, parse_count) {
            if tx.send(WatchEvent::Changed(disc)).await.is_err() {
                return false;
            }
        }
    }
    true
}

fn fold_event(pending: &mut HashMap<PathBuf, Op>, event: notify::Event) {
    let kind = event.kind;
    let paths = event.paths;
    match &kind {
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => {}
        EventKind::Remove(_) => {
            for path in paths {
                pending.insert(path, Op::Remove);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for path in paths {
                pending.insert(path, Op::Remove);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let Some(from) = paths.first() {
                pending.insert(from.clone(), Op::Remove);
            }
            if let Some(to) = paths.get(1) {
                pending.insert(to.clone(), Op::Change);
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
            for path in paths {
                pending.insert(path, Op::Change);
            }
        }
    }
}

async fn flush(
    batch: HashMap<PathBuf, Op>,
    roots: &[PathBuf],
    cache: &Mutex<HashMap<PathBuf, CacheEntry>>,
    parse_count: &AtomicU64,
    tx: &mpsc::Sender<WatchEvent>,
) -> bool {
    for (path, op) in batch {
        if !is_session_jsonl(&path, roots) {
            if matches!(op, Op::Remove) {
                let stale = evict_under(cache, &path);
                for stale_path in stale {
                    if tx.send(WatchEvent::Removed(stale_path)).await.is_err() {
                        return false;
                    }
                }
            }
            continue;
        }
        match op {
            Op::Remove => {
                let key = normalize_path(&path);
                cache_lock(cache).remove(&key);
                cache_lock(cache).remove(&path);
                if tx.send(WatchEvent::Removed(path)).await.is_err() {
                    return false;
                }
            }
            Op::Change => {
                if is_symlink(&path) {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    if is_symlink(parent) {
                        continue;
                    }
                }
                if let Some(disc) = parse_and_store(cache, parse_count, &path) {
                    if tx.send(WatchEvent::Changed(disc)).await.is_err() {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn evict_under(cache: &Mutex<HashMap<PathBuf, CacheEntry>>, prefix: &Path) -> Vec<PathBuf> {
    let mut cache = cache_lock(cache);
    let keys: Vec<PathBuf> = cache
        .keys()
        .filter(|p| *p == prefix || p.starts_with(prefix))
        .cloned()
        .collect();
    for key in &keys {
        cache.remove(key);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    fn write_jsonl(path: &Path, lines: &[Value]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        fs::write(path, body).unwrap();
    }

    fn session_line(id: &str, cwd: &str) -> Value {
        json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd,
        })
    }

    fn title_line(title: &str) -> Value {
        json!({
            "type": "title",
            "v": 1,
            "title": title,
            "updatedAt": "2026-01-01T00:00:00.000Z",
        })
    }

    async fn wait_event<F>(
        evs: &mut (impl Stream<Item = WatchEvent> + Unpin),
        mut pred: F,
    ) -> WatchEvent
    where
        F: FnMut(&WatchEvent) -> bool,
    {
        timeout(Duration::from_secs(8), async {
            loop {
                match evs.next().await {
                    Some(ev) if pred(&ev) => return ev,
                    Some(_) => continue,
                    None => panic!("watch stream ended"),
                }
            }
        })
        .await
        .expect("timed out waiting for watch event")
    }

    #[tokio::test]
    async fn scan_two_level_walk_skips_artifacts_and_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let proj = root.join("-dev-cascade");
        fs::create_dir(&proj).unwrap();

        let jsonl = proj.join("sess.jsonl");
        write_jsonl(
            &jsonl,
            &[title_line("Keep"), session_line("sid-keep", "/work")],
        );

        let artifact = proj.join("sess");
        fs::create_dir(&artifact).unwrap();
        write_jsonl(
            &artifact.join("nested.jsonl"),
            &[session_line("sid-nested", "/nested")],
        );

        write_jsonl(
            &root.join("too-shallow.jsonl"),
            &[session_line("sid-shallow", "/shallow")],
        );

        let watcher = SessionWatcher::new(vec![root.to_path_buf()]);
        let found = watcher.scan_once().await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].session_id, "sid-keep");
        assert_eq!(found[0].title.as_deref(), Some("Keep"));
        assert_eq!(found[0].cwd, "/work");
        assert_eq!(found[0].path.file_name().unwrap(), "sess.jsonl");
        assert!(found[0].size_bytes > 0);
    }

    #[tokio::test]
    async fn scan_signature_cache_no_reparse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let jsonl = root.join("proj").join("a.jsonl");
        write_jsonl(&jsonl, &[session_line("sid-a", "/a"), title_line("A")]);

        let watcher = SessionWatcher::new(vec![root.to_path_buf()]);
        let first = watcher.scan_once().await;
        assert_eq!(first.len(), 1);
        assert_eq!(watcher.parse_count(), 1);

        let second = watcher.scan_once().await;
        assert_eq!(second, first);
        assert_eq!(watcher.parse_count(), 1);

        let mut file = fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
        writeln!(file, "{}", title_line("B")).unwrap();
        drop(file);

        let third = watcher.scan_once().await;
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].title.as_deref(), Some("B"));
        assert_eq!(watcher.parse_count(), 2);
    }

    #[tokio::test]
    async fn scan_truncated_line_tolerance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let jsonl = root.join("proj").join("trunc.jsonl");
        fs::create_dir_all(jsonl.parent().unwrap()).unwrap();
        let mut body = String::new();
        body.push_str(&session_line("sid-trunc", "/t").to_string());
        body.push('\n');
        body.push_str(&title_line("ok").to_string());
        body.push('\n');
        body.push_str(r#"{"type":"title","title":"partial"#);
        fs::write(&jsonl, body).unwrap();

        let watcher = SessionWatcher::new(vec![root.to_path_buf()]);
        let found = watcher.scan_once().await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "sid-trunc");
        assert_eq!(found[0].title.as_deref(), Some("ok"));
        assert_eq!(found[0].cwd, "/t");
    }

    #[tokio::test]
    async fn scan_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let proj = root.join("proj");
        fs::create_dir(&proj).unwrap();
        let real = proj.join("real.jsonl");
        write_jsonl(&real, &[session_line("sid-real", "/r")]);
        std::os::unix::fs::symlink(&real, proj.join("link.jsonl")).unwrap();

        let linked_proj = root.join("linked-proj");
        std::os::unix::fs::symlink(&proj, &linked_proj).unwrap();

        let watcher = SessionWatcher::new(vec![root.to_path_buf()]);
        let found = watcher.scan_once().await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].session_id, "sid-real");
        assert_eq!(found[0].path.file_name().unwrap(), "real.jsonl");
    }

    #[tokio::test]
    async fn scan_newest_title_in_head_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let jsonl = root.join("proj").join("t.jsonl");
        write_jsonl(
            &jsonl,
            &[
                title_line("old"),
                session_line("sid-t", "/cwd"),
                title_line("new"),
            ],
        );
        let watcher = SessionWatcher::new(vec![root.to_path_buf()]);
        let found = watcher.scan_once().await;
        assert_eq!(found[0].title.as_deref(), Some("new"));
        assert_eq!(found[0].cwd, "/cwd");
    }

    #[tokio::test]
    async fn scan_missing_root_is_tolerated() {
        let watcher = SessionWatcher::new(vec![PathBuf::from("/no/such/cascade-sessions")]);
        let found = watcher.scan_once().await;
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn watch_create_append_remove_events() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let proj = root.join("proj");
        fs::create_dir(&proj).unwrap();

        let watcher = SessionWatcher::new(vec![root.clone()]);
        let mut evs = watcher.watch().await;
        tokio::time::sleep(Duration::from_millis(150)).await;

        let jsonl = proj.join("live.jsonl");
        write_jsonl(
            &jsonl,
            &[title_line("one"), session_line("sid-live", "/live")],
        );

        let created = wait_event(&mut evs, |ev| match ev {
            WatchEvent::Changed(s) => {
                s.session_id == "sid-live" && s.title.as_deref() == Some("one")
            }
            WatchEvent::Removed(_) => false,
        })
        .await;
        match created {
            WatchEvent::Changed(s) => {
                assert_eq!(s.cwd, "/live");
                assert_eq!(s.path.file_name().unwrap(), "live.jsonl");
            }
            WatchEvent::Removed(_) => unreachable!(),
        }

        let mut file = fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
        writeln!(file, "{}", title_line("two")).unwrap();
        drop(file);

        wait_event(&mut evs, |ev| match ev {
            WatchEvent::Changed(s) => {
                s.session_id == "sid-live" && s.title.as_deref() == Some("two")
            }
            WatchEvent::Removed(_) => false,
        })
        .await;

        fs::remove_file(&jsonl).unwrap();
        wait_event(&mut evs, |ev| match ev {
            WatchEvent::Removed(p) => p.file_name().and_then(|n| n.to_str()) == Some("live.jsonl"),
            WatchEvent::Changed(_) => false,
        })
        .await;
    }
}
