//! `h4rnd` process entry point.

use clap::Parser;
use h4rn_daemon::{Cli, init_tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A Windows update renames the running binary to `.old` before moving the
    // new one in; if it was interrupted, finish that here.
    h4rn_daemon::update::cleanup_stale_files();
    let cli = Cli::parse();
    let (config, command) = cli.into_parts()?;
    init_tracing(&config.log_level);
    h4rn_daemon::run(config, command).await
}
