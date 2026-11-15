//! Copy identities into another catalog directory.

use alloy_primitives::{Address, B256};
use eyre::WrapErr;
use redb::ReadableDatabase;
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use store_model::{decode_row, entity_key, kind_name, Put, VenueRow, UNI_V3};

use super::identity::{count_identities, identity_keys, write_identities};
use super::{catalog_dir, Catalog, VENUES};
use crate::storage::Database;

/// How many identities a sample copied, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    pub copied: u64,
    pub by_kind: BTreeMap<u8, u64>,
    /// The USDC/WETH 0.3% pool was in the source and is in the copy.
    pub anchor: bool,
}

/// Copy a varied subset of identities from `src` into the catalog at `dest`.
///
/// `src` is opened as a plain redb file. The live `venues.redb` and the live
/// catalog directory are refused.
pub fn sample_identities(
    src: &Path,
    dest: &Path,
    target: u64,
    caches: &config::Store,
) -> eyre::Result<SampleReport> {
    if target == 0 {
        return Err(eyre::eyre!("sample target is 0"));
    }
    refuse_live_file(src)?;
    let dest_dir = catalog_dir(dest);
    std::fs::create_dir_all(&dest_dir)?;
    let src_dir = src.parent().unwrap_or_else(|| Path::new("."));
    if dirs_equal(src_dir, &dest_dir)? {
        return Err(eyre::eyre!(
            "sample source and destination are the same directory"
        ));
    }
    refuse_live_dir(&dest_dir)?;
    let source = Database::open(src, caches.entities).wrap_err("open identity source")?;
    let counts = count_identities(&source)?;
    let plan = allocate_sample(&counts, target);
    let catalog = Catalog::open_with(&dest_dir, caches)?;
    let anchor = anchor_id();
    let read = source.begin_read()?;
    let table = read.open_table(VENUES).wrap_err("identity table")?;
    let mut copied = 0_u64;
    let mut by_kind = BTreeMap::new();
    let mut found_anchor = false;
    for (kind, want) in plan {
        if want == 0 {
            continue;
        }
        let count = counts.get(&kind).copied().unwrap_or(0);
        let step = (count / want).max(1);
        observability::info!(kind = kind_name(kind), count, want, step, "sampling kind");
        let start_key = entity_key(kind, &B256::ZERO);
        let mut end_key = [0u8; 33];
        let end = if kind == u8::MAX {
            Bound::Unbounded
        } else {
            end_key[0] = kind + 1;
            Bound::Excluded(end_key.as_slice())
        };
        let mut picked = 0_u64;
        let mut wrote = 0_u64;
        let mut chunk = Vec::new();
        for (index, item) in table
            .range::<&[u8]>((Bound::Included(start_key.as_slice()), end))?
            .enumerate()
        {
            let index = index as u64;
            let (key_bytes, value) = item?;
            let key_bytes = key_bytes.value();
            if key_bytes.len() != 33 || key_bytes[0] != kind {
                return Err(eyre::eyre!("venue key"));
            }
            let id = B256::from_slice(&key_bytes[1..]);
            let is_anchor = kind == UNI_V3 && id == anchor;
            let on_stride = picked < want && index.is_multiple_of(step);
            if !on_stride && !is_anchor {
                continue;
            }
            if !is_anchor && picked >= want {
                continue;
            }
            let row = decode_row(kind, id, value.value())?;
            if is_anchor {
                found_anchor = true;
            }
            picked += 1;
            chunk.push(row);
            if chunk.len() == 512 {
                wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
                chunk.clear();
            }
            if picked >= want && (kind != UNI_V3 || found_anchor) {
                break;
            }
        }
        if !chunk.is_empty() {
            wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
        }
        if wrote > 0 {
            by_kind.insert(kind, wrote);
            copied += wrote;
        }
    }
    Ok(SampleReport {
        copied,
        by_kind,
        anchor: found_anchor,
    })
}

/// Identities `fill_identities` added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillReport {
    pub copied: u64,
    pub by_kind: BTreeMap<u8, u64>,
    pub ids: Vec<(u8, B256)>,
}

