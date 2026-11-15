//! Tick bitmap words and the storage slots those ticks occupy.

use super::types::*;
use crate::rpc::{Rpc, StorageError};
use crate::tick::{self, PoolTicks};
use alloy_primitives::{keccak256, Address, B256, I256, U256};
use eyre::eyre;
use store_model::{AERO, PANCAKE_V3, UNI_V4};

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn attach_ticks(
    rpc: &Rpc,
    details: &mut [PoolDetail],
    block: Option<u64>,
) -> eyre::Result<()> {
    let mut indexes = Vec::new();
    let mut requests = Vec::new();
    let mut reads = Vec::new();
    for (index, detail) in details.iter().enumerate() {
        let PoolDetail::Concentrated(pool) = detail else {
            continue;
        };
        let ticks = pool.initialized_ticks();
        if ticks.is_empty() {
            continue;
        }
        indexes.push(index);
        let slot_base = tick_slot_base(pool);
        for tick in &ticks {
            let (account, slot) = tick_storage(pool, *tick);
            debug_assert_eq!(account, pool.home);
            debug_assert_eq!(slot, mapping_slot(i256_from_i32(*tick), slot_base));
            reads.push((account, slot));
        }
        requests.push(PoolTicks {
            kind: pool.kind,
            id: pool.id,
            home: pool.home,
            slot_base,
            ticks,
        });
    }
    if requests.is_empty() {
        return Ok(());
    }
    match rpc.storage_words_at(&reads, block) {
        Ok(words) => {
            apply_storage_words(details, &indexes, &requests, &words)?;
            return Ok(());
        }
        Err(StorageError::Unsupported) => {}
        Err(StorageError::Failed(err)) => return Err(err),
    }
    let loaded = tick::load_ticks(rpc, &requests, block)?;
    for ((index, request), (words, failed)) in indexes
        .into_iter()
        .zip(&requests)
        .zip(loaded.ticks.into_iter().zip(loaded.failed))
    {
        if words.len() != request.ticks.len() {
            return Err(eyre!("tick lens dropped words"));
        }
        let mut ticks = words
            .into_iter()
            .map(|read| {
                let (liquidity_gross, liquidity_net) = unpack_tick(read.word);
                TickWord {
                    tick: read.tick,
                    liquidity_gross,
                    liquidity_net,
                }
            })
            .collect::<Vec<_>>();
        ticks.sort_by_key(|tick| tick.tick);
        if let PoolDetail::Concentrated(pool) = &mut details[index] {
            if failed {
                pool.flags |= FLAG_FAILED;
            }
            pool.ticks = ticks;
        }
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn apply_storage_words(
    details: &mut [PoolDetail],
    indexes: &[usize],
    requests: &[PoolTicks],
    words: &[U256],
) -> eyre::Result<()> {
    let mut cursor = 0;
    for (index, request) in indexes.iter().zip(requests) {
        let mut ticks = Vec::with_capacity(request.ticks.len());
        for tick in &request.ticks {
            let word = words
                .get(cursor)
                .copied()
                .ok_or_else(|| eyre!("storage values short"))?;
            cursor += 1;
            let (liquidity_gross, liquidity_net) = unpack_tick(word);
            ticks.push(TickWord {
                tick: *tick,
                liquidity_gross,
                liquidity_net,
            });
        }
        ticks.sort_by_key(|tick| tick.tick);
        if let PoolDetail::Concentrated(pool) = &mut details[*index] {
            pool.ticks = ticks;
        }
    }
    if cursor != words.len() {
        return Err(eyre!("storage values long"));
    }
    Ok(())
}

impl Concentrated {
    pub fn initialized_ticks(&self) -> Vec<i32> {
        let mut ticks = Vec::new();
        for map in &self.bitmaps {
            for bit in 0..256 {
                if !map.bits.bit(bit) {
                    continue;
                }
                let compressed = (i32::from(map.word_pos) << 8) + bit as i32;
                let Some(tick) = compressed.checked_mul(self.tick_spacing) else {
                    continue;
                };
                ticks.push(tick);
            }
        }
        ticks.sort_unstable();
        ticks.dedup();
        ticks
    }
}

pub fn tick_request(pool: &Concentrated, ticks: Vec<i32>) -> crate::tick::PoolTicks {
    crate::tick::PoolTicks {
        kind: pool.kind,
        id: pool.id,
        home: pool.home,
        slot_base: tick_slot_base(pool),
        ticks,
    }
}

fn tick_slot_base(pool: &Concentrated) -> U256 {
    match pool.kind {
        PANCAKE_V3 => U256::from(6u8),
        AERO => U256::from(14u8),
        UNI_V4 => v4_state_slot(pool.id) + U256::from(4u8),
        _ => U256::from(5u8),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tick_storage(pool: &Concentrated, tick: i32) -> (Address, U256) {
    (
        pool.home,
        mapping_slot(i256_from_i32(tick), tick_slot_base(pool)),
    )
}

pub(crate) fn v4_state_slot(id: B256) -> U256 {
    keccak_pair(id, U256::from(6u8))
}

#[cfg_attr(not(test), allow(dead_code))]
fn mapping_slot(key: I256, slot: U256) -> U256 {
    keccak_pair(B256::from(key.to_be_bytes::<32>()), slot)
}

fn keccak_pair(left: B256, right: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left.as_slice());
    buf[32..].copy_from_slice(&right.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

#[cfg_attr(not(test), allow(dead_code))]
fn unpack_tick(word: U256) -> (u128, i128) {
    let gross = (word & U256::from(u128::MAX)).to::<u128>();
    let high: U256 = word >> 128;
    let net = high.to::<u128>() as i128;
    (gross, net)
}

pub(super) fn i256_from_i32(value: i32) -> I256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0; 32] };
    bytes[28..].copy_from_slice(&value.to_be_bytes());
    I256::from_be_bytes(bytes)
}
