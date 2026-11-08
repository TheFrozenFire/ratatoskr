//! Sync mainnet headers from `ratatoskr-peer` through the entity socket.
//!
//! Peers only serve headers by block range. [`pipeline::HeaderSource`] fetches
//! batches and submits them. The database server checks genesis and parents.
//! This process does not check parents itself and does not open `headers.redb`.
//!
//! A standalone source needs `ratatoskr-db` or `ratatoskr` on the entity socket.

use clap::Parser;
use eyre::WrapErr;
use ipc::{socket_path, SocketPeer};
use pipeline::{HeaderSource, Source, StoreSink};
use std::path::Path;
use store::RemoteStore;
use tokio::sync::watch;

/// Sync mainnet headers. Requires `ratatoskr-db` or `ratatoskr`.
///
/// Peers serve headers by block range. This process writes through the entity
/// socket and does not open `headers.redb`.
#[derive(Debug, Parser)]
#[command(name = "ratatoskr-headers")]
struct Args {}

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
    let _args = Args::parse();
    let socket = store::db_socket()?;
    tracing::info!(socket = %socket.display(), "header client");
    let store = open_remote(&socket);
    let source = HeaderSource::new(SocketPeer::new(socket_path()?), store.clone());
    let mut sink = StoreSink::new(store);
    // Held for the run. Dropping the sender makes `changed` ready immediately.
    let (_stop_tx, stop) = watch::channel(false);
    source.run(&mut sink, stop).await
}

fn open_remote(socket: &Path) -> RemoteStore {
    RemoteStore::new(socket)
}

#[cfg(test)]
mod tests {
    use super::open_remote;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::{serve, EntityStore, LocalStore};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-headers-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Serving {
        dir: TempDir,
        socket: PathBuf,
        cancel: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Serving {
        async fn start() -> Self {
            let dir = TempDir::new();
            let files = dir.0.join("files");
            std::fs::create_dir_all(&files).unwrap();
            let socket = dir.0.join("db.sock");
            let store = Arc::new(
                LocalStore::open(
                    &files.join("headers.redb"),
                    &files.join("corpus.redb"),
                    &files.join("venues.redb"),
                )
                .unwrap(),
            );
            let path = socket.clone();
            let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
            let thread = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async move {
                    tokio::select! {
                        result = serve(store, &path) => result.expect("serve"),
                        _ = cancel_rx => {}
                    }
                });
            });
            let mut ready = false;
            for _ in 0..100 {
                if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(ready, "serve did not bind");
            Self {
                dir,
                socket,
                cancel: Some(cancel),
                thread: Some(thread),
            }
        }
    }

    impl Drop for Serving {
        fn drop(&mut self) {
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[tokio::test]
    async fn header_client_uses_remote_store_and_does_not_create_headers_redb() {
        let serving = Serving::start().await;
        let store = open_remote(&serving.socket);
        assert_eq!(store.header_cursor().await.unwrap(), 0);
        assert!(!serving.dir.0.join("headers.redb").exists());
        assert!(serving.dir.0.join("files").join("headers.redb").exists());
    }
}
