//! Append-only notes of conduit row changes.
//!
//! A spot pass keeps the decoded rows and applies this log. The note is written
//! in the same transaction as the conduit row. When the log passes
//! [`LOG_LIMIT`], further notes stop and the next pass reads the table.

use alloy_primitives::Address;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use routing::SlopeSegment;

use super::codec::decode_conduit;
use super::{pair_addresses, CONDUITS};

const LOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_log");

const ENTITY_META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const SEQ: &str = "conduit_log_seq";

const OVERFLOW: &str = "conduit_log_overflow";

pub(crate) const LOG_LIMIT: u64 = 65_536;

/// One pair whose stored conduit changed at or after a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitNote {
    pub token_in: Address,
    pub token_out: Address,
    pub block: Option<u64>,
    pub segments: Vec<SlopeSegment>,
}

/// Exclusive end of the log, and whether a note was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConduitTip {
    pub next: u64,
    pub overflow: bool,
}

pub(super) fn note_conduit(
    meta: &mut redb::Table<'_, &str, u64>,
    log: &mut redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
    present: bool,
) -> eyre::Result<()> {
    note_limited(meta, log, pair, present, LOG_LIMIT)
}

pub(super) fn note_limited(
    meta: &mut redb::Table<'_, &str, u64>,
    log: &mut redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
    present: bool,
    limit: u64,
) -> eyre::Result<()> {
    if meta.get(OVERFLOW)?.is_some() {
        return Ok(());
    }
    if log.len()? >= limit {
        meta.insert(OVERFLOW, 1)?;
        return Ok(());
    }
    let seq = meta.get(SEQ)?.map(|value| value.value()).unwrap_or(0);
    let mut bytes = [0_u8; 41];
    bytes[0] = u8::from(present);
    bytes[1..].copy_from_slice(pair);
    log.insert(seq.to_be_bytes().as_slice(), bytes.as_slice())?;
    meta.insert(SEQ, seq.saturating_add(1))?;
    Ok(())
}

pub(super) fn open_log<'txn>(
    write: &'txn redb::WriteTransaction,
) -> eyre::Result<(
    redb::Table<'txn, &'static str, u64>,
    redb::Table<'txn, &'static [u8], &'static [u8]>,
)> {
    Ok((write.open_table(ENTITY_META)?, write.open_table(LOG)?))
}

impl super::super::catalog::Catalog {
    pub fn conduit_log_tip(&self) -> eyre::Result<ConduitTip> {
        let read = self.entities.begin_read()?;
        read_tip(&read)
    }

