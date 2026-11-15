//! Log classification and one pass of the head watch.

use super::{
    concentrated_kind, VenueRow, WatchHit, CLASS_HEAD, CLASS_TICKS, TOPIC_BAL3_SWAP,
    TOPIC_BAL_BALANCE, TOPIC_BAL_SWAP, TOPIC_CURVE_EXCHANGE, TOPIC_PANCAKE_SWAP, TOPIC_V2_SWAP,
    TOPIC_V2_SYNC, TOPIC_V3_BURN, TOPIC_V3_MINT, TOPIC_V3_SWAP, TOPIC_V4_LIQUIDITY, TOPIC_V4_SWAP,
};
use crate::catalog::{AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, UNI_V4};
use crate::{ChainStore, VenueStore};
use alloy_primitives::{Address, B256};
use journal::Log;
use peer::HistoryPeer;
use store_model::BlockRecord;

/// Classify one log into a dirty hint. Factory creation logs are ignored.
/// The caller resolves identities. A singleton pool id comes from the event.
pub fn classify_log(
    log: &Log,
    mut has_id: impl FnMut(u8, B256) -> bool,
    mut by_address: impl FnMut(Address) -> Option<(u8, B256)>,
) -> Option<WatchHit> {
    let topic = log.topics.first().copied()?;
    let known = chain::decode::known();
    if log.address == known.pool_manager {
        return singleton_hit(
            log,
            topic,
            UNI_V4,
            TOPIC_V4_SWAP,
            TOPIC_V4_LIQUIDITY,
            CLASS_TICKS,
            &mut has_id,
        );
    }
    if log.address == known.bal_v2 {
        return singleton_hit(
            log,
            topic,
            BALANCER_V2,
            TOPIC_BAL_SWAP,
            TOPIC_BAL_BALANCE,
            CLASS_HEAD,
            &mut has_id,
        );
    }
    if log.address == known.bal_v3 && topic == TOPIC_BAL3_SWAP {
        let id = topic_id(log)?;
        if !has_id(BALANCER_V3, id) {
            return None;
        }
        return Some(WatchHit {
            kind: BALANCER_V3,
            id,
            block: log.block_number,
            class: CLASS_HEAD,
        });
    }
    let (kind, id) = by_address(log.address)?;
    let class = if matches!(topic, TOPIC_V3_MINT | TOPIC_V3_BURN | TOPIC_V4_LIQUIDITY) {
        CLASS_TICKS
    } else if matches!(
        topic,
        TOPIC_V2_SYNC
            | TOPIC_V2_SWAP
            | TOPIC_V3_SWAP
            | TOPIC_PANCAKE_SWAP
            | TOPIC_V4_SWAP
            | TOPIC_BAL_SWAP
            | TOPIC_BAL3_SWAP
            | TOPIC_BAL_BALANCE
            | TOPIC_CURVE_EXCHANGE
    ) {
        CLASS_HEAD
    } else if concentrated_kind(kind) || kind == METRIC {
        CLASS_TICKS
    } else {
        CLASS_HEAD
    };
    Some(WatchHit {
        kind,
        id,
        block: log.block_number,
        class,
    })
}

fn singleton_hit(
    log: &Log,
    topic: B256,
    kind: u8,
    head: B256,
    other: B256,
    other_class: u8,
    has_id: &mut impl FnMut(u8, B256) -> bool,
) -> Option<WatchHit> {
    if topic != head && topic != other {
        return None;
    }
    let id = topic_id(log)?;
    if !has_id(kind, id) {
        return None;
    }
    Some(WatchHit {
        kind,
        id,
        block: log.block_number,
        class: if topic == other {
            other_class
        } else {
            CLASS_HEAD
        },
    })
}

fn topic_id(log: &Log) -> Option<B256> {
    log.topics.get(1).copied()
}

pub(super) async fn watch_once<S: ChainStore + VenueStore, P: HistoryPeer>(
    store: &S,
    peer: &P,
) -> eyre::Result<Option<String>> {
    let head = store.header_cursor().await?;
    if head == 0 {
        return Ok(None);
    }
    if store.watch_through().await?.is_none() {
        store.apply_watch(head, &[]).await?;
        return Ok(Some(format!("watch armed at {head}")));
    }
    let from = store.watch_through().await?.unwrap_or(0);
    if head <= from {
        return Ok(None);
    }
    let count = (head - from).min(8);
    let blocks = peer.chunk(from, count).await?;
    if blocks.is_empty() {
        return Ok(None);
    }
    if !contiguous(&blocks, from) {
        return Err(eyre::eyre!("watch chunk did not start at {from}"));
    }
    let mut hits = Vec::new();
    for block in &blocks {
        let logs = journal::logs_in_block(block)?;
        for log in logs {
            if let Some(hit) = watch_hit(store, &log).await? {
                hits.push(hit);
            }
        }
    }
    let through = from + blocks.len() as u64;
    store.apply_watch(through, &hits).await?;
    Ok(Some(format!("watch {from}..{through} hits {}", hits.len())))
}

async fn watch_hit<S: VenueStore>(store: &S, log: &Log) -> eyre::Result<Option<WatchHit>> {
    let Some(&topic) = log.topics.first() else {
        return Ok(None);
    };
    let known = chain::decode::known();
    if log.address == known.pool_manager || log.address == known.bal_v2 {
        let (kind, head, other, _other_class) = if log.address == known.pool_manager {
            (UNI_V4, TOPIC_V4_SWAP, TOPIC_V4_LIQUIDITY, CLASS_TICKS)
        } else {
            (BALANCER_V2, TOPIC_BAL_SWAP, TOPIC_BAL_BALANCE, CLASS_HEAD)
        };
        if topic != head && topic != other {
            return Ok(None);
        }
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(kind, id).await?.is_some();
        return Ok(super::classify_log(
            log,
            |asked_kind, asked_id| asked_kind == kind && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 && topic == TOPIC_BAL3_SWAP {
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(BALANCER_V3, id).await?.is_some();
        return Ok(super::classify_log(
            log,
            |asked_kind, asked_id| asked_kind == BALANCER_V3 && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 {
        return Ok(None);
    }
    let Some((kind, id)) = find_address(store, log.address).await? else {
        return Ok(None);
    };
    Ok(super::classify_log(log, |_, _| false, |_| Some((kind, id))))
}

async fn find_address<S: VenueStore>(
    store: &S,
    address: Address,
) -> eyre::Result<Option<(u8, B256)>> {
    let id = VenueRow::address_id(address);
    for kind in 1..=METRIC {
        if !(concentrated_kind(kind)
            || matches!(kind, 1 | 2 | 3 | 4 | AERO | CURVE | METRIC | BALANCER_V3))
        {
            continue;
        }
        if store.read_venue(kind, id).await?.is_some() {
            return Ok(Some((kind, id)));
        }
    }
    Ok(None)
}

fn contiguous(blocks: &[BlockRecord], from: u64) -> bool {
    blocks
        .iter()
        .enumerate()
        .all(|(index, block)| block.number == from + index as u64)
}
