//! Read-only open of every file the live process writes.

use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use store_model::{HeaderRecord, TokenRecord};

use super::{
    Corpus, DiscoveryRead, EntityRead, FeedRead, HeaderChain, PeerRead, SpotRead, WorkRead,
};

/// One file that failed to open does not close the others.
#[derive(Default)]
pub struct Shelf {
    pub headers: Option<HeaderChain>,
    pub corpus: Option<Corpus>,
    pub discovery: Option<DiscoveryRead>,
    pub entities: Option<EntityRead>,
    pub work: Option<WorkRead>,
    pub peers: Option<PeerRead>,
    pub feeds: Option<FeedRead>,
    pub spots: Option<SpotRead>,
}

impl Shelf {
    /// Open each file read-only. A missing file leaves that slot empty.
    pub fn open(dir: &Path, caches: &config::Store) -> Self {
        let dir = catalog_dir(dir);
        Self {
            headers: HeaderChain::open_with(&dir.join("headers.redb"), caches.headers).ok(),
            corpus: Corpus::open_with(&dir.join("corpus.redb"), caches.corpus).ok(),
            discovery: DiscoveryRead::open(&dir.join("discovery.redb"), caches.discovery).ok(),
            entities: EntityRead::open_with(&dir, caches.read).ok(),
            work: WorkRead::open(&dir.join("work.redb"), caches.work).ok(),
            peers: PeerRead::open(&dir.join("peers.redb"), caches.peers).ok(),
            feeds: FeedRead::open(&dir.join("feeds.redb"), caches.feeds).ok(),
            spots: SpotRead::open(&dir.join("spots.redb"), caches.spots).ok(),
        }
    }

    pub fn tip(&self) -> Option<HeaderRecord> {
        self.headers
            .as_ref()
            .and_then(|chain| chain.tip().ok().flatten())
    }

    pub fn corpus_bytes(&self) -> Option<u64> {
        self.corpus.as_ref()?.meta("bytes").ok().flatten()
    }

    pub fn corpus_holes(&self) -> Option<u64> {
        self.corpus.as_ref()?.meta("holes").ok().flatten()
    }

    pub fn cursors(&self) -> Vec<(String, u64)> {
        self.discovery
            .as_ref()
            .and_then(|discovery| discovery.cursors().ok())
            .unwrap_or_default()
    }

    pub fn tokens(&self, after: Option<Address>, limit: usize) -> Vec<TokenRecord> {
        self.entities
            .as_ref()
            .and_then(|entities| entities.page_tokens(after, limit).ok())
            .unwrap_or_default()
    }
}

