//! Hold a small set of outbound mainnet sessions and serve blocks on a local socket.
//!
//! The peer service keeps the peer directory. A client asks it for a block and exits.
//! The node secret is `peer.key`, hex, from configuration.
//! Peers we have dialed are remembered in the peer catalog: each status change and
//! each newly covered block is a sighting, and the summary of that history picks
//! the next dial.

mod catalog;
mod dial;
mod history;
mod jobs;
mod key;
mod socket;

pub use history::{clamp_count, json_call, max_head, serving, BatchKind, HistoryPeer, SessionInfo};
pub use jobs::LocalPeer;

use dial::maintain;
use eth_p2p::Egress;
use eyre::WrapErr;
use key::parse_node_key;
use observability::{info, warn};
use reth_network_peers::pk2id;
use secp256k1::{SecretKey, SECP256K1};
use socket::{bind_socket, handle_conn};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch},
};

/// What the supervisor passes into [`serve`].
pub struct PeerConfig {
    pub socket: PathBuf,
    /// `peers.redb`. Opened once. A held writer lock is returned, not retried.
    pub catalog: PathBuf,
    pub egress: Option<Egress>,
    pub peer: config::Peer,
    pub network: config::Network,
}

/// Catalog owner. Dropping it releases the writer lock. The socket is not touched.
pub struct OpenedPeer {
    catalog: catalog::Catalog,
    local: LocalPeer,
}

impl OpenedPeer {
    /// The same table [`run`] dials. Empty until a handshake probe publishes a slot.
    pub fn local(&self) -> LocalPeer {
        self.local.clone()
    }
}

/// Open `peers.redb` and keep the writer lock. Does not unlink or bind a socket.
pub fn open(path: &Path) -> eyre::Result<OpenedPeer> {
    open_with(path, config::Store::default().peers)
}

/// Open `peers.redb` with `cache` bytes of page cache.
pub fn open_with(path: &Path, cache: usize) -> eyre::Result<OpenedPeer> {
    let catalog = catalog::Catalog::open_with(path, cache)?;
    Ok(OpenedPeer {
        catalog,
        local: LocalPeer::new(),
    })
}

/// Node key loaded and socket bound. [`run`] dials until the shutdown watch is set.
pub struct PreparedPeer {
    key: SecretKey,
    listener: UnixListener,
    egress: Option<Egress>,
    catalog_path: PathBuf,
    opened: OpenedPeer,
    peer: config::Peer,
    network: config::Network,
}

/// Load the node key and bind. `open` has already taken the catalog writer lock.
pub fn prepare(opened: OpenedPeer, settings: &PeerConfig) -> eyre::Result<PreparedPeer> {
    let key = parse_node_key(&settings.peer.key)?;
    let id = pk2id(&key.public_key(SECP256K1));
    info!(
        %id,
        peers = settings.peer.target,
        socket = %settings.socket.display(),
        catalog = %settings.catalog.display(),
        egress = ?settings.egress,
        "peer service starting"
    );
    let listener = bind_socket(&settings.socket)?;
    Ok(PreparedPeer {
        key,
        listener,
        egress: settings.egress,
        catalog_path: settings.catalog.clone(),
        opened,
        peer: settings.peer.clone(),
        network: settings.network.clone(),
    })
}

/// Open `peers.redb`, bind the socket, and dial until the process stops.
///
/// The catalog is opened before the socket file is touched. `Database already
/// open` leaves the running peer service and its socket alone.
pub async fn serve(config: PeerConfig) -> eyre::Result<()> {
    let opened = open(&config.catalog)?;
    let prepared = prepare(opened, &config)?;
    let (_tx, stop) = watch::channel(false);
    run(prepared, stop).await
}

