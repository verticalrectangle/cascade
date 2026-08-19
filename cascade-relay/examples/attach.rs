//! Attach to a live collab room (terminal omp session) as a native guest.
//! Run: cargo run -p cascade-relay --example attach -- "<join link>" "prompt text"
use cascade_relay::{CollabAttach, GuestCommand};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("cascade_relay=info").init();
    let link = std::env::args().nth(1).expect("join link arg");
    let prompt = std::env::args().nth(2);
    let (mut rx, cmd) = CollabAttach::connect(&link).await?;
    println!("connected to room");
    if let Some(p) = prompt {
        cmd.send(GuestCommand::Prompt { text: p }).await?;
    }
    let mut text = String::new();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if let cascade_core::SessionEvent::TextDelta { delta, .. } = &ev {
                    text.push_str(delta);
                }
                println!("ev: {:?}", std::mem::discriminant(&ev));
                if matches!(ev, cascade_core::SessionEvent::AgentEnd) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    println!("assistant text: {}", &text[..text.len().min(200)]);
    Ok(())
}
