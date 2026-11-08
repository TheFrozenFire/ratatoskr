//! Second-call tick words.
//!
//! `DetailLens` returns bitmap words only. This lens reads the initialized tick
//! structs, several pools per `eth_call`, capped so one call stays inside a
//! hosted RPC's gas limit and revert-data size. `eth_getStorageValues` remains
//! the preferred second call when the provider implements it.

use crate::lens::CALL_GAS;
use crate::rpc::Rpc;
use alloy_primitives::{Address, B256, I256, U256};
use eyre::eyre;
use std::sync::OnceLock;

const MAGIC: [u8; 32] = [
    0xfc, 0xba, 0x65, 0xb2, 0x0d, 0x7c, 0x4d, 0x16, 0x20, 0x3a, 0x3c, 0xd7, 0x1d, 0x89, 0x5c, 0xe7,
    0x0f, 0xf6, 0x88, 0xc2, 0x42, 0x4e, 0xae, 0x5d, 0x34, 0xa5, 0xd0, 0xbe, 0xd0, 0x6b, 0x5d, 0x0f,
];
const CREATION_HEX: &str = include_str!("../../../contracts/lens/TickLens.bin");
/// Ticks packed into one `eth_call`.
///
/// A tick is one `ticks()` or `extsload` staticcall, on the order of a few tens
/// of thousands of gas, plus two 32-byte words in the revert payload. 512 ticks
/// stay well under the 50_000_000 gas passed to `eth_call` and under a few
/// hundred kilobytes of response data. A provider that still rejects the call
/// is retried with the batch split in half. `eth_getStorageValues` uses the
/// execution-api cap of 1024 slots instead of this limit.
const TICK_BATCH: usize = 512;
const FLAG_TRUNCATED: u64 = 1;
const FLAG_FAILED: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PoolTicks {
    pub kind: u8,
    pub id: B256,
    pub home: Address,
    pub slot_base: U256,
    pub ticks: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TickWordRead {
    pub tick: i32,
    pub word: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TickLoad {
    pub ticks: Vec<Vec<TickWordRead>>,
    /// A `ticks()` or `extsload` staticcall failed in this pool.
    pub failed: Vec<bool>,
}

#[derive(Debug, Clone)]
struct Work {
    index: usize,
    kind: u8,
    id: B256,
    home: Address,
    slot_base: U256,
    ticks: Vec<i32>,
}

struct Decoded {
    words: Vec<TickWordRead>,
    truncated: bool,
    failed: bool,
}

pub(crate) fn load_ticks(rpc: &Rpc, pools: &[PoolTicks]) -> eyre::Result<TickLoad> {
    let mut ticks = vec![Vec::new(); pools.len()];
    let mut failed = vec![false; pools.len()];
    let pending = pools
        .iter()
        .enumerate()
        .filter(|(_, pool)| !pool.ticks.is_empty())
        .map(|(index, pool)| Work {
            index,
            kind: pool.kind,
            id: pool.id,
            home: pool.home,
            slot_base: pool.slot_base,
            ticks: pool.ticks.clone(),
        })
        .collect();
    send(rpc, &mut ticks, &mut failed, pending)?;
    Ok(TickLoad { ticks, failed })
}

fn send(
    rpc: &Rpc,
    ticks: &mut [Vec<TickWordRead>],
    failed: &mut [bool],
    mut pending: Vec<Work>,
) -> eyre::Result<()> {
    let mut cap = TICK_BATCH;
    while !pending.is_empty() {
        let (batch, rest) = take_batch(std::mem::take(&mut pending), cap);
        match fetch(rpc, &batch) {
            Ok(rows) => {
                cap = TICK_BATCH;
                let mut retry = Vec::new();
                for (work, row) in batch.into_iter().zip(rows) {
                    if row.words.len() > work.ticks.len() {
                        return Err(eyre!("tick lens returned extra words"));
                    }
                    for (asked, read) in work.ticks.iter().zip(&row.words) {
                        if read.tick != *asked {
                            return Err(eyre!("tick lens returned {0} for {asked}", read.tick));
                        }
                    }
                    failed[work.index] |= row.failed;
                    let done = row.words.len();
                    ticks[work.index].extend(row.words);
                    if row.truncated {
                        if done == 0 {
                            return Err(eyre!("tick lens made no progress"));
                        }
                        let mut again = work;
                        again.ticks = again.ticks[done..].to_vec();
                        retry.push(again);
                    } else if done != work.ticks.len() {
                        return Err(eyre!("tick lens stopped without the truncated flag"));
                    }
                }
                retry.extend(rest);
                pending = retry;
            }
            Err(_) if tick_count(&batch) > 1 => {
                let (left, right) = split_even(batch);
                cap = tick_count(&left).max(1);
                let mut again = left;
                again.extend(right);
                again.extend(rest);
                pending = again;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn fetch(rpc: &Rpc, batch: &[Work]) -> eyre::Result<Vec<Decoded>> {
    let call = rpc.eth_call(&call_data(batch), None, CALL_GAS)?;
    decode(&call.bytes, batch).map_err(|err| eyre!("{err}: {}", call.message))
}

fn take_batch(works: Vec<Work>, cap: usize) -> (Vec<Work>, Vec<Work>) {
    let mut batch = Vec::new();
    let mut rest = Vec::new();
    let mut room = cap.max(1);
    for work in works {
        if work.ticks.is_empty() {
            continue;
        }
        if room == 0 {
            rest.push(work);
            continue;
        }
        if work.ticks.len() <= room {
            room -= work.ticks.len();
            batch.push(work);
        } else {
            let mut head = work.clone();
            head.ticks.truncate(room);
            let mut tail = work;
            tail.ticks.drain(..room);
            rest.push(tail);
            batch.push(head);
            room = 0;
        }
    }
    (batch, rest)
}

fn split_even(batch: Vec<Work>) -> (Vec<Work>, Vec<Work>) {
    let total = tick_count(&batch).max(2);
    take_batch(batch, total / 2)
}

fn tick_count(works: &[Work]) -> usize {
    works.iter().map(|work| work.ticks.len()).sum()
}

fn call_data(works: &[Work]) -> Vec<u8> {
    let mut words = vec![U256::from(works.len())];
    for work in works {
        words.push(U256::from(work.kind));
        words.push(U256::from_be_slice(work.id.as_slice()));
        words.push(address_word(work.home));
        words.push(work.slot_base);
        words.push(U256::from(work.ticks.len()));
        for tick in &work.ticks {
            words.push(u256_from_i32(*tick));
        }
    }
    let mut out = creation_code().to_vec();
    push_u256(&mut out, U256::from(0x20u8));
    push_u256(&mut out, U256::from(words.len()));
    for word in words {
        push_u256(&mut out, word);
    }
    out
}

fn decode(bytes: &[u8], works: &[Work]) -> Result<Vec<Decoded>, String> {
    if bytes.len() < 64 || !bytes.len().is_multiple_of(32) || bytes[..32] != MAGIC {
        return Err("tick payload has no magic".into());
    }
    let count = read_u64(&bytes[32..64])?;
    if count as usize != works.len() {
        return Err(format!("tick count {count}"));
    }
    let mut at = 64;
    let mut rows = Vec::with_capacity(works.len());
    for work in works {
        if at + 128 > bytes.len() {
            return Err("tick record header".into());
        }
        let n_words = read_u64(&bytes[at..at + 32])?;
        let end = at
            .checked_add(n_words as usize * 32)
            .ok_or_else(|| "tick record length".to_string())?;
        if end > bytes.len() || n_words < 4 {
            return Err("tick record length".into());
        }
        let id = B256::from_slice(&bytes[at + 32..at + 64]);
        if id != work.id {
            return Err("tick record id".into());
        }
        let flags = read_u64(&bytes[at + 64..at + 96])?;
        let written = read_u64(&bytes[at + 96..at + 128])? as usize;
        if n_words != 4 + written as u64 * 2 {
            return Err("tick record length".into());
        }
        let mut words = Vec::with_capacity(written);
        let mut cursor = at + 128;
        for _ in 0..written {
            let tick = fit_i32(&bytes[cursor..cursor + 32])?;
            let word = U256::from_be_slice(&bytes[cursor + 32..cursor + 64]);
            words.push(TickWordRead { tick, word });
            cursor += 64;
        }
        if cursor != end {
            return Err("tick record length".into());
        }
        at = end;
        rows.push(Decoded {
            words,
            truncated: flags & FLAG_TRUNCATED != 0,
            failed: flags & FLAG_FAILED != 0,
        });
    }
    if at != bytes.len() {
        return Err("tick payload tail".into());
    }
    Ok(rows)
}

fn creation_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        alloy_primitives::hex::decode(CREATION_HEX.trim()).expect("TickLens.bin is hex")
    })
}

fn address_word(address: Address) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    U256::from_be_bytes(bytes)
}

fn u256_from_i32(value: i32) -> U256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0; 32] };
    bytes[28..].copy_from_slice(&value.to_be_bytes());
    U256::from_be_bytes(bytes)
}

