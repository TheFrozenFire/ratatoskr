//! On-chain index walks.
//!
//! One lens `eth_call` reads a slice of `allPairs`, `allPools`, `pool_list`,
//! or `idxToPool`. The cursor is the next index. A chunk commits the rows and
//! the cursor together.

use alloy_primitives::{Address, U256};
use eyre::eyre;

use crate::catalog::{Put, VenueRow};
use crate::lens::{self, LensHit, CALL_GAS};
use crate::rpc::Rpc;
use crate::EntityStore;

const ALL_PAIRS_LEN: [u8; 4] = [0x57, 0x4f, 0x2b, 0xa3];
const ALL_PAIRS: [u8; 4] = [0x1e, 0x3d, 0xd1, 0x8b];
const POOL_COUNT: [u8; 4] = [0x95, 0x6a, 0xae, 0x3a];
const POOL_LIST: [u8; 4] = [0x3a, 0x1d, 0x5d, 0x8e];
const NEXT_POOL_IDX: [u8; 4] = [0x81, 0x0d, 0x36, 0xd9];
const IDX_TO_POOL: [u8; 4] = [0x15, 0x8b, 0x67, 0xfa];
const ALL_POOLS_LEN: [u8; 4] = [0xef, 0xde, 0x4e, 0x64];
const ALL_POOLS: [u8; 4] = [0x41, 0xd1, 0xde, 0x97];

#[derive(Clone, Copy)]
enum Length {
    /// `view` with no arguments. Indexes are `0..result`.
    Selector([u8; 4]),
    /// Metric `nextPoolIdx()`. Indexes are `1..result` (the value itself is empty).
    NextPoolIdx,
}

#[derive(Clone)]
pub struct ListJob {
    pub name: String,
    pub factory: Address,
    pub kind: u8,
    length: Length,
    index_sel: [u8; 4],
    mode: u8,
    origin: u64,
    batch: u64,
    put: Put,
}

struct View {
    length: Length,
    index_sel: [u8; 4],
    mode: u8,
    origin: u64,
    batch: u64,
    probe: [u8; 4],
}

const VIEWS: &[View] = &[
    View {
        length: Length::Selector(ALL_PAIRS_LEN),
        index_sel: ALL_PAIRS,
        mode: 1,
        origin: 0,
        batch: 800,
        probe: ALL_PAIRS_LEN,
    },
    View {
        length: Length::Selector(ALL_POOLS_LEN),
        index_sel: ALL_POOLS,
        mode: 2,
        origin: 0,
        batch: 200,
        probe: ALL_POOLS_LEN,
    },
    View {
        length: Length::NextPoolIdx,
        index_sel: IDX_TO_POOL,
        mode: 0,
        origin: 1,
        batch: 190,
        probe: NEXT_POOL_IDX,
    },
    View {
        length: Length::Selector(POOL_COUNT),
        index_sel: POOL_LIST,
        mode: 3,
        origin: 0,
        batch: 40,
        probe: POOL_COUNT,
    },
];

pub(crate) fn view_count() -> usize {
    VIEWS.len()
}

pub(crate) fn view_probe(index: usize) -> [u8; 4] {
    VIEWS[index].probe
}

pub(crate) fn job_from(name: &str, factory: Address, kind: u8, view: usize, put: Put) -> ListJob {
    let view = &VIEWS[view];
    ListJob {
        name: name.into(),
        factory,
        kind,
        length: view.length,
        index_sel: view.index_sel,
        mode: view.mode,
        origin: view.origin,
        batch: view.batch,
        put,
    }
}

pub async fn walk<S: EntityStore>(
    rpc: &Rpc,
    store: &S,
    job: &ListJob,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    let mut at = store
        .cursor(&job.name)
        .await?
        .unwrap_or(job.origin)
        .max(job.origin);
    let mut wrote = 0_u64;
    loop {
        if crate::hold_for_pause(stop).await {
            return Ok(wrote);
        }
        let rpc_len = rpc.clone();
        let job_len = job.clone();
        let end = tokio::task::spawn_blocking(move || length(&rpc_len, &job_len))
            .await
            .map_err(|err| eyre!("list length: {err}"))??;
        if at >= end {
            store.commit_chunk(&job.name, at, &[], job.put).await?;
            tracing::info!(job = %job.name, at, end, "list done");
            return Ok(wrote);
        }
        let mut count = (end - at).min(job.batch).max(1);
        loop {
            let rpc_fetch = rpc.clone();
            let job_fetch = job.clone();
            let fetched =
                match tokio::task::spawn_blocking(move || fetch(&rpc_fetch, &job_fetch, at, count))
                    .await
                {
                    Ok(fetched) => fetched,
                    Err(err) => Err(Fetch::Fatal(eyre!("list fetch: {err}"))),
                };
            match fetched {
                Ok(hits) => {
                    let rows = hits_to_rows(job, hits);
                    let next = at + count;
                    let n = store.commit_chunk(&job.name, next, &rows, job.put).await?;
                    wrote += n;
                    tracing::info!(job = %job.name, at, next, end, rows = rows.len(), "lens");
                    at = next;
                    break;
                }
                Err(Fetch::Shrink(reason)) if count > 1 => {
                    let smaller = (count / 2).max(1);
                    tracing::warn!(job = %job.name, at, count, smaller, %reason, "lens shrink");
                    count = smaller;
                }
                Err(Fetch::Shrink(reason)) => {
                    return Err(eyre!("lens {} at {at}: {reason}", job.name));
                }
                Err(Fetch::Fatal(err)) => return Err(err.wrap_err(job.name.clone())),
            }
        }
    }
}

