//! Own the selected databases and run pipelines in-process.

use clap::Parser;
use eyre::WrapErr;

mod cli;
mod export;
mod layout;
mod oneshot;
mod peer_cmd;
mod sampler;
mod status;
mod supervise;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    venues::install_catalog_hooks();
    let cli = cli::Cli::parse();
    let loaded = config::load(&cli.config_dir)?;
    observability::init(&loaded.process.log);
    match cli.command {
        Some(cli::Command::Peer(cmd)) => peer_cmd::run(cmd, &loaded).await,
        None if cli.args.status => {
            let url = cli
                .args
                .status_url
                .clone()
                .unwrap_or_else(|| loaded.process.status_url.clone());
            let window = loaded.process.rate_window();
            tokio::task::spawn_blocking(move || status::run(&url, window))
                .await
                .wrap_err("status task")?
        }
        None => supervise::supervise(cli.args, loaded).await,
    }
}
