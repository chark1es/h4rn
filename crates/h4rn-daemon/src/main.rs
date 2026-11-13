//! `h4rn-daemon` process entry point.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "h4rn_daemon=info".into()),
        )
        .init();
    h4rn_daemon::run()
}
