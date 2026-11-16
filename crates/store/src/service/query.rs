//! The reads a search runs against one snapshot of `entities.redb`.
//!
//! Each query names its parameter and its return. The caller opens the
//! transaction. [`Catalog`] and [`EntityRead`] open one transaction per call
//! and then run the same query.

use alloy_primitives::{Address, B256};
use routing::SlopeSegment;
use store_model::{TokenRecord, VenueRow};

use super::catalog;
use super::conduit::{self, StoredConduit};

/// Every token row. The key is the 20-byte address.
pub struct TokenRows;

impl TokenRows {
    pub fn run(read: &redb::ReadTransaction) -> eyre::Result<Vec<TokenRecord>> {
        catalog::token_rows(read)
    }
}

/// One ordered pair. The key is `token_in` followed by `token_out`.
pub struct ConduitPoint {
    pub token_in: Address,
    pub token_out: Address,
}

impl ConduitPoint {
    pub fn run(&self, read: &redb::ReadTransaction) -> eyre::Result<Option<StoredConduit>> {
        conduit::conduit_point(read, self.token_in, self.token_out)
    }
}

/// The conduits table of one snapshot. Repeated point gets share it.
pub struct ConduitRead {
    table: redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
}

impl ConduitRead {
    pub fn open(read: &redb::ReadTransaction) -> eyre::Result<Option<Self>> {
        Ok(conduit::open_conduit_table(read)?.map(|table| Self { table }))
    }

    pub fn get(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<StoredConduit>> {
        conduit::conduit_get(&self.table, token_in, token_out)
    }
}

/// Every stored pair, including a curve with no slope.
pub struct ConduitScan;

impl ConduitScan {
    pub fn run(
        read: &redb::ReadTransaction,
        mut visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        conduit::conduit_rows(read, |token_in, token_out, _block, segments| {
            visit(token_in, token_out, segments)
        })
    }
}

/// One venue identity. The key is the kind byte followed by the pool id.
pub struct VenuePoint {
    pub kind: u8,
    pub id: B256,
}

impl VenuePoint {
    pub fn run(&self, read: &redb::ReadTransaction) -> eyre::Result<Option<VenueRow>> {
        catalog::venue_row(read, self.kind, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::conduit::ConduitBody;
    use crate::service::{Catalog, EntityRead};
    use alloy_primitives::{address, Address, B256};
    use num_bigint::BigInt;
    use routing::{Action, SlopeSegment};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use store_model::{Put, TokenRecord, VenueRow};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-query-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
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

    fn small() -> config::Store {
        config::Store {
            discovery: 1024 * 1024,
            entities: 1024 * 1024,
            work: 1024 * 1024,
            read: 1024 * 1024,
            ..config::Store::default()
        }
    }

    #[test]
    fn a_shared_read_matches_the_service_for_tokens_conduits_and_venues() {
        let dir = TempDir::new();
        let priced_in = address!("1111111111111111111111111111111111111111");
        let priced_out = address!("2222222222222222222222222222222222222222");
        let empty_in = address!("3333333333333333333333333333333333333333");
        let empty_out = address!("4444444444444444444444444444444444444444");
        let missing = address!("5555555555555555555555555555555555555555");
        let pool = [9_u8; 32];
        let segment = SlopeSegment {
            width: BigInt::from(4),
            output: BigInt::from(3),
            action: Action { kind: 10, pool },
        };
        let venue = VenueRow {
            kind: 10,
            id: B256::from(pool),
            factory: address!("6666666666666666666666666666666666666666"),
            tokens: vec![priced_in, priced_out],
            fee: Some(3000),
            tick_spacing: None,
            block_number: Some(12),
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let token = TokenRecord {
            address: priced_in,
            symbol: "P".to_string(),
            decimals: Some(18),
            wraps: None,
        };
        {
            let catalog = Catalog::open_with(&dir.0, &small()).unwrap();
            catalog.put_tokens(&[token.clone()]).unwrap();
            catalog
                .commit_chunk("query", 1, &[venue.clone()], Put::Merge)
                .unwrap();
            let priced = body(priced_in, priced_out, vec![segment.clone()]);
            let empty = body(empty_in, empty_out, Vec::new());
            assert!(!catalog.store_conduit(&priced).unwrap());
            assert!(!catalog.store_conduit(&empty).unwrap());

            let read = catalog.begin_read().unwrap();
            assert_eq!(TokenRows::run(&read).unwrap(), vec![token.clone()]);
            assert_eq!(TokenRows::run(&read).unwrap(), catalog.tokens().unwrap());

            let priced_row = ConduitPoint {
                token_in: priced_in,
                token_out: priced_out,
            }
            .run(&read)
            .unwrap()
            .expect("priced");
            assert_eq!(priced_row.segments, vec![segment]);
            assert_eq!(
                priced_row,
                catalog
                    .read_conduit(priced_in, priced_out)
                    .unwrap()
                    .expect("priced")
            );
            let empty_row = ConduitPoint {
                token_in: empty_in,
                token_out: empty_out,
            }
            .run(&read)
            .unwrap()
            .expect("empty");
            assert!(empty_row.segments.is_empty());
            assert!(ConduitPoint {
                token_in: priced_in,
                token_out: missing,
            }
            .run(&read)
            .unwrap()
            .is_none());

            let table = ConduitRead::open(&read).unwrap().expect("conduits");
            assert_eq!(
                table
                    .get(priced_in, priced_out)
                    .unwrap()
                    .expect("priced")
                    .segments,
                priced_row.segments
            );

            let mut seen = Vec::new();
            ConduitScan::run(&read, |token_in, token_out, segments| {
                seen.push((token_in, token_out, segments.is_empty()));
                Ok(())
            })
            .unwrap();
            seen.sort_by_key(|(token_in, _, _)| *token_in);
            assert_eq!(
                seen,
                vec![(priced_in, priced_out, false), (empty_in, empty_out, true)]
            );

            let found = VenuePoint {
                kind: venue.kind,
                id: venue.id,
            }
            .run(&read)
            .unwrap()
            .expect("venue");
            assert_eq!(found, venue);
            assert_eq!(
                found,
                catalog
                    .read_venue(venue.kind, venue.id)
                    .unwrap()
                    .expect("venue")
            );
            assert!(VenuePoint {
                kind: venue.kind,
                id: B256::ZERO,
            }
            .run(&read)
            .unwrap()
            .is_none());
        }
        let reader = EntityRead::open_with(&dir.0, small().read).unwrap();
        let read = reader.begin_read().unwrap();
        assert_eq!(
            TokenRows::run(&read).unwrap(),
            vec![TokenRecord {
                address: priced_in,
                symbol: "P".to_string(),
                decimals: Some(18),
                wraps: None,
            }]
        );
        assert!(ConduitScan::run(&read, |token_in, token_out, segments| {
            if token_in == empty_in {
                assert_eq!(token_out, empty_out);
                assert!(segments.is_empty());
            }
            Ok(())
        })
        .is_ok());
    }

    fn body(token_in: Address, token_out: Address, segments: Vec<SlopeSegment>) -> ConduitBody {
        ConduitBody {
            token_in,
            token_out,
            snapshot: Vec::new(),
            kept: vec![[1_u8; 73]],
            block: 7,
            generation: 3,
            segments,
        }
    }
}