/// Copy `add` identities that are not already in `dest`.
///
/// `src` is opened as a plain redb file. Kinds that are already fully copied
/// contribute nothing. The live `venues.redb` is refused.
pub fn fill_identities(
    src: &Path,
    dest: &Path,
    add: u64,
    caches: &config::Store,
) -> eyre::Result<FillReport> {
    if add == 0 {
        return Err(eyre::eyre!("fill target is 0"));
    }
    refuse_live_file(src)?;
    let dest_dir = catalog_dir(dest);
    std::fs::create_dir_all(&dest_dir)?;
    let src_dir = src.parent().unwrap_or_else(|| Path::new("."));
    if dirs_equal(src_dir, &dest_dir)? {
        return Err(eyre::eyre!(
            "sample source and destination are the same directory"
        ));
    }
    refuse_live_dir(&dest_dir)?;
    let source = Database::open(src, caches.entities).wrap_err("open identity source")?;
    let source_counts = count_identities(&source)?;
    let catalog = Catalog::open_with(&dest_dir, caches)?;
    let mut present = identity_keys(&catalog)?;
    drop(catalog);
    remember_dropped(&dest_dir, &mut present)?;
    let mut remaining = BTreeMap::new();
    for (kind, count) in &source_counts {
        let have = present.iter().filter(|key| key[0] == *kind).count() as u64;
        let left = count.saturating_sub(have);
        if left > 0 {
            remaining.insert(*kind, left);
        }
    }
    let plan = allocate_sample(&remaining, add);
    let catalog = Catalog::open_with(&dest_dir, caches)?;
    let read = source.begin_read()?;
    let table = read.open_table(VENUES).wrap_err("identity table")?;
    let mut copied = 0_u64;
    let mut by_kind = BTreeMap::new();
    let mut ids = Vec::new();
    for (kind, want) in plan {
        if want == 0 {
            continue;
        }
        let count = remaining.get(&kind).copied().unwrap_or(0);
        let step = (count / want).max(1);
        observability::info!(kind = kind_name(kind), count, want, step, "filling kind");
        let start_key = entity_key(kind, &B256::ZERO);
        let mut end_key = [0u8; 33];
        let end = if kind == u8::MAX {
            Bound::Unbounded
        } else {
            end_key[0] = kind + 1;
            Bound::Excluded(end_key.as_slice())
        };
        let mut seen = 0_u64;
        let mut picked = 0_u64;
        let mut wrote = 0_u64;
        let mut chunk = Vec::new();
        for item in table.range::<&[u8]>((Bound::Included(start_key.as_slice()), end))? {
            let (key_bytes, value) = item?;
            let key_bytes = key_bytes.value();
            if key_bytes.len() != 33 || key_bytes[0] != kind {
                return Err(eyre::eyre!("venue key"));
            }
            let mut owned = [0u8; 33];
            owned.copy_from_slice(key_bytes);
            if present.contains(&owned) {
                continue;
            }
            let take = picked < want && seen.is_multiple_of(step);
            seen += 1;
            if !take {
                continue;
            }
            let id = B256::from_slice(&key_bytes[1..]);
            let row = decode_row(kind, id, value.value())?;
            picked += 1;
            chunk.push(row);
            ids.push((kind, id));
            if chunk.len() == 512 {
                wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
                chunk.clear();
            }
            if picked >= want {
                break;
            }
        }
        if !chunk.is_empty() {
            wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
        }
        if wrote > 0 {
            by_kind.insert(kind, wrote);
            copied += wrote;
        }
    }
    Ok(FillReport {
        copied,
        by_kind,
        ids,
    })
}