fn push_u256(out: &mut Vec<u8>, word: U256) {
    out.extend_from_slice(&word.to_be_bytes::<32>());
}

fn read_u64(word: &[u8]) -> Result<u64, String> {
    if word.len() != 32 || word[..24].iter().any(|byte| *byte != 0) {
        return Err("word".into());
    }
    Ok(u64::from_be_bytes(word[24..32].try_into().unwrap()))
}

fn fit_i32(word: &[u8]) -> Result<i32, String> {
    let bytes: [u8; 32] = word.try_into().map_err(|_| "tick".to_string())?;
    i32::try_from(I256::from_be_bytes::<32>(bytes)).map_err(|_| "tick".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};

    fn work(id_byte: u8, ticks: &[i32]) -> Work {
        Work {
            index: 0,
            kind: 5,
            id: B256::repeat_byte(id_byte),
            home: address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"),
            slot_base: U256::from(5u8),
            ticks: ticks.to_vec(),
        }
    }

    #[test]
    fn bytecode_carries_the_tick_magic() {
        assert_eq!(keccak256("ratatoskr.tick.v1").0, MAGIC);
        let code = creation_code();
        assert!(code.len() < 49_152, "{}", code.len());
        assert!(code
            .windows(33)
            .any(|window| window[0] == 0x7f && window[1..] == MAGIC));
    }

    #[test]
    fn constructor_argument_is_a_dynamic_array() {
        let data = call_data(&[work(0x11, &[184_320, -60])]);
        let args = &data[creation_code().len()..];
        // offset, length, poolCount, five header words, two ticks.
        assert_eq!(args.len(), 32 * (2 + 1 + 5 + 2));
        assert_eq!(args[31], 0x20);
        assert_eq!(args[63], 8);
        assert_eq!(args[95], 1);
        assert_eq!(args[32 * 7 + 31], 2);
        assert_eq!(
            &args[32 * 8..32 * 9],
            &u256_from_i32(184_320).to_be_bytes::<32>()
        );
        assert_eq!(
            &args[32 * 9..32 * 10],
            &u256_from_i32(-60).to_be_bytes::<32>()
        );
    }

    fn batch_plan(counts: &[usize], cap: usize) -> Vec<Vec<usize>> {
        let works: Vec<Work> = counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, count)| Work {
                index,
                kind: 0,
                id: B256::ZERO,
                home: Address::ZERO,
                slot_base: U256::ZERO,
                ticks: vec![0; *count],
            })
            .collect();
        let mut pending = works;
        let mut plans = Vec::new();
        while !pending.is_empty() {
            let (batch, rest) = take_batch(pending, cap);
            plans.push(batch.iter().map(|work| work.ticks.len()).collect());
            pending = rest;
        }
        plans
    }

    #[test]
    fn batches_mix_pools_up_to_the_cap() {
        assert_eq!(batch_plan(&[200, 300], 512), vec![vec![200, 300]]);
        assert_eq!(batch_plan(&[400, 200], 512), vec![vec![400, 112], vec![88]]);
        assert_eq!(
            batch_plan(&[400, 400], 512),
            vec![vec![400, 112], vec![288]]
        );
        assert_eq!(batch_plan(&[0, 10, 10], 16), vec![vec![10, 6], vec![4]]);
    }

    #[test]
    fn payload_round_trip_keeps_negative_ticks() {
        let works = [work(0x22, &[184_320, -60])];
        let mut body = Vec::new();
        push_u256(&mut body, U256::from_be_slice(&MAGIC));
        push_u256(&mut body, U256::from(1u8));
        push_u256(&mut body, U256::from(8u8));
        push_u256(&mut body, U256::from_be_slice(works[0].id.as_slice()));
        push_u256(&mut body, U256::from(FLAG_FAILED));
        push_u256(&mut body, U256::from(2u8));
        push_u256(&mut body, u256_from_i32(184_320));
        push_u256(&mut body, U256::from(9u8));
        push_u256(&mut body, u256_from_i32(-60));
        push_u256(&mut body, U256::from(4u8));
        let rows = decode(&body, &works).unwrap();
        assert!(rows[0].failed);
        assert!(!rows[0].truncated);
        assert_eq!(rows[0].words[0].tick, 184_320);
        assert_eq!(rows[0].words[1].tick, -60);
        assert_eq!(rows[0].words[1].word, U256::from(4u8));
    }
}
