//! Stored price curves.

use alloy_primitives::B256;
use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::{decode_curve_record, decode_mutable, encode_curve, entity_key, CurveRecord};

use super::super::catalog::Catalog;
use super::*;

impl Catalog {
    pub fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<Option<CurveRecord>> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let previous = {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut curves = write.open_table(CURVES)?;
            let key = entity_key(curve.kind, &curve.id);
            let Some(stored) = mutable_table.get(key.as_slice())? else {
                return Err(eyre::eyre!("curve has no mutable row"));
            };
            let view = decode_mutable(stored.value())?;
            if view.generation != curve.generation {
                return Err(eyre::eyre!("curve generation"));
            }
            let previous = match curves.get(key.as_slice())? {
                Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                None => None,
            };
            let bytes = encode_curve(curve);
            curves.insert(key.as_slice(), bytes.as_slice())?;
            let mut pending = write.open_table(PENDING_MEMBERS)?;
            stage_pending(&mut pending, &key, previous.as_ref())?;
            previous
        };
        write.commit()?;
        Ok(previous)
    }

    /// Store a page of curves in one commit. A generation mismatch stores none of them.
    pub fn put_curves(&self, curves: &[CurveRecord]) -> eyre::Result<Vec<Option<CurveRecord>>> {
        if curves.is_empty() {
            return Ok(Vec::new());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let mut replaced = Vec::with_capacity(curves.len());
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut table = write.open_table(CURVES)?;
            let mut pending = write.open_table(PENDING_MEMBERS)?;
            for curve in curves {
                let key = entity_key(curve.kind, &curve.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("curve has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != curve.generation {
                    return Err(eyre::eyre!("curve generation"));
                }
                let previous = match table.get(key.as_slice())? {
                    Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                    None => None,
                };
                let bytes = encode_curve(curve);
                table.insert(key.as_slice(), bytes.as_slice())?;
                stage_pending(&mut pending, &key, previous.as_ref())?;
                replaced.push(previous);
            }
        }
        write.commit()?;
        Ok(replaced)
    }

    pub fn read_curve(&self, kind: u8, id: B256) -> eyre::Result<Option<CurveRecord>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(CURVES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let key = entity_key(kind, &id);
        match table.get(key.as_slice())? {
            Some(value) => Ok(decode_curve_record(kind, id, value.value()).ok()),
            None => Ok(None),
        }
    }

    pub fn member_fill_done(&self) -> eyre::Result<bool> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(MEMBER_FILL)?.is_some())
    }

    /// Curves written since the last full member fill whose retarget did not commit.
    pub fn pending_member_curves(&self) -> eyre::Result<Vec<PendingMember>> {
        let read = self.entities.begin_read()?;
        let pending = match read.open_table(PENDING_MEMBERS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let curves = match read.open_table(CURVES) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(err.into()),
        };
        let mut out = Vec::new();
        for item in pending.iter()? {
            let (key, value) = item?;
            let key = key.value();
            if key.len() != 33 {
                continue;
            }
            let kind = key[0];
            let id = B256::from_slice(&key[1..]);
            let previous = if value.value().is_empty() {
                None
            } else {
                decode_curve_record(kind, id, value.value()).ok()
            };
            let current = match &curves {
                Some(table) => match table.get(key)? {
                    Some(stored) => decode_curve_record(kind, id, stored.value()).ok(),
                    None => None,
                },
                None => None,
            };
            out.push(PendingMember {
                kind,
                id,
                previous,
                current,
            });
        }
        Ok(out)
    }

    pub fn mark_member_fill_done(&self) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut meta = write.open_table(META)?;
            let mut pending = write.open_table(PENDING_MEMBERS)?;
            meta.insert(MEMBER_FILL, 1)?;
            let mut keys = Vec::new();
            for item in pending.iter()? {
                let (key, _) = item?;
                keys.push(key.value().to_vec());
            }
            for key in keys {
                pending.remove(key.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

/// A curve whose member rows still need the retarget that follows its write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMember {
    pub kind: u8,
    pub id: B256,
    pub previous: Option<CurveRecord>,
    pub current: Option<CurveRecord>,
}

pub(super) fn stage_pending(
    pending: &mut redb::Table<'_, &[u8], &[u8]>,
    key: &[u8],
    previous: Option<&CurveRecord>,
) -> eyre::Result<()> {
    let bytes = match previous {
        Some(curve) => encode_curve(curve),
        None => Vec::new(),
    };
    pending.insert(key, bytes.as_slice())?;
    Ok(())
}

pub(crate) fn clear_pending(
    pending: &mut redb::Table<'_, &[u8], &[u8]>,
    kind: u8,
    id: &B256,
) -> eyre::Result<()> {
    let key = entity_key(kind, id);
    pending.remove(key.as_slice())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::catalog::Catalog;
    use alloy_primitives::B256;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store_model::{CurveRecord, MutableWrite};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-member-fill-{}-{}-{}",
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

    fn mutable(id: B256) -> MutableWrite {
        MutableWrite {
            kind: 1,
            id,
            block: 1,
            class: 0,
            bitmap: B256::ZERO,
            tick_count: 0,
            truncated: false,
            next_scan: None,
            body: Vec::new(),
        }
    }

    fn curve(id: B256, payload: Vec<u8>) -> CurveRecord {
        CurveRecord {
            kind: 1,
            id,
            generation: 1,
            version: 3,
            payload,
        }
    }

    #[test]
    fn a_curve_stays_pending_until_its_retarget_commits() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let id = B256::from([1_u8; 32]);
        catalog.put_mutable(&mutable(id)).unwrap();
        catalog.put_curve(&curve(id, vec![1])).unwrap();
        let pending = catalog.pending_member_curves().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].previous.is_none());
        assert_eq!(
            pending[0].current.as_ref().map(|row| row.payload.clone()),
            Some(vec![1])
        );
        catalog.apply_retarget(1, id, &[], &[], None).unwrap();
        assert!(catalog.pending_member_curves().unwrap().is_empty());
        catalog.put_curve(&curve(id, vec![2])).unwrap();
        catalog.put_mutable(&mutable(id)).unwrap();
        let pending = catalog.pending_member_curves().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].current.is_none());
        assert_eq!(
            pending[0].previous.as_ref().map(|row| row.payload.clone()),
            Some(vec![2])
        );
        catalog.mark_member_fill_done().unwrap();
        assert!(catalog.member_fill_done().unwrap());
        assert!(catalog.pending_member_curves().unwrap().is_empty());
    }
}
