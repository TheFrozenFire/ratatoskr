//! Pool-state tables in `entities.redb` and the work queue in `work.redb`.

mod curve;
mod pool;
mod snapshot;
mod summary;
mod watch;
mod work;

pub(crate) use work::unix_now;

use alloy_primitives::B256;
use redb::{ReadableTable, TableDefinition};
use store_model::entity_key;

const IMMUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable");

const MUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutable");

const TICK_PAGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tick_pages");

const CURVES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("curves");

const QUOTES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("quotes");

const DIRTY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("dirty");

const MISMATCH: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable_mismatch");

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jobs");

/// Claim order for one stage. The identity key stays on `JOBS`.
const JOB_RANK: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_rank");

const WATCH_KEY: &str = "watch_through";

fn tick_key(kind: u8, id: B256, page: u32) -> [u8; 37] {
    let mut out = [0u8; 37];
    out[..33].copy_from_slice(&entity_key(kind, &id));
    out[33..].copy_from_slice(&page.to_be_bytes());
    out
}

fn delete_prefix(table: &mut redb::Table<'_, &[u8], &[u8]>, prefix: &[u8; 33]) -> eyre::Result<()> {
    let mut start = [0u8; 37];
    start[..33].copy_from_slice(prefix);
    let mut end = start;
    end[33..].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut keys = Vec::new();
    for item in table.range::<&[u8]>(start.as_slice()..=end.as_slice())? {
        let (key, _) = item?;
        keys.push(key.value().to_vec());
    }
    for key in keys {
        table.remove(key.as_slice())?;
    }
    Ok(())
}
