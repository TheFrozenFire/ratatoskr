//! Pull receipt ranges from `ratatoskr-peer` into a redb corpus.
//!
//! The ranges cover the Uniswap v3 factory deployment, the Uniswap v4
//! PoolManager deployment, and a recent window. The process stops once the
//! database holds the requested number of bytes. A restart continues from the
//! saved cursor.

use clap::Parser;
use corpus::Corpus;
use ipc::{socket_path, SocketPeer};
use pipeline::{ReceiptSource, Source, StoreSink};
use std::path::PathBuf;
use store::{EntityStore, LocalStore};
use tracing::info;

const TARGET_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-ingest")]
struct Args {
    /// Stop after the database reaches this many bytes. Default is 10GiB.
    #[arg(long, default_value_t = TARGET_BYTES)]
    target_bytes: u64,
    /// redb path. Created if missing.
    #[arg(long)]
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ingest=info".into()),
        )
        .init();

    let args = Args::parse();
    let db_path = args.db.unwrap_or_else(Corpus::default_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = LocalStore::corpus(&db_path)?;
    let stored = store.meta("bytes").await?.unwrap_or(0);
    info!(
        path = %db_path.display(),
        stored,
        target = args.target_bytes,
        "corpus open"
    );
    if stored >= args.target_bytes {
        info!("corpus already reached the target");
        return Ok(());
    }
    let socket = socket_path()?;
    let peer = SocketPeer::new(socket);
    let (_stop, stop) = tokio::sync::watch::channel(false);
    let log_store = store.clone();
    let mut sink = StoreSink::new(store.clone());
    ReceiptSource::new(store, peer, args.target_bytes)
        .run(&mut sink, stop)
        .await?;
    let stored = log_store.meta("bytes").await?.unwrap_or(0);
    info!(stored, path = %db_path.display(), "ingest stopped");
    Ok(())
}
