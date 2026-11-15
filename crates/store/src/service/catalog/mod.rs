//! Venue catalog across three redb files.
//!
//! `entities.redb` holds identities and later pool state. `discovery.redb`
//! holds list cursors, bloom hits, and era1 receipt bodies. `work.redb` holds
//! only leases. redb cannot commit across files. Identities are written
//! first and the cursor second, so a crash retries the chunk instead of
//! skipping it. Deleting `work.redb` drops leases, not rows.
//!
//! The identity table keeps the name `venues`. A path whose file name ends in
//! `.redb` selects that file's parent.

mod discovery;
mod identity;
mod sample;
mod token;

pub(crate) use identity::read_venue_row;
pub use sample::{
    append_dropped_ids, append_fill_ids, fill_identities, read_fill_ids, sample_identities,
    write_fill_ids, FillReport, SampleReport,
};

use crate::storage::{Database, ReadDatabase};
use redb::TableDefinition;
use std::path::{Path, PathBuf};

pub(crate) const VENUES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("venues");
const CURSORS: TableDefinition<&str, u64> = TableDefinition::new("cursors");
/// Bloom hit. Key is the job name, `0xff`, then the block number in big-endian.
const HITS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hits");
/// Next block a job has already bloom-tested. Exclusive.
const HIT_SCAN: TableDefinition<&str, u64> = TableDefinition::new("hit_scan");
/// Era1 receipt payload for one block, so a retry does not range-read it again.
const BODIES: TableDefinition<u64, &[u8]> = TableDefinition::new("bodies");
/// Token symbol, decimals, and an optional wrap counterpart. Key is the address.
const TOKENS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tokens");
/// List-view index and log-root deploy block. The value is a `u64`.
const NOTES: TableDefinition<&str, u64> = TableDefinition::new("notes");

pub struct Catalog {
    pub(crate) discovery: Database,
    pub(crate) entities: Database,
    pub(crate) work: Database,
}

/// Read `entities.redb` while another process writes it.
///
/// Discovery and the work queue stay closed.
pub struct EntityRead {
    pub(crate) entities: ReadDatabase,
}

/// redb's builder default is 1 GiB. These are the budgets this process sets.
const ENTITIES_CACHE: usize = 256 * 1024 * 1024;
const DISCOVERY_CACHE: usize = 128 * 1024 * 1024;
const WORK_CACHE: usize = 16 * 1024 * 1024;

/// Page cache for a process that follows the writer.
///
/// The writer keeps the larger entity cache. This reader scans conduits and
/// then reads the pairs a trade names.
const READ_CACHE: usize = 64 * 1024 * 1024;

impl Catalog {
    /// Open the catalog directory.
    ///
    /// A path whose file name ends in `.redb` selects that file's parent, so
    /// existing callers that pass `venues.redb` keep the same directory.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let dir = catalog_dir(path);
        Ok(Self {
            discovery: Database::open_or_create(&dir.join("discovery.redb"), DISCOVERY_CACHE)?,
            entities: Database::open_or_create(&dir.join("entities.redb"), ENTITIES_CACHE)?,
            work: Database::open_or_create(&dir.join("work.redb"), WORK_CACHE)?,
        })
    }

    /// Page caches for the three catalog files.
    pub fn cache_readings(&self) -> Vec<ipc::db::CacheReading> {
        vec![
            self.discovery.reading(),
            self.entities.reading(),
            self.work.reading(),
        ]
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr/entities.redb")
    }
}

impl EntityRead {
    /// Open `entities.redb` in the catalog directory.
    ///
    /// A path whose file name ends in `.redb` selects that file's parent.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let file = catalog_dir(path).join("entities.redb");
        Ok(Self {
            entities: ReadDatabase::open(&file, READ_CACHE)?,
        })
    }
}

