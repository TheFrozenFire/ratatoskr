//! Sync mainnet headers from `ratatoskr-peer` into `headers.redb`.
//!
//! Peers only serve headers by block range. [`pipeline::HeaderSource`] fetches
//! batches and submits them. The sink appends through [`chain::HeaderChain`],
//! which checks genesis and parents. This process does not check parents itself
//! and opens only the header file.

use clap::Parser;
use eyre::WrapErr;
use ipc::{socket_path, SocketPeer};
use pipeline::{HeaderSource, Source, StoreSink};
use std::path::PathBuf;
use store::LocalStore;
use tokio::sync::watch;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-headers")]
struct Args {
    /// redb path. Created if missing.
    #[arg(long)]
    db: Option<PathBuf>,
}

fn main() -> eyre::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .wrap_err("runtime")?
        .block_on(run())
}

async fn run() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "headers=info".into()),
        )
        .init();
    let args = Args::parse();
    let db_path = args.db.unwrap_or_else(chain::HeaderChain::default_path);
    let store = LocalStore::open_headers(&db_path)?;
    let socket = socket_path()?;
    tracing::info!(path = %db_path.display(), "header chain open");
    let source = HeaderSource::new(SocketPeer::new(socket), store.clone());
    let mut sink = StoreSink::new(store);
    // Held for the run. Dropping the sender makes `changed` ready immediately.
    let (_stop_tx, stop) = watch::channel(false);
    source.run(&mut sink, stop).await
}
