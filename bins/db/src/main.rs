//! Serve the entity files on `~/.config/ratatoskr/db.sock`.

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use store::{db_socket, serve, LocalStore};

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-db")]
struct Args {
    /// Entity directory. Default is `data/ratatoskr` in the repo.
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let dir = args.dir.unwrap_or_else(data_dir);
    let store = LocalStore::open(
        &dir.join("headers.redb"),
        &dir.join("corpus.redb"),
        &dir.join("venues.redb"),
    )?;
    let socket = db_socket()?;
    tracing::info!(dir = %dir.display(), socket = %socket.display(), "entity store");
    serve(Arc::new(store), &socket).await
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}
