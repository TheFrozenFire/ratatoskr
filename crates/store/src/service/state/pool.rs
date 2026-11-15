//! Identity removal.

use alloy_primitives::B256;
use redb::Durability;
use store_model::{decode_curve_record, entity_key, STAGE_IMMUTABLE, STAGE_QUOTE};

use super::super::catalog::Catalog;
use super::super::catalog::VENUES;
use super::work::clear_job;
use super::*;

impl Catalog {
    /// Delete a stride of `listed` identities until `target` rows remain.
    ///
    /// Identities that are not in `listed` stay. The returned list is `listed`
    /// without the rows that were removed.
    pub fn trim_listed(
        &self,
        target: u64,
        listed: &[(u8, B256)],
    ) -> eyre::Result<(u64, Vec<(u8, B256)>, Vec<store_model::CurveRecord>)> {
        let have = self.state_summary()?.identities;
        if have <= target {
            return Ok((0, listed.to_vec(), Vec::new()));
        }
        let excess = (have - target) as usize;
        let mut present = Vec::new();
        for (kind, id) in listed {
            if self.read_venue(*kind, *id)?.is_some() {
                present.push((*kind, *id));
            }
        }
        let remove = stride_pick(&present, excess.min(present.len()));
        let (removed, curves) = self.delete_pools(&remove)?;
        let gone: std::collections::HashSet<(u8, B256)> = remove.into_iter().collect();
        let left = listed
            .iter()
            .copied()
            .filter(|id| !gone.contains(id))
            .collect();
        Ok((removed, left, curves))
    }

    /// Remove identities, their quote rows, and work from the immutable stage through quote.
    ///
    /// The returned curves were stored on removed identities. The caller drops those conduit members.
    pub fn delete_pools(
        &self,
        ids: &[(u8, B256)],
    ) -> eyre::Result<(u64, Vec<store_model::CurveRecord>)> {
        let mut removed = 0_u64;
        let mut dropped = Vec::new();
        for chunk in ids.chunks(128) {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            let mut dropped_curves = Vec::new();
            {
                let mut venues = write.open_table(VENUES)?;
                let mut immutable = write.open_table(IMMUTABLE)?;
                let mut mutable = write.open_table(MUTABLE)?;
                let mut ticks = write.open_table(TICK_PAGES)?;
                let mut curves = write.open_table(CURVES)?;
                let mut quotes = write.open_table(QUOTES)?;
                let mut dirty = write.open_table(DIRTY)?;
                let mut mismatch = write.open_table(MISMATCH)?;
                for (kind, id) in chunk {
                    let key = entity_key(*kind, id);
                    if venues.remove(key.as_slice())?.is_some() {
                        removed += 1;
                    }
                    if let Some(value) = curves.get(key.as_slice())? {
                        if let Ok(curve) = decode_curve_record(*kind, *id, value.value()) {
                            dropped_curves.push(curve);
                        }
                    }
                    immutable.remove(key.as_slice())?;
                    mutable.remove(key.as_slice())?;
                    curves.remove(key.as_slice())?;
                    quotes.remove(key.as_slice())?;
                    dirty.remove(key.as_slice())?;
                    mismatch.remove(key.as_slice())?;
                    delete_prefix(&mut ticks, &key)?;
                }
            }
            write.commit()?;
            dropped.extend(dropped_curves);
            let mut work = self.work.begin_write()?;
            work.set_durability(Durability::Immediate)?;
            {
                let mut jobs = work.open_table(JOBS)?;
                let mut ranks = work.open_table(JOB_RANK)?;
                for (kind, id) in chunk {
                    for stage in STAGE_IMMUTABLE..=STAGE_QUOTE {
                        clear_job(&mut jobs, &mut ranks, stage, *kind, id)?;
                    }
                }
            }
            work.commit()?;
        }
        Ok((removed, dropped))
    }
}

fn stride_pick(ids: &[(u8, B256)], remove: usize) -> Vec<(u8, B256)> {
    if remove == 0 || ids.is_empty() {
        return Vec::new();
    }
    if remove >= ids.len() {
        return ids.to_vec();
    }
    let mut out = Vec::with_capacity(remove);
    let mut acc = 0_usize;
    for id in ids {
        acc += remove;
        if acc >= ids.len() {
            acc -= ids.len();
            out.push(*id);
        }
    }
    out
}
