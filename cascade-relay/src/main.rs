use cascade_relay::{serve, RelayConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cascade_relay=info".parse().unwrap()),
        )
        .init();

    let cfg = RelayConfig::from_env()?;
    serve(cfg).await
}
