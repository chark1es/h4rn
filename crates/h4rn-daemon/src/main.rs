//! `h4rnd` process entry point.

use clap::Parser;
use h4rn_daemon::{Cli, init_tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Finish an interrupted update before doing anything else: a Windows
    // replace moves the running binary aside and a rollback moves the current
    // one aside, so the install path may be missing. This holds the same
    // per-installation lock as an update and works from a moved-aside image.
    h4rn_daemon::update::startup_recovery();
    let cli = Cli::parse();
    if cli.recover_only {
        // The scheduled task's launcher: take the same lock as an update and
        // publish a recovered binary atomically, then exit.
        h4rn_daemon::update::recover_only()?;
        return Ok(());
    }
    let (config, command) = cli.into_parts()?;
    init_tracing(&config.log_level);
    h4rn_daemon::run(config, command).await
}