    /// Notes with sequence `after` or later. The tip is the next sequence.
    ///
    /// A pair named more than once keeps its latest note. An upsert reads the
    /// stored row in this snapshot.
    pub fn conduit_log_since(&self, after: u64) -> eyre::Result<(Vec<ConduitNote>, ConduitTip)> {
        let read = self.entities.begin_read()?;
        let tip = read_tip(&read)?;
        let log = match read.open_table(LOG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), tip)),
            Err(err) => return Err(err.into()),
        };
        let conduits = match read.open_table(CONDUITS) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(err.into()),
        };
        let start = after.to_be_bytes();
        let mut last: std::collections::BTreeMap<[u8; 40], bool> =
            std::collections::BTreeMap::new();
        for item in log.range(start.as_slice()..)? {
            let (_, value) = item?;
            let bytes = value.value();
            if bytes.len() != 41 {
                continue;
            }
            let mut pair = [0_u8; 40];
            pair.copy_from_slice(&bytes[1..]);
            last.insert(pair, bytes[0] == 1);
        }
        let mut notes = Vec::with_capacity(last.len());
        for (pair, present) in last {
            let (token_in, token_out) = pair_addresses(&pair);
            let stored = if present {
                conduit_at(conduits.as_ref(), &pair)?
            } else {
                None
            };
            let (block, segments) = match stored {
                Some(stored) => (Some(stored.block), stored.segments),
                None => (None, Vec::new()),
            };
            notes.push(ConduitNote {
                token_in,
                token_out,
                block,
                segments,
            });
        }
        Ok((notes, tip))
    }

    pub fn clear_conduit_log(&self) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(redb::Durability::Immediate)?;
        {
            let mut meta = write.open_table(ENTITY_META)?;
            let mut log = write.open_table(LOG)?;
            let mut keys = Vec::new();
            for item in log.iter()? {
                let (key, _) = item?;
                keys.push(key.value().to_vec());
            }
            for key in keys {
                log.remove(key.as_slice())?;
            }
            meta.remove(SEQ)?;
            meta.remove(OVERFLOW)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Drop notes whose sequence is below `before`.
    pub fn trim_conduit_log(&self, before: u64) -> eyre::Result<()> {
        if before == 0 {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(redb::Durability::Immediate)?;
        {
            let mut log = write.open_table(LOG)?;
            let end = before.to_be_bytes();
            let mut keys = Vec::new();
            for item in log.range(..end.as_slice())? {
                let (key, _) = item?;
                keys.push(key.value().to_vec());
            }
            for key in keys {
                log.remove(key.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn read_tip(read: &redb::ReadTransaction) -> eyre::Result<ConduitTip> {
    let meta = match read.open_table(ENTITY_META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(ConduitTip {
                next: 0,
                overflow: false,
            });
        }
        Err(err) => return Err(err.into()),
    };
    Ok(ConduitTip {
        next: meta.get(SEQ)?.map(|value| value.value()).unwrap_or(0),
        overflow: meta.get(OVERFLOW)?.is_some(),
    })
}

fn conduit_at(
    table: Option<&redb::ReadOnlyTable<&'static [u8], &'static [u8]>>,
    pair: &[u8; 40],
) -> eyre::Result<Option<super::StoredConduit>> {
    let Some(table) = table else {
        return Ok(None);
    };
    match table.get(pair.as_slice())? {
        Some(value) => Ok(Some(decode_conduit(value.value())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::catalog::Catalog;
    use alloy_primitives::address;
    use num_bigint::BigInt;
    use routing::{Action, SlopeSegment};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-conduit-log-{}-{}-{}",
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

    fn segment() -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(2),
            output: BigInt::from(1),
            action: Action {
                kind: 1,
                pool: [9; 32],
            },
        }
    }

    #[test]
    fn a_conduit_log_keeps_the_latest_note_until_it_is_trimmed() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let token_in = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token_out = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        catalog
            .insert_conduit_without_meta_for_test(token_in, token_out, &[segment()])
            .unwrap();
        catalog
            .insert_conduit_without_meta_for_test(token_in, token_out, &[segment()])
            .unwrap();
        let (notes, tip) = catalog.conduit_log_since(0).unwrap();
        assert_eq!(tip.next, 2);
        assert!(!tip.overflow);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].token_in, token_in);
        assert_eq!(notes[0].block, Some(1));
        assert_eq!(notes[0].segments, vec![segment()]);
        catalog.trim_conduit_log(tip.next).unwrap();
        let (notes, tip) = catalog.conduit_log_since(0).unwrap();
        assert!(notes.is_empty());
        assert_eq!(tip.next, 2);
    }

    #[test]
    fn a_conduit_log_stops_when_a_note_would_pass_the_limit() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let pair = [7_u8; 40];
        let write = catalog.entities.begin_write().unwrap();
        {
            let (mut meta, mut log) = super::open_log(&write).unwrap();
            super::note_limited(&mut meta, &mut log, &pair, true, 1).unwrap();
            super::note_limited(&mut meta, &mut log, &pair, false, 1).unwrap();
        }
        write.commit().unwrap();
        let (notes, tip) = catalog.conduit_log_since(0).unwrap();
        assert!(tip.overflow);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].block.is_none());
        catalog.clear_conduit_log().unwrap();
        let tip = catalog.conduit_log_tip().unwrap();
        assert_eq!(tip.next, 0);
        assert!(!tip.overflow);
    }
}