/// Take small kinds whole, then split whatever remains across the large ones.
pub(super) fn allocate_sample(counts: &BTreeMap<u8, u64>, target: u64) -> BTreeMap<u8, u64> {
    if target == 0 || counts.is_empty() {
        return BTreeMap::new();
    }
    let min_each = (target / counts.len() as u64 / 2).max(1);
    let mut ordered: Vec<(u8, u64)> = counts.iter().map(|(kind, count)| (*kind, *count)).collect();
    ordered.sort_by_key(|(_, count)| *count);
    let mut take = BTreeMap::new();
    let mut used = 0_u64;
    let mut large = Vec::new();
    for (index, (kind, count)) in ordered.iter().copied().enumerate() {
        let remaining = (ordered.len() - index - 1) as u64;
        let room_after = target.saturating_sub(used).saturating_sub(count);
        let need = min_each.saturating_mul(remaining);
        if count > 0 && room_after >= need && used.saturating_add(count) <= target {
            take.insert(kind, count);
            used += count;
        } else if count > 0 {
            large.push((kind, count));
        }
    }
    let mut left = target.saturating_sub(used);
    while left > 0 {
        let open: Vec<usize> = large
            .iter()
            .enumerate()
            .filter(|(_, (kind, count))| take.get(kind).copied().unwrap_or(0) < *count)
            .map(|(index, _)| index)
            .collect();
        if open.is_empty() {
            break;
        }
        let share = (left / open.len() as u64).max(1);
        let mut progressed = false;
        for index in open {
            if left == 0 {
                break;
            }
            let (kind, count) = large[index];
            let have = take.get(&kind).copied().unwrap_or(0);
            let add = share.min(count - have).min(left);
            if add > 0 {
                take.insert(kind, have + add);
                left -= add;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    take
}

fn remember_dropped(dir: &Path, present: &mut HashSet<[u8; 33]>) -> eyre::Result<()> {
    let path = dir.join("dropped-empty.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).wrap_err("read dropped empty ids"),
    };
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts
            .next()
            .ok_or_else(|| eyre::eyre!("dropped id"))?
            .parse::<u8>()
            .map_err(|err| eyre::eyre!("dropped kind: {err}"))?;
        let id = parts
            .next()
            .ok_or_else(|| eyre::eyre!("dropped id"))?
            .parse::<B256>()
            .map_err(|err| eyre::eyre!("dropped id: {err}"))?;
        present.insert(entity_key(kind, &id));
    }
    Ok(())
}

pub fn append_dropped_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    append_ids(dir, "dropped-empty.txt", ids)
}

pub fn append_fill_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    append_ids(dir, "fill-added.txt", ids)
}

fn append_ids(dir: &Path, name: &str, ids: &[(u8, B256)]) -> eyre::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .wrap_err("append ids")?;
    for (kind, id) in ids {
        use std::io::Write;
        writeln!(file, "{kind} {id}").wrap_err("write fill id")?;
    }
    Ok(())
}

pub fn read_fill_ids(dir: &Path) -> eyre::Result<Vec<(u8, B256)>> {
    let path = dir.join("fill-added.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).wrap_err("read fill ids"),
    };
    let mut ids = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts
            .next()
            .ok_or_else(|| eyre::eyre!("fill id"))?
            .parse::<u8>()
            .map_err(|err| eyre::eyre!("fill kind: {err}"))?;
        let id = parts
            .next()
            .ok_or_else(|| eyre::eyre!("fill id"))?
            .parse::<B256>()
            .map_err(|err| eyre::eyre!("fill id: {err}"))?;
        ids.push((kind, id));
    }
    Ok(ids)
}

pub fn write_fill_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    let mut text = String::new();
    for (kind, id) in ids {
        text.push_str(&format!("{kind} {id}\n"));
    }
    std::fs::write(dir.join("fill-added.txt"), text).wrap_err("rewrite fill ids")
}

pub(super) fn anchor_id() -> B256 {
    let address = Address::from_str("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8").expect("anchor");
    VenueRow::address_id(address)
}

fn live_share_dir() -> PathBuf {
    crate::storage::data_dir()
}

fn dirs_equal(left: &Path, right: &Path) -> eyre::Result<bool> {
    if !left.exists() || !right.exists() {
        return Ok(false);
    }
    Ok(std::fs::canonicalize(left)? == std::fs::canonicalize(right)?)
}

fn refuse_live_dir(dir: &Path) -> eyre::Result<()> {
    if dirs_equal(dir, &live_share_dir())? {
        return Err(eyre::eyre!("refusing to write the live catalog"));
    }
    Ok(())
}

fn refuse_live_file(path: &Path) -> eyre::Result<()> {
    let live = live_share_dir().join("venues.redb");
    if path.exists()
        && live.exists()
        && std::fs::canonicalize(path)? == std::fs::canonicalize(&live)?
    {
        return Err(eyre::eyre!("refusing to open the live venues.redb"));
    }
    Ok(())
}
