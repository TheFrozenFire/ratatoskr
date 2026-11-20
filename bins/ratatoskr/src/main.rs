//! Own the selected databases and run pipelines in-process.

use clap::Parser;
use eyre::WrapErr;

mod cli;
mod export;
mod layout;
mod maintain;
mod oneshot;
mod peer_cmd;
mod sampler;
mod status;
mod supervise;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    venue::install_catalog_hooks();
    let cli = cli::Cli::parse();
    let loaded = config::load(&cli.config_dir)?;
    observability::init(&loaded.process.log);
    match cli.command {
        Some(cli::Command::Peer(cmd)) => peer_cmd::run(cmd, &loaded).await,
        Some(cli::Command::Maintain { data_dir }) => {
            let data_dir = match data_dir.or(cli.args.data_dir) {
                Some(dir) => dir,
                None => config::expand_home(&loaded.process.data_dir)?,
            };
            maintain::run(&data_dir, &loaded.store, &loaded.archival).await
        }
        None if cli.args.status => {
            let url = cli
                .args
                .status_url
                .clone()
                .unwrap_or_else(|| loaded.process.status_url.clone());
            let window = loaded.process.rate_window();
            let data_dir = match &cli.args.data_dir {
                Some(dir) => dir.clone(),
                None => config::expand_home(&loaded.process.data_dir)?,
            };
            let socket = config::peer_socket(&data_dir, &loaded.peer.socket)?;
            let caches = loaded.store.clone();
            let spots = loaded.spots.clone();
            tokio::task::spawn_blocking(move || {
                status::run(&url, window, &data_dir, &caches, &spots, &socket)
            })
            .await
            .wrap_err("status task")?
        }
        None => supervise::supervise(cli.args, loaded).await,
    }
}
