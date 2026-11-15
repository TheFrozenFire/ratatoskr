//! Identity removal.

use alloy_primitives::B256;
use redb::Durability;
use store_model::{decode_curve_record, entity_key, METRIC, STAGE_IMMUTABLE, STAGE_QUOTE};

use super::super::catalog::Catalog;
use super::super::catalog::VENUES;
use super::work::clear_job;
use super::*;

impl Catalog {
    /// Delete identities whose stored snapshot cannot price any positive input.
    ///
    /// Pools with no snapshot yet stay, so a fill can be ingested afterwards.
    /// A one-wei reserve stays. Zero reserves and an empty book do not.
    pub fn drop_empty_pools(&self) -> eyre::Result<Vec<(u8, B256)>> {
        let mut drop = Vec::new();
        for kind in 1..=METRIC {
            let mut after = None;
            loop {
                let page = self.page_venues(kind, after, 64)?;
                if page.is_empty() {
                    break;
                }
                after = Some(page.last().expect("page").id);
                for row in &page {
                    let state = self.read_state(row.kind, row.id)?;
                    if !super::super::hooks::has_trade_size(row.kind, row.id, state)? {
                        drop.push((row.kind, row.id));
                    }
                }
                if page.len() < 64 {
                    break;
                }
            }
        }
        self.delete_pools(&drop)?;
        Ok(drop)
    }

    /// Delete a stride of `listed` identities until `target` rows remain.
    ///
    /// Identities that are not in `listed` stay. The returned list is `listed`
    /// without the rows that were removed.
    pub fn trim_listed(
        &self,
        target: u64,
        listed: &[(u8, B256)],
    ) -> eyre::Result<(u64, Vec<(u8, B256)>)> {
        let have = self.state_summary()?.identities;
        if have <= target {
            return Ok((0, listed.to_vec()));
        }
        let excess = (have - target) as usize;
        let mut present = Vec::new();
        for (kind, id) in listed {
            if self.read_venue(*kind, *id)?.is_some() {
                present.push((*kind, *id));
            }
        }
        let remove = stride_pick(&present, excess.min(present.len()));
        let removed = self.delete_pools(&remove)?;
        let gone: std::collections::HashSet<(u8, B256)> = remove.into_iter().collect();
        let left = listed
            .iter()
            .copied()
            .filter(|id| !gone.contains(id))
            .collect();
        Ok((removed, left))
    }

    /// Remove identities and every state row that hangs off them.
    pub fn delete_pools(&self, ids: &[(u8, B256)]) -> eyre::Result<u64> {
        let mut removed = 0_u64;
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
            for curve in &dropped_curves {
                super::super::conduit::retarget(self, curve.kind, curve.id, Some(curve), None)?;
            }
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
        Ok(removed)
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
