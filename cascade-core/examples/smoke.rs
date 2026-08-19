//! Smoke test: spawn a real `omp --mode rpc-ui` session through cascade-core,
//! send a prompt, stream events, verify plan + questionnaire paths exist.
//! Run: cargo run -p cascade-core --example smoke -- "say the word ok and nothing else"
use cascade_core::{OmpSession, SessionEvent, SpawnOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("cascade_core=info").init();
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "say ok".into());
    let dir = std::env::temp_dir().join("cascade-smoke");
    std::fs::create_dir_all(&dir)?;

    let session = OmpSession::spawn(SpawnOptions {
        cwd: dir,
        ..Default::default()
    })
    .await?;
    println!("spawned, cascade id = {}", session.id());

    let mut rx = session.subscribe();
    let s = session.clone();
    let p = prompt.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        s.prompt(p).await.expect("prompt");
    });

    let mut deltas = 0usize;
    loop {
        match rx.recv().await {
            Ok(ev) => {
                match &ev {
                    SessionEvent::TextDelta { .. } => deltas += 1,
                    SessionEvent::UiRequest(r) => {
                        println!("UI REQUEST {:?} {:?}", r.method, r.title);
                        session
                            .answer_ui(r.id.clone(), cascade_core::UiAnswer::Cancelled)
                            .await?;
                    }
                    other => println!("event: {:?}", std::mem::discriminant(other)),
                }
                if matches!(&ev, SessionEvent::TodoChanged { .. }) {
                    println!("TODOCHANGED fired");
                }
                if matches!(ev, SessionEvent::AgentEnd | SessionEvent::ProcessExited { .. }) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let state = session.get_state().await.ok();
    println!("text deltas: {deltas}");
    println!("state: {:?}", state.map(|s| (s.session_id, s.message_count)));
    session.shutdown().await?;
    Ok(())
}
