//! Print the fee histogram of stored v3 pool identities.
//!
//! A running database service is read through its socket. Otherwise this
//! process opens the venue catalog itself.

use alloy_primitives::Address;
use clap::Parser;
use eyre::WrapErr;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use store::RemoteStore;
use venues::{Catalog, VenueRow, PANCAKE_V3, SUSHI_V3, UNI_V3, V3_FORK};

const PAGE: usize = 256;

#[derive(Debug, Parser)]
pub struct Univ3Args {
    /// Catalog directory. Default is the live catalog.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Also count Pancake v3, Sushi v3, and other v3 fork pools.
    #[arg(long)]
    pub any_emitter: bool,
}

pub async fn run(args: Univ3Args) -> eyre::Result<()> {
    let text = collect(args.data_dir.as_deref(), args.any_emitter).await?;
    print!("{text}");
    Ok(())
}

pub async fn collect(data_dir: Option<&Path>, any_emitter: bool) -> eyre::Result<String> {
    let (catalog_path, socket) = locations(data_dir)?;
    let mut kinds = vec![UNI_V3];
    if any_emitter {
        kinds.extend([PANCAKE_V3, SUSHI_V3, V3_FORK]);
    }
    let rows = if socket_open(&socket).await {
        remote_rows(&socket, &kinds).await?
    } else {
        let path = catalog_path;
        tokio::task::spawn_blocking(move || local_rows(&path, &kinds))
            .await
            .wrap_err("venue catalog")??
    };
    render(&rows)
}

fn locations(data_dir: Option<&Path>) -> eyre::Result<(PathBuf, PathBuf)> {
    match data_dir {
        Some(dir) => Ok((dir.join("entities.redb"), dir.join("db.sock"))),
        None => Ok((Catalog::default_path(), store::db_socket()?)),
    }
}

async fn socket_open(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

async fn remote_rows(socket: &Path, kinds: &[u8]) -> eyre::Result<Vec<VenueRow>> {
    let store = RemoteStore::new(socket);
    let mut rows = Vec::new();
    for &kind in kinds {
        rows.extend(store.scan_venues(kind).await?);
    }
    Ok(rows)
}

fn local_rows(path: &Path, kinds: &[u8]) -> eyre::Result<Vec<VenueRow>> {
    let catalog = Catalog::open(path)?;
    let mut rows = Vec::new();
    for &kind in kinds {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, PAGE)?;
            let full = page.len() == PAGE;
            let end = page.last().map(|row| row.id);
            rows.extend(page);
            if !full {
                break;
            }
            let Some(end) = end else {
                break;
            };
            if after.is_some_and(|prev| end <= prev) {
                break;
            }
            after = Some(end);
        }
    }
    Ok(rows)
}

pub fn render(rows: &[VenueRow]) -> eyre::Result<String> {
    let mut out = format!("identities {}\n", rows.len());
    let mut fees: BTreeMap<u32, u64> = BTreeMap::new();
    for row in rows {
        if let Some(fee) = row.fee {
            *fees.entry(fee).or_default() += 1;
        }
    }
    for (fee, count) in fees {
        out.push_str(&format!("fee {fee} pools {count}\n"));
    }
    for row in rows.iter().take(5) {
        let pool = Address::from_slice(&row.id.as_slice()[12..]);
        let line = serde_json::to_string(&serde_json::json!({
            "pool": pool,
            "tokens": row.tokens,
            "fee": row.fee,
            "tick": row.tick_spacing,
            "factory": row.factory,
            "block": row.block_number,
            "block_hash": row.block_hash,
            "tx_index": row.tx_index,
            "log_index": row.log_index,
        }))
        .wrap_err("encode identity")?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, B256};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::LocalStore;
    use venues::Put;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-univ3-{}-{}-{}",
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

    fn row(fee: u32) -> VenueRow {
        VenueRow {
            kind: UNI_V3,
            id: VenueRow::address_id(address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            factory: address!("1F98431c8aD98523631AE4a59f267346ea31F984"),
            tokens: vec![
                address!("1111111111111111111111111111111111111111"),
                address!("2222222222222222222222222222222222222222"),
            ],
            fee: Some(fee),
            tick_spacing: Some(60),
            block_number: Some(12_369_621),
            extra: None,
            block_hash: Some(B256::from([0xab; 32])),
            tx_index: Some(2),
            log_index: Some(5),
        }
    }

    #[test]
    fn a_fee_histogram_counts_each_pool_fee() {
        let mut other = row(500);
        other.id = VenueRow::address_id(address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        let text = render(&[row(3000), row(3000), other]).unwrap();
        assert!(text.starts_with("identities 3\n"), "{text}");
        assert!(text.contains("fee 500 pools 1\n"), "{text}");
        assert!(text.contains("fee 3000 pools 2\n"), "{text}");
        let json_lines: Vec<_> = text.lines().skip(3).collect();
        assert_eq!(json_lines.len(), 3, "{text}");
        let parsed: serde_json::Value = serde_json::from_str(json_lines[0]).unwrap();
        assert_eq!(parsed["fee"], 3000);
        assert_eq!(parsed["tick"], 60);
        assert_eq!(
            parsed["pool"].as_str().unwrap().to_ascii_lowercase(),
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[tokio::test]
    async fn an_unserved_catalog_is_read_in_process() {
        let dir = TempDir::new();
        let venues = dir.0.join("entities.redb");
        let catalog = Catalog::open(&venues).unwrap();
        catalog
            .commit_chunk("univ3", 1, &[row(3000)], Put::Merge)
            .unwrap();
        drop(catalog);
        let text = collect(Some(&dir.0), false).await.unwrap();
        assert!(text.contains("identities 1\n"), "{text}");
        assert!(text.contains("fee 3000 pools 1\n"), "{text}");
        assert!(!dir.0.join("univ3-pools.redb").exists());
    }

    #[tokio::test]
    async fn a_running_database_service_answers_the_fee_summary() {
        let dir = TempDir::new();
        let venues = dir.0.join("entities.redb");
        let catalog = Catalog::open(&venues).unwrap();
        catalog
            .commit_chunk("univ3", 1, &[row(500)], Put::Merge)
            .unwrap();
        drop(catalog);
        let store = Arc::new(LocalStore::parts(None, None, Some(&venues)).unwrap());
        let socket = dir.0.join("db.sock");
        let path = socket.clone();
        let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::select! {
                    result = store::serve(store, &path) => result.unwrap(),
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
        assert!(ready, "database service did not bind");
        let text = collect(Some(&dir.0), false).await.unwrap();
        let _ = cancel.send(());
        let _ = thread.join();
        assert!(text.contains("fee 500 pools 1\n"), "{text}");
        assert!(text.contains("identities 1\n"), "{text}");
    }
}
