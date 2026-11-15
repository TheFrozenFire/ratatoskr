//! One detail-lens page, resumed when the lens truncates the scan.

use super::codec::{decode_details, scan_all, LENS_BATCH};
use super::slots::i256_from_i32;
use super::types::*;
use crate::lens::CALL_GAS;
use crate::rpc::Rpc;
use alloy_primitives::{Address, U256};
use eyre::eyre;
use std::sync::OnceLock;
use store_model::VenueRow;

const CREATION_HEX: &str = include_str!("../../../../contracts/lens/DetailLens.bin");
const RESUME_ROUNDS: u32 = 8;

pub fn load_details(rpc: &Rpc, rows: &[VenueRow]) -> eyre::Result<Vec<PoolDetail>> {
    load_details_at(rpc, rows, None)
}

/// Detail lens only. Tick words are a later stage, at the same `block` when
/// the caller pinned one. `None` uses the tag `latest`.
pub fn load_details_at(
    rpc: &Rpc,
    rows: &[VenueRow],
    block: Option<u64>,
) -> eyre::Result<Vec<PoolDetail>> {
    let queries = rows.iter().map(DetailQuery::from_row).collect::<Vec<_>>();
    let mut out = Vec::with_capacity(queries.len());
    for chunk in queries.chunks(LENS_BATCH) {
        let mut page = fetch_page(rpc, chunk, block)?;
        for (query, detail) in chunk.iter().zip(page.iter_mut()) {
            resume_one(rpc, query, detail, block)?;
        }
        out.extend(page);
    }
    Ok(out)
}

pub(crate) fn creation_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        alloy_primitives::hex::decode(CREATION_HEX.trim()).expect("DetailLens.bin is hex")
    })
}

pub fn call_data(queries: &[DetailQuery]) -> Vec<u8> {
    let mut out = creation_code().to_vec();
    push_u256(&mut out, U256::from(0x20));
    push_u256(&mut out, U256::from(queries.len() * 8));
    for query in queries {
        push_u256(&mut out, U256::from(query.kind));
        out.extend_from_slice(query.id.as_slice());
        push_u256(&mut out, address_word(query.home));
        push_u256(&mut out, address_word(query.aux));
        push_u256(&mut out, scan_word(query.scan_from));
        push_u256(&mut out, address_word(query.token0));
        push_u256(&mut out, address_word(query.token1));
        push_u256(&mut out, u256_from_i32(query.tick_spacing));
    }
    out
}

fn fetch_page(
    rpc: &Rpc,
    queries: &[DetailQuery],
    block: Option<u64>,
) -> eyre::Result<Vec<PoolDetail>> {
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let call = rpc.eth_call_at(&call_data(queries), None, CALL_GAS, block)?;
    match decode_details(&call.bytes, queries) {
        Ok(rows) => Ok(rows),
        Err(_err) if queries.len() > 1 => {
            let mid = queries.len() / 2;
            let mut rows = fetch_page(rpc, &queries[..mid], block)?;
            rows.extend(fetch_page(rpc, &queries[mid..], block)?);
            Ok(rows)
        }
        Err(err) => Err(eyre!("{err}: {}", call.message)),
    }
}

fn resume_one(
    rpc: &Rpc,
    query: &DetailQuery,
    detail: &mut PoolDetail,
    block: Option<u64>,
) -> eyre::Result<()> {
    for _ in 0..RESUME_ROUNDS {
        let Some(at) = resume_at(detail) else {
            return Ok(());
        };
        let mut next = query.clone();
        next.scan_from = ScanFrom::At(at);
        let mut page = fetch_page(rpc, &[next], block)?;
        let Some(page) = page.pop() else {
            return Ok(());
        };
        if resume_at(&page) == Some(at) {
            return Ok(());
        }
        *detail = merge_page(detail.clone(), page);
    }
    Ok(())
}

fn resume_at(detail: &PoolDetail) -> Option<i32> {
    match detail {
        PoolDetail::Concentrated(pool) if pool.flags & FLAG_TRUNCATED != 0 => pool.next_scan,
        PoolDetail::Metric(pool) if pool.flags & FLAG_TRUNCATED != 0 => pool.next_scan,
        _ => None,
    }
}

fn merge_page(base: PoolDetail, page: PoolDetail) -> PoolDetail {
    match (base, page) {
        (PoolDetail::Concentrated(mut base), PoolDetail::Concentrated(page)) => {
            base.bitmaps.extend(page.bitmaps);
            base.next_scan = page.next_scan;
            base.flags = (base.flags & !FLAG_TRUNCATED) | (page.flags & FLAG_TRUNCATED);
            base.flags |= page.flags & !FLAG_TRUNCATED;
            PoolDetail::Concentrated(base)
        }
        (PoolDetail::Metric(mut base), PoolDetail::Metric(page)) => {
            base.bins.extend(page.bins);
            base.next_scan = page.next_scan;
            base.flags = (base.flags & !FLAG_TRUNCATED) | (page.flags & FLAG_TRUNCATED);
            base.flags |= page.flags & !FLAG_TRUNCATED;
            PoolDetail::Metric(base)
        }
        (base, _) => base,
    }
}

pub(crate) fn address_word(address: Address) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    U256::from_be_bytes(bytes)
}

fn scan_word(scan: ScanFrom) -> U256 {
    match scan {
        ScanFrom::Start => U256::from_be_bytes(scan_all()),
        ScanFrom::At(value) => u256_from_i32(value),
    }
}

pub(crate) fn u256_from_i32(value: i32) -> U256 {
    U256::from_be_bytes::<32>(i256_from_i32(value).to_be_bytes::<32>())
}

fn push_u256(out: &mut Vec<u8>, word: U256) {
    out.extend_from_slice(&word.to_be_bytes::<32>());
}