fn catalog_dir(path: &Path) -> PathBuf {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if name.ends_with(".redb") => path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Catalog, ConduitBody};
    use alloy_primitives::{address, Address, B256};
    use num_bigint::BigInt;
    use routing::{Action, SlopeSegment};
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use store_model::{
        CurveRecord, DirectoryRow, HeaderRecord, ImmutableRow, MutableWrite, Put, Sighting, Spot,
        TokenRecord, VenueRow, WorkJob, MAINNET_GENESIS_HASH, SAW_STATUS,
    };

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-shelf-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn blank_dir() -> DirectoryRow {
        DirectoryRow {
            proxy: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            name: "eth-usd".into(),
            path: String::new(),
            category: String::new(),
            feed_type: String::new(),
            contract_version: 0,
            decimals: 8,
            multiply: String::new(),
            heartbeat: 0,
            threshold: String::new(),
            base_asset: String::new(),
            quote_asset: String::new(),
            product_type: String::new(),
            product_type_code: String::new(),
            attribute_type: String::new(),
            market_hours: String::new(),
            shutdown_date: String::new(),
            clic_product_name: String::new(),
            hidden: false,
            secondary_proxy: None,
            svr_label: String::new(),
            directory_aggregator: Address::ZERO,
        }
    }

    #[test]
    fn a_reader_beside_the_writer_sees_each_committed_row() {
        let dir = Temp::new();
        let token_in = address!("1111111111111111111111111111111111111111");
        let token_out = address!("2222222222222222222222222222222222222222");
        let pool = [9_u8; 32];
        let id = B256::from(pool);
        let venue = VenueRow {
            kind: 10,
            id,
            factory: Address::ZERO,
            tokens: vec![token_in, token_out],
            fee: Some(3000),
            tick_spacing: None,
            block_number: Some(12),
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let token = TokenRecord {
            address: token_in,
            symbol: "WETH".into(),
            decimals: Some(18),
            wraps: None,
        };
        let segment = SlopeSegment {
            width: BigInt::from(4),
            output: BigInt::from(3),
            action: Action { kind: 10, pool },
        };
        let catalog = Catalog::open(&dir.0).unwrap();
        catalog.put_tokens(&[token.clone()]).unwrap();
        catalog
            .commit_chunk("list:test", 40, &[venue.clone()], Put::Merge)
            .unwrap();
        catalog
            .put_immutable(&[ImmutableRow {
                kind: 10,
                id,
                body: vec![1],
            }])
            .unwrap();
        catalog
            .put_mutable(&MutableWrite {
                kind: 10,
                id,
                block: 77,
                class: 0,
                bitmap: B256::ZERO,
                tick_count: 2,
                truncated: false,
                next_scan: None,
                body: Vec::new(),
            })
            .unwrap();
        catalog
            .put_curve(&CurveRecord {
                kind: 10,
                id,
                generation: 1,
                version: 1,
                payload: Vec::new(),
            })
            .unwrap();
        assert!(!catalog
            .store_conduit(&ConduitBody {
                token_in,
                token_out,
                snapshot: Vec::new(),
                kept: vec![[1_u8; 73]],
                block: 7,
                generation: 3,
                segments: vec![segment.clone()],
            })
            .unwrap());
        catalog
            .enqueue_work(
                &[WorkJob {
                    stage: 4,
                    kind: 10,
                    id,
                    lease_until: 0,
                    attempt: 2,
                    not_before: 0,
                    generation: 0,
                    tick_cursor: 0,
                    class: 0,
                    priority: 0,
                    error: "lens".into(),
                }],
                0,
            )
            .unwrap();
        let feeds = crate::service::Feeds::open(&dir.0.join("feeds.redb")).unwrap();
        feeds.apply_directory(&[blank_dir()]).unwrap();
        let spots = crate::service::Spots::open(&dir.0.join("spots.redb")).unwrap();
        spots
            .replace(&[Spot {
                token: token_in,
                numeraire: token_out,
                size: BigInt::from(1),
                bought: BigInt::from(2),
                sold: BigInt::from(3),
                hops: 1,
                block: 8,
            }])
            .unwrap();
        let mut peers = crate::service::Peers::open(&dir.0.join("peers.redb")).unwrap();
        peers.record(Sighting {
            id: reth_network_peers::PeerId::ZERO,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            tcp_port: 30303,
            udp_port: 30303,
            outcome: SAW_STATUS,
            eth: 68,
            earliest: None,
            latest: Some(9),
            served: None,
        });
        let chain = HeaderChain::create(&dir.0.join("headers.redb")).unwrap();
        let second = B256::repeat_byte(0x11);
        chain
            .append(
                &[
                    HeaderRecord {
                        number: 0,
                        hash: MAINNET_GENESIS_HASH,
                        parent_hash: B256::ZERO,
                        timestamp: 1,
                        receipts_root: B256::ZERO,
                        logs_bloom: [0; 256],
                    },
                    HeaderRecord {
                        number: 1,
                        hash: second,
                        parent_hash: MAINNET_GENESIS_HASH,
                        timestamp: 9,
                        receipts_root: B256::ZERO,
                        logs_bloom: [0; 256],
                    },
                ],
                0,
                B256::ZERO,
            )
            .unwrap();

        let shelf = Shelf::open(&dir.0, &config::Store::default());
        let tip = shelf.tip().expect("header");
        assert_eq!(tip.number, 1);
        assert_eq!(tip.timestamp, 9);
        assert_eq!(shelf.cursors(), vec![("list:test".into(), 40)]);
        let venues = shelf
            .entities
            .as_ref()
            .unwrap()
            .page_venues(10, None, 8)
            .unwrap();
        assert_eq!(venues[0].block_number, Some(12));
        let card = shelf.entities.as_ref().unwrap().pool_card(10, id).unwrap();
        assert!(card.immutable);
        assert_eq!(card.block, Some(77));
        assert_eq!(card.curve_generation, Some(1));
        let conduits = shelf
            .entities
            .as_ref()
            .unwrap()
            .page_conduits(None, 8)
            .unwrap();
        assert_eq!(conduits[0].token_in, token_in);
        let stored = shelf
            .entities
            .as_ref()
            .unwrap()
            .conduit(token_in, token_out)
            .unwrap()
            .expect("conduit");
        assert_eq!(stored.segments[0].width, BigInt::from(4));
        assert_eq!(stored.segments[0].output, BigInt::from(3));
        let tokens = shelf.tokens(None, 8);
        assert_eq!(tokens[0].symbol, "WETH");
        let jobs = shelf
            .work
            .as_ref()
            .unwrap()
            .page_jobs(None, None, 8)
            .unwrap();
        assert_eq!(jobs[0].error, "lens");
        assert_eq!(jobs[0].attempt, 2);
        let feeds = shelf.feeds.as_ref().unwrap().page(None, 8).unwrap();
        assert_eq!(feeds[0].feed.row.name, "eth-usd");
        assert_eq!(feeds[0].feed.row.decimals, 8);
        let spots = shelf.spots.as_ref().unwrap().page(None, 8).unwrap();
        assert_eq!(spots[0].hops, 1);
        assert_eq!(spots[0].block, 8);
        let peers = shelf.peers.as_ref().unwrap().page(None, 8).unwrap();
        assert_eq!(peers[0].summary.eth, 68);
        drop(catalog);
    }
}
