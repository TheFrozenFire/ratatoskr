//! Serve the entity files on `~/.config/ratatoskr/db.sock`.

use clap::Parser;
use eyre::WrapErr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use store::{serve, LocalStore};

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-db")]
struct Args {
    /// Entity directory. Default is ~/.local/share/ratatoskr.
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "db=info".into()),
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
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/ratatoskr")
}

fn db_socket() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").wrap_err("HOME")?;
    let dir = PathBuf::from(home).join(".config/ratatoskr");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir.join("db.sock"))
}
