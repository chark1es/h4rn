//! `h4rn-connect` process entry point.

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "h4rn_connect=info".into()),
        )
        .init();

    let addr = std::env::var("H4RN_CONNECT_ADDR")
        .ok()
        .and_then(|value| value.parse::<SocketAddr>().ok())
        .unwrap_or_else(h4rn_connect::default_addr);

    h4rn_connect::serve(addr).await?;
    Ok(())
}