fn catalog_dir(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name.ends_with(".redb") {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::identity::merge;
    use super::sample::{allocate_sample, anchor_id};
    use super::*;
    use alloy_primitives::{address, B256};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use store_model::{
        decode_row, encode_row, Put, ReceiptBody, TokenRecord, VenueRow, FLAG_OBS, RECORD_VERSION,
        RECORD_VERSION_OBS, UNI_V2, UNI_V3, V3_FORK,
    };

    /// redb's builder default is 1 GiB. A test file uses a 1 MiB budget.
    fn empty_db(path: &std::path::Path) -> Database {
        Database::create(path, 1024 * 1024).unwrap()
    }

    fn sample() -> VenueRow {
        VenueRow {
            kind: 1,
            id: VenueRow::address_id(address!("1111111111111111111111111111111111111111")),
            factory: address!("2222222222222222222222222222222222222222"),
            tokens: vec![
                address!("3333333333333333333333333333333333333333"),
                address!("4444444444444444444444444444444444444444"),
            ],
            fee: Some(3000),
            tick_spacing: Some(-10),
            block_number: Some(12_345),
            extra: Some(address!("5555555555555555555555555555555555555555")),
            block_hash: None,
            tx_index: None,
            log_index: None,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            // Parallel tests can sample the same nanosecond and then fight over one redb lock.
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-catalog-{}-{}-{}",
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

    fn history() -> VenueRow {
        let mut row = sample();
        row.block_hash = Some(alloy_primitives::b256!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        row.tx_index = Some(4);
        row.log_index = Some(9);
        row
    }

    #[test]
    fn row_codec_roundtrip() {
        let row = sample();
        let bytes = encode_row(&row).unwrap();
        let back = decode_row(row.kind, row.id, &bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn a_row_without_an_observation_stays_version_one() {
        let row = sample();
        let bytes = encode_row(&row).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION);
        assert_eq!(bytes[1] & FLAG_OBS, 0);
        assert_eq!(decode_row(row.kind, row.id, &bytes).unwrap(), row);
    }

    #[test]
    fn a_history_row_is_version_two() {
        let row = history();
        let bytes = encode_row(&row).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION_OBS);
        assert_ne!(bytes[1] & FLAG_OBS, 0);
        let back = decode_row(row.kind, row.id, &bytes).unwrap();
        assert_eq!(back.block_hash, row.block_hash);
        assert_eq!(back.tx_index, row.tx_index);
        assert_eq!(back.log_index, row.log_index);
        assert_eq!(back, row);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_row(row.kind, row.id, &trailing).is_err());
        let mut bare = encode_row(&sample()).unwrap();
        bare[0] = RECORD_VERSION_OBS;
        assert!(decode_row(row.kind, row.id, &bare).is_err());
    }

    #[test]
    fn version_one_with_the_observation_flag_fails() {
        let row = sample();
        let mut bytes = encode_row(&row).unwrap();
        bytes[1] |= FLAG_OBS;
        assert!(decode_row(row.kind, row.id, &bytes).is_err());
    }

    #[test]
    fn merge_keeps_a_stored_observation_when_the_new_row_has_none() {
        let stored = history();
        let bytes = encode_row(&stored).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION_OBS);
        let old = decode_row(stored.kind, stored.id, &bytes).unwrap();
        let mut incoming = sample();
        incoming.fee = Some(100);
        let merged = merge(old, incoming);
        assert_eq!(merged.block_hash, stored.block_hash);
        assert_eq!(merged.tx_index, stored.tx_index);
        assert_eq!(merged.log_index, stored.log_index);
        assert_eq!(merged.fee, Some(100));
        let again = encode_row(&merged).unwrap();
        assert_eq!(again[0], RECORD_VERSION_OBS);
        assert_eq!(decode_row(merged.kind, merged.id, &again).unwrap(), merged);
    }

    #[test]
    fn page_venues_excludes_the_after_id_and_other_kinds() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let mut rows = Vec::new();
        for n in 1..=3 {
            let mut row = sample();
            row.kind = UNI_V3;
            let mut id = B256::ZERO;
            id[31] = n;
            row.id = id;
            rows.push(row);
        }
        let mut other = rows[0].clone();
        other.kind = UNI_V2;
        catalog.commit_chunk("page", 1, &rows, Put::Merge).unwrap();
        catalog
            .commit_chunk("page", 2, &[other], Put::Merge)
            .unwrap();
        let page = catalog.page_venues(UNI_V3, None, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id[31], 1);
        let next = catalog.page_venues(UNI_V3, Some(page[1].id), 2).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id[31], 3);
        assert!(catalog
            .page_venues(UNI_V3, Some(next[0].id), 2)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn merge_keeps_a_block_when_the_new_row_has_none() {
        let old = sample();
        let mut new = sample();
        new.block_number = None;
        new.tokens.clear();
        new.fee = Some(100);
        let merged = merge(old.clone(), new);
        assert_eq!(merged.block_number, old.block_number);
        assert_eq!(merged.tokens, old.tokens);
        assert_eq!(merged.fee, Some(100));
    }

    #[test]
    fn commit_is_resumable_and_if_absent_does_not_replace() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let row = sample();
        catalog
            .commit_chunk("list:test", 10, std::slice::from_ref(&row), Put::Merge)
            .unwrap();
        assert_eq!(catalog.cursor("list:test").unwrap(), Some(10));
        let mut other = row.clone();
        other.factory = address!("6666666666666666666666666666666666666666");
        let wrote = catalog
            .commit_chunk("list:test", 11, &[other], Put::IfAbsent)
            .unwrap();
        assert_eq!(wrote, 0);
        let counts = catalog.tally_by_kind().unwrap();
        assert_eq!(counts.get(&(1, row.factory)).copied(), Some(1));
        assert!(dir.0.join("entities.redb").exists());
        assert!(dir.0.join("discovery.redb").exists());
        assert!(dir.0.join("work.redb").exists());
        assert!(!dir.0.join("venues.redb").exists());
    }

    #[test]
    fn hits_and_bodies_survive_a_reopen() {
        let dir = TempDir::new();
        let body = ReceiptBody {
            hash: alloy_primitives::b256!(
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            parent_hash: B256::ZERO,
            timestamp: 9,
            receipts_root: B256::ZERO,
            payload: vec![1, 2, 3],
        };
        {
            let catalog = Catalog::open(&dir.0).unwrap();
            assert!(catalog.hit_scan("logs:test").unwrap().is_none());
            catalog.commit_hits("logs:test", 10, &[3, 4]).unwrap();
            catalog.commit_hits("logs:test", 8, &[1]).unwrap();
            catalog.put_bodies(&[(4, body.clone())]).unwrap();
        }
        let catalog = Catalog::open(&dir.0).unwrap();
        assert_eq!(catalog.hit_scan("logs:test").unwrap(), Some(10));
        assert_eq!(
            catalog.hits_from("logs:test", 0, 10).unwrap(),
            vec![1, 3, 4]
        );
        assert_eq!(catalog.hits_from("logs:test", 4, 5).unwrap(), vec![4]);
        let stored = catalog.read_bodies(&[4, 5]).unwrap();
        assert_eq!(stored[0].as_ref(), Some(&body));
        assert!(stored[1].is_none());
    }

    #[test]
    fn sample_takes_small_kinds_and_a_stride_of_the_large_one() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = empty_db(&src);
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..30_u8 {
                    let mut row = sample();
                    row.kind = if index < 5 { V3_FORK } else { UNI_V2 };
                    row.id[31] = index;
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let report = sample_identities(&src, &dest.0, 10).unwrap();
        assert_eq!(report.copied, 10);
        assert_eq!(report.by_kind.get(&V3_FORK).copied(), Some(5));
        assert_eq!(report.by_kind.get(&UNI_V2).copied(), Some(5));
        assert!(src.exists());
        assert!(!src_dir.0.join("entities.redb").exists());
        assert!(dest.0.join("entities.redb").exists());
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(V3_FORK, None, 10).unwrap().len(), 5);
    }

    #[test]
    fn fill_adds_identities_the_sample_did_not_copy() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = empty_db(&src);
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..30_u8 {
                    let mut row = sample();
                    row.kind = UNI_V2;
                    row.id[31] = index;
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let first = sample_identities(&src, &dest.0, 10).unwrap();
        assert_eq!(first.copied, 10);
        let filled = fill_identities(&src, &dest.0, 5).unwrap();
        assert_eq!(filled.copied, 5);
        assert_eq!(filled.ids.len(), 5);
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(UNI_V2, None, 30).unwrap().len(), 15);
        drop(catalog);
        let again = fill_identities(&src, &dest.0, 5).unwrap();
        assert_eq!(again.copied, 5);
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(UNI_V2, None, 30).unwrap().len(), 20);
        let mut seen = std::collections::HashSet::new();
        for row in catalog.page_venues(UNI_V2, None, 30).unwrap() {
            assert!(seen.insert(row.id));
        }
    }

    #[test]
    fn sample_keeps_the_anchor_pool_off_the_stride() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = empty_db(&src);
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..20_u8 {
                    let mut row = sample();
                    row.kind = UNI_V3;
                    row.id[31] = index;
                    if index == 19 {
                        row.id = anchor_id();
                    }
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let report = sample_identities(&src, &dest.0, 3).unwrap();
        assert!(report.anchor);
        assert!(report.copied >= 3);
        let catalog = Catalog::open(&dest.0).unwrap();
        let rows = catalog.page_venues(UNI_V3, None, 10).unwrap();
        assert!(rows.iter().any(|row| row.id == anchor_id()));
    }

    #[test]
    fn allocate_covers_every_live_kind_near_ten_thousand() {
        let counts = [
            (1_u8, 527_256_u64),
            (2, 4_818),
            (3, 8_110),
            (4, 212),
            (5, 48_693),
            (6, 1_498),
            (7, 253),
            (8, 12),
            (9, 102),
            (10, 192_676),
            (11, 334),
            (12, 253),
            (13, 2_501),
            (14, 190),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let plan = allocate_sample(&counts, 10_000);
        let total: u64 = plan.values().sum();
        assert_eq!(total, 10_000);
        for kind in [4_u8, 6, 7, 8, 9, 11, 12, 13, 14] {
            assert_eq!(plan.get(&kind).copied(), counts.get(&kind).copied());
        }
        for kind in [1_u8, 2, 3, 5, 10] {
            assert!(plan.get(&kind).copied().unwrap_or(0) >= 400, "{plan:?}");
        }
    }

    #[test]
    fn a_token_keeps_its_symbol_when_a_later_write_is_blank() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let token = address!("9999999999999999999999999999999999999999");
        catalog
            .put_tokens(&[TokenRecord {
                address: token,
                symbol: "ABC".into(),
                decimals: Some(6),
                wraps: None,
            }])
            .unwrap();
        catalog
            .put_tokens(&[TokenRecord {
                address: token,
                symbol: String::new(),
                decimals: None,
                wraps: Some(alloy_primitives::Address::ZERO),
            }])
            .unwrap();
        let stored = catalog.token(token).unwrap().unwrap();
        assert_eq!(stored.symbol, "ABC");
        assert_eq!(stored.decimals, Some(6));
        assert_eq!(stored.wraps, Some(alloy_primitives::Address::ZERO));
        catalog.put_note("shape:list:uni-v2", 1).unwrap();
        assert_eq!(catalog.note("shape:list:uni-v2").unwrap(), Some(1));
        assert!(catalog.note("missing").unwrap().is_none());
        let row = sample();
        catalog
            .commit_chunk("list:test", 1, &[row], Put::Merge)
            .unwrap();
        let missing = catalog.missing_tokens(10).unwrap();
        assert_eq!(missing.len(), 2);
        catalog
            .put_tokens(&[TokenRecord {
                address: missing[0],
                symbol: "A".into(),
                decimals: Some(18),
                wraps: None,
            }])
            .unwrap();
        assert_eq!(catalog.missing_tokens(10).unwrap(), vec![missing[1]]);
        assert_eq!(catalog.token_count().unwrap(), 2);
    }
}
