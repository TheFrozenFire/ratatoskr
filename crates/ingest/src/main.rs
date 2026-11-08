//! Pull receipt ranges from `ratatoskr-peer` into the entity store.
//!
//! The ranges cover the Uniswap v3 factory deployment, the Uniswap v4
//! PoolManager deployment, and a recent window. The process stops once the
//! database holds the requested number of bytes. A restart continues from the
//! saved cursor. This process does not create a local corpus file.
//!
//! A standalone source needs `ratatoskr-db` or `ratatoskr` on the entity socket.

use clap::Parser;
use ipc::{socket_path, SocketPeer};
use pipeline::{ReceiptSource, Source, StoreSink};
use store::{EntityStore, RemoteStore};
use tracing::info;

const TARGET_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Pull receipt ranges into the entity store. Requires `ratatoskr-db` or `ratatoskr`.
///
/// Stops once the database holds the requested number of bytes. This process
/// does not create a local corpus file.
#[derive(Debug, Parser)]
#[command(name = "ratatoskr-ingest")]
struct Args {
    /// Stop after the database reaches this many bytes. Default is 10GiB.
    #[arg(long, default_value_t = TARGET_BYTES)]
    target_bytes: u64,
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
    let socket = store::db_socket()?;
    let store = RemoteStore::new(&socket);
    let stored = store.meta("bytes").await?.unwrap_or(0);
    info!(
        socket = %socket.display(),
        stored,
        target = args.target_bytes,
        "corpus"
    );
    if stored >= args.target_bytes {
        info!("corpus already reached the target");
        return Ok(());
    }
    let peer = SocketPeer::new(socket_path()?);
    let (_stop, stop) = tokio::sync::watch::channel(false);
    let log_store = store.clone();
    let mut sink = StoreSink::new(store.clone());
    ReceiptSource::new(store, peer, args.target_bytes)
        .run(&mut sink, stop)
        .await?;
    let stored = log_store.meta("bytes").await?.unwrap_or(0);
    info!(stored, socket = %socket.display(), "ingest stopped");
    Ok(())
}