enum Fetch {
    Shrink(String),
    Fatal(eyre::Report),
}

fn fetch(rpc: &Rpc, job: &ListJob, at: u64, count: u64) -> Result<Vec<LensHit>, Fetch> {
    let data = lens::call_data(job.factory, job.index_sel, at, count, job.mode);
    let call = rpc.eth_call(&data, None, CALL_GAS).map_err(Fetch::Fatal)?;
    let rows = match lens::decode_rows(&call.bytes, job.mode) {
        Ok(rows) => rows,
        Err(reason) => {
            return Err(Fetch::Shrink(format!("{reason}; {}", call.message)));
        }
    };
    if !rows.is_empty() && rows.iter().all(|row| row.pool == Address::ZERO) {
        let live = direct_pool(rpc, job, at).map_err(Fetch::Fatal)?;
        if live != Address::ZERO {
            if count > 1 {
                return Err(Fetch::Shrink(format!(
                    "lens returned zeros; index {at} is {live}"
                )));
            }
            return Ok(vec![LensHit {
                pool: live,
                tokens: Vec::new(),
                fee: None,
                tick_spacing: None,
            }]);
        }
    }
    Ok(rows)
}

fn direct_pool(rpc: &Rpc, job: &ListJob, index: u64) -> eyre::Result<Address> {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&job.index_sel);
    let mut arg = [0u8; 32];
    arg[24..].copy_from_slice(&index.to_be_bytes());
    data.extend_from_slice(&arg);
    let word = rpc.call_word(job.factory, &data)?;
    Ok(lens::address_from_u256(word))
}

fn hits_to_rows(job: &ListJob, hits: Vec<LensHit>) -> Vec<VenueRow> {
    hits.into_iter()
        .filter(|hit| hit.pool != Address::ZERO)
        .map(|hit| VenueRow {
            kind: job.kind,
            id: VenueRow::address_id(hit.pool),
            factory: job.factory,
            tokens: hit.tokens,
            fee: hit.fee,
            tick_spacing: hit.tick_spacing,
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        })
        .collect()
}

fn length(rpc: &Rpc, job: &ListJob) -> eyre::Result<u64> {
    let selector = match job.length {
        Length::Selector(selector) => selector,
        Length::NextPoolIdx => NEXT_POOL_IDX,
    };
    let word = rpc.call_word(job.factory, &selector)?;
    u64_word(word).ok_or_else(|| eyre!("{} length does not fit u64", job.name))
}

fn u64_word(value: U256) -> Option<u64> {
    let bytes = value.to_be_bytes::<32>();
    if bytes[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u64::from_be_bytes(bytes[24..32].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CURVE, METRIC};

    #[test]
    fn a_registry_does_not_overwrite_and_metric_starts_at_one() {
        let pool = (0..view_count())
            .find(|index| view_probe(*index) == POOL_COUNT)
            .unwrap();
        let ng = job_from("list:curve-ng", Address::ZERO, CURVE, pool, Put::Merge);
        assert!(matches!(ng.put, Put::Merge));
        assert_eq!(ng.mode, 3);
        let registry = job_from("list:curve-id-0", Address::ZERO, CURVE, pool, Put::IfAbsent);
        assert!(matches!(registry.put, Put::IfAbsent));
        let metric_view = (0..view_count())
            .find(|index| view_probe(*index) == NEXT_POOL_IDX)
            .unwrap();
        let metric = job_from(
            "list:metric",
            Address::ZERO,
            METRIC,
            metric_view,
            Put::Merge,
        );
        assert_eq!(metric.origin, 1);
        assert_eq!(metric.mode, 0);
    }
}
