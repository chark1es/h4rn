//! `h4rn-connect` process entry point.

use h4rn_connect::{Config, build_state, serve};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "h4rn_connect=info,tower_http=info".into()),
        )
        .init();

    let config_path = std::env::args().nth(1).filter(|arg| arg != "--help");
    let config = Config::load(config_path.as_deref().map(std::path::Path::new))?;
    tracing::info!(
        mode = ?config.mode,
        base_url = %config.public_base_url,
        providers = config.providers.len(),
        "configuration loaded"
    );
    let bind = config.bind;
    let state = build_state(config).await?;
    serve(bind, state).await?;
    Ok(())
}