/// Accept on the bound socket and dial until `stop` is set.
///
/// Session tasks end when their job channels drop. No devp2p disconnect
/// reason is written.
pub async fn run(prepared: PreparedPeer, mut stop: watch::Receiver<bool>) -> eyre::Result<()> {
    let PreparedPeer {
        key,
        listener,
        egress,
        catalog_path,
        opened,
        peer,
        network,
    } = prepared;
    let OpenedPeer { catalog, local } = opened;
    let (catalog_tx, catalog_rx) = mpsc::unbounded_channel();
    let slots = Arc::clone(&local.slots);
    let wanted = Arc::clone(&local.wanted);
    let maintain_stop = stop.clone();
    let mut maintain_task = tokio::spawn(maintain(
        key,
        peer,
        network.clone(),
        Arc::clone(&slots),
        Arc::clone(&wanted),
        egress,
        catalog,
        catalog_rx,
        maintain_stop,
    ));
    let catalog_path = Arc::new(catalog_path);
    let mut maintain_ended = None;
    let accept_result = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break Ok(());
                }
            }
            joined = &mut maintain_task => {
                maintain_ended = Some(joined);
                break Ok(());
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((conn, _)) => {
                        let slots = Arc::clone(&slots);
                        let wanted = Arc::clone(&wanted);
                        let catalog_tx = catalog_tx.clone();
                        let catalog_path = Arc::clone(&catalog_path);
                        let network = network.clone();
                        tokio::spawn(async move {
                            if let Err(err) =
                                handle_conn(conn, slots, wanted, catalog_tx, catalog_path, network)
                                    .await
                            {
                                warn!(%err, "client request failed");
                            }
                        });
                    }
                    Err(err) => break Err(err).wrap_err("accept"),
                }
            }
        }
    };
    drop(listener);
    drop(catalog_tx);
    // Closed before maintain returns, so a probe still in flight cannot publish
    // after the catalog writer lock is released.
    slots.lock().await.close();
    if accept_result.is_err() {
        maintain_task.abort();
    }
    let maintain_result = match maintain_ended {
        Some(result) => result,
        None => maintain_task.await,
    };
    drop(local);
    let maintain_err = match maintain_result {
        Ok(()) => None,
        Err(err) if err.is_cancelled() => None,
        Err(err) => Some(eyre::eyre!("peer maintain: {err}")),
    };
    match (accept_result, maintain_err) {
        (Err(err), _) => Err(err),
        (Ok(()), Some(err)) => Err(err),
        (Ok(()), None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::bind_socket;
    use std::{
        fs,
        process::{Command, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ratatoskr-peer-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn second_catalog_open_fails_without_retry() {
        let dir = temp_dir("lock");
        let path = dir.join("peers.redb");
        let socket = dir.join("peer.sock");
        let child = Command::new("flock")
            .arg("-x")
            .arg(&path)
            .arg("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let kill = Kill(child);
        let wait_started = Instant::now();
        loop {
            let status = Command::new("flock")
                .arg("-n")
                .arg("-x")
                .arg(&path)
                .arg("true")
                .status()
                .unwrap();
            if !status.success() {
                break;
            }
            assert!(
                wait_started.elapsed() < Duration::from_secs(2),
                "catalog lock was not taken"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::write(&socket, b"stale").unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            serve(PeerConfig {
                socket: socket.clone(),
                catalog: path,
                egress: None,
                peer: config::Peer {
                    target: 1,
                    ..config::Peer::default()
                },
                network: config::Network::default(),
            }),
        )
        .await;
        match result {
            Ok(Err(err)) => {
                let text = format!("{err:#}");
                assert!(
                    text.contains("Database already open"),
                    "fail-fast error was {text}"
                );
            }
            Ok(Ok(())) => panic!("serve returned without holding the accept loop"),
            Err(_) => panic!("serve did not fail within 2s"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read(&socket).unwrap(), b"stale");
        drop(kill);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_after_the_catalog_opens() {
        use std::os::unix::fs::FileTypeExt;
        let dir = temp_dir("bind");
        let path = dir.join("peers.redb");
        let socket = dir.join("peer.sock");
        fs::write(&socket, b"stale").unwrap();
        let opened = open(&path).unwrap();
        assert_eq!(fs::read(&socket).unwrap(), b"stale");
        let listener = bind_socket(&socket).unwrap();
        assert!(socket.metadata().unwrap().file_type().is_socket());
        drop(listener);
        drop(opened);
        let _ = fs::remove_dir_all(&dir);
    }
}
