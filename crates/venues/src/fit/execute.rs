//! Execution-lens quotes at the stored snapshot block.
//!
//! One multicall carries 16 swaps. The lens returns the output-token balance
//! delta. The state pipeline does not call this path.

use super::abi::{
    clip, decode_aggregate, encode_aggregate3, hex_prefix, interpret, push_addr, ChainOut, RawOut,
};
use super::extract::payload_of;
use super::plan::{plan_of, sample_at, PoolPlan, BISECT_STEPS};
use super::prepare::{prepare, Prep, Prepared};
use super::{multicall, QUOTE_LENS};
use crate::state::{
    curve_is_current, curve_job, keep_or_drop, quote_is_current, unix_now, QuoteRecord, WorkJob,
    QUOTE_VERSION,
};
use crate::{Rpc, VenueStore};
use alloy_primitives::{Address, B256, U256};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Quotes per Multicall3. Sixteen heavy quotes fit in one `eth_call` and return
/// in about the same time as a single quote.
pub(super) const CALLS_PER_BATCH: usize = 16;
/// Independent multicalls issued together. The provider runs these at once.
pub(super) const QUOTE_IN_FLIGHT: usize = 8;
pub(super) const CALL_GAS: u64 = 50_000_000;

pub(super) const LENS_RUNTIME: &str = include_str!("../../../../contracts/lens/QuoteLens.bin");

pub(super) struct Quoters {
    pub(super) code: BTreeMap<(Address, u64), bool>,
    /// Probed ERC-20 balance slot for the lens, at one snapshot block.
    pub(super) slots: BTreeMap<(Address, u64), Option<B256>>,
}

impl Quoters {
    /// One JSON-RPC batch for every quoter this page has not already probed.
    fn warm(&mut self, rpc: &Rpc, keys: &[(Address, u64)]) -> eyre::Result<()> {
        let mut missing = BTreeSet::new();
        for key in keys {
            if !self.code.contains_key(key) {
                missing.insert(*key);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let missing: Vec<(Address, u64)> = missing.into_iter().collect();
        let present = rpc.codes_at(&missing)?;
        for (key, present) in missing.into_iter().zip(present) {
            self.code.insert(key, present);
        }
        Ok(())
    }

    fn has_cached(&self, to: Address, block: u64) -> bool {
        self.code.get(&(to, block)).copied().unwrap_or(false)
    }
}

pub(super) struct Staged {
    pub(super) pool: usize,
    pub(super) dir: usize,
    pub(super) sample: usize,
    pub(super) block: u64,
    pub(super) prepared: Prepared,
}

#[derive(Clone)]
pub(super) struct RawCall {
    pub(super) block: u64,
    pub(super) target: Address,
    pub(super) data: Vec<u8>,
    pub(super) method: &'static str,
    pub(super) token_in: Address,
    pub(super) fund_eth: bool,
}

pub(super) struct Bound {
    pub(super) pool: usize,
    pub(super) dir: usize,
    pub(super) lo: U256,
    pub(super) hi: U256,
    pub(super) lo_out: Option<U256>,
    pub(super) steps: u8,
}

/// Fetch chain samples and store them. The state pipeline does not call this.
#[allow(dead_code)]
pub(crate) async fn drive_quotes<S: VenueStore>(
    store: &S,
    rpc: &Rpc,
    jobs: &[WorkJob],
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut follow = Vec::new();
    let mut plans = Vec::new();
    let mut picked = Vec::new();
    for (index, job) in jobs.iter().enumerate() {
        let state = store.read_state(job.kind, job.id).await?;
        let Some(mutable) = state.mutable.clone() else {
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        if job.generation != 0 && job.generation != mutable.generation {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        if quote_is_current(state.quotes.as_ref(), &mutable) {
            if !curve_is_current(state.curve.as_ref(), mutable.generation) {
                let mut next = curve_job(job.kind, job.id, mutable.generation);
                next.priority = job.priority;
                follow.push(next);
            }
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let curve = state.curve.clone();
        let generation = mutable.generation;
        plans.push(plan_of(job.kind, job.id, state));
        picked.push((index, generation, curve));
    }
    if !plans.is_empty() {
        let rpc = rpc.clone();
        match tokio::task::spawn_blocking(move || {
            let mut quoters = Quoters {
                code: BTreeMap::new(),
                slots: BTreeMap::new(),
            };
            quote_page(&rpc, &mut quoters, &mut plans)?;
            Ok::<_, eyre::Report>(plans)
        })
        .await
        {
            Ok(Ok(quoted)) => plans = quoted,
            Ok(Err(err)) => {
                let error = chain::rpc::redact_secret(&format!("{err:#}"));
                for (index, _, _) in &picked {
                    keep_or_drop(
                        &jobs[*index],
                        error.clone(),
                        give_up_after,
                        &mut done,
                        &mut failed,
                        notes,
                    );
                }
                store.finish_work(&done, &failed).await?;
                store.enqueue_work(&follow, unix_now()).await?;
                return Ok(());
            }
            Err(err) => return Err(eyre::eyre!("quote task: {err}")),
        }
    }
    let mut records = Vec::new();
    for (plan, (index, generation, curve)) in plans.into_iter().zip(picked) {
        let job = &jobs[index];
        if plan.missing.is_some() {
            keep_or_drop(
                job,
                plan.missing.unwrap_or_else(|| "missing snapshot".into()),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        records.push((
            index,
            generation,
            curve,
            QuoteRecord {
                kind: plan.kind,
                id: plan.id,
                generation,
                version: QUOTE_VERSION,
                block: plan.block,
                payload: payload_of(&plan),
            },
        ));
    }
    let batch: Vec<QuoteRecord> = records
        .iter()
        .map(|(_, _, _, record)| record.clone())
        .collect();
    if let Err(err) = store.put_quotes(&batch).await {
        let error = err.to_string();
        for (index, _, _, _) in &records {
            keep_or_drop(
                &jobs[*index],
                error.clone(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
        }
    } else {
        for (index, generation, curve, _) in records {
            let job = &jobs[index];
            if !curve_is_current(curve.as_ref(), generation) {
                let mut next = curve_job(job.kind, job.id, generation);
                next.priority = job.priority;
                follow.push(next);
            }
            done.push((job.stage, job.kind, job.id));
        }
    }
    store.finish_work(&done, &failed).await?;
    store.enqueue_work(&follow, unix_now()).await?;
    Ok(())
}

pub(super) fn quote_page(
    rpc: &Rpc,
    quoters: &mut Quoters,
    plans: &mut [PoolPlan],
) -> eyre::Result<()> {
    let mut staged = Vec::new();
    for (pool_index, plan) in plans.iter_mut().enumerate() {
        let Some(detail) = plan.detail.take() else {
            continue;
        };
        if plan.block == 0 {
            for dir in &mut plan.directions {
                for sample in &mut dir.samples {
                    sample.chain_error = Some("snapshot block is 0".into());
                }
            }
            plan.detail = Some(detail);
            continue;
        }
        let block = plan.block;
        for (dir_index, dir) in plan.directions.iter_mut().enumerate() {
            for (sample_index, sample) in dir.samples.iter_mut().enumerate() {
                match prepare(
                    &detail,
                    dir.token_in,
                    dir.token_out,
                    sample.amount_in,
                    block,
                ) {
                    Prep::Skip(reason) => sample.chain_error = Some(reason),
                    Prep::Ready(prepared) => {
                        sample.quoter = Some(prepared.target);
                        sample.method = Some(prepared.method);
                        sample.alt = prepared.alt.clone();
                        sample.alt_method = prepared.alt_method;
                        staged.push(Staged {
                            pool: pool_index,
                            dir: dir_index,
                            sample: sample_index,
                            block,
                            prepared,
                        });
                    }
                }
            }
        }
        plan.detail = Some(detail);
    }
    let keys: Vec<(Address, u64)> = staged
        .iter()
        .filter(|item| item.prepared.target != QUOTE_LENS)
        .map(|item| (item.prepared.target, item.block))
        .collect();
    quoters.warm(rpc, &keys)?;
    let mut calls = Vec::new();
    let mut back: Vec<(usize, usize, usize)> = Vec::new();
    for item in staged {
        let sample = &mut plans[item.pool].directions[item.dir].samples[item.sample];
        if item.prepared.target != QUOTE_LENS
            && !quoters.has_cached(item.prepared.target, item.block)
        {
            sample.chain_error = Some("quoter has no code at this block".into());
            sample.alt = None;
            continue;
        }
        sample.attempted = true;
        calls.push(RawCall {
            block: item.block,
            target: item.prepared.target,
            data: item.prepared.data,
            method: item.prepared.method,
            token_in: item.prepared.token_in,
            fund_eth: item.prepared.fund_eth,
        });
        back.push((item.pool, item.dir, item.sample));
    }
    apply_calls(rpc, quoters, plans, &calls, &back)?;
    calls.clear();
    back.clear();
    for (pool_index, plan) in plans.iter_mut().enumerate() {
        let block = plan.block;
        for (dir_index, dir) in plan.directions.iter_mut().enumerate() {
            for (sample_index, sample) in dir.samples.iter_mut().enumerate() {
                if sample.chain_out.is_some() {
                    continue;
                }
                let Some(data) = sample.alt.take() else {
                    continue;
                };
                let Some(method) = sample.alt_method else {
                    continue;
                };
                let Some(target) = sample.quoter else {
                    continue;
                };
                calls.push(RawCall {
                    block,
                    target,
                    data,
                    method,
                    token_in: dir.token_in,
                    fund_eth: dir.token_in == Address::ZERO,
                });
                back.push((pool_index, dir_index, sample_index));
            }
        }
    }
    apply_calls(rpc, quoters, plans, &calls, &back)?;
    bisect(rpc, quoters, plans)
}

pub(super) fn apply_calls(
    rpc: &Rpc,
    quoters: &mut Quoters,
    plans: &mut [PoolPlan],
    calls: &[RawCall],
    back: &[(usize, usize, usize)],
) -> eyre::Result<()> {
    let outs = dispatch_raw(rpc, quoters, calls)?;
    for (index, out) in outs.into_iter().enumerate() {
        let (pool, dir, sample) = back[index];
        let slot = &mut plans[pool].directions[dir].samples[sample];
        if let Some(amount) = out.amount_out {
            slot.chain_out = Some(amount);
            slot.chain_error = None;
            slot.method = Some(calls[index].method);
        } else if slot.chain_error.is_none() {
            slot.chain_error = out.error.or_else(|| Some("quote failed".into()));
        }
    }
    Ok(())
}

pub(super) fn bisect(rpc: &Rpc, quoters: &mut Quoters, plans: &mut [PoolPlan]) -> eyre::Result<()> {
    let mut bounds = Vec::new();
    for (pool_index, plan) in plans.iter().enumerate() {
        for (dir_index, dir) in plan.directions.iter().enumerate() {
            let Some(top) = dir.samples.last() else {
                continue;
            };
            if top.chain_out.is_some() {
                continue;
            }
            if !top.attempted {
                continue;
            }
            let mut lo = U256::ZERO;
            let mut lo_out = None;
            for sample in &dir.samples {
                if sample.chain_out.is_some() && sample.amount_in >= lo {
                    lo = sample.amount_in;
                    lo_out = sample.chain_out;
                }
            }
            bounds.push(Bound {
                pool: pool_index,
                dir: dir_index,
                lo,
                hi: top.amount_in,
                lo_out,
                steps: 0,
            });
        }
    }
    for _ in 0..BISECT_STEPS {
        let mut calls = Vec::new();
        let mut which = Vec::new();
        let mut staged = Vec::new();
        // Indexing keeps `plans` free while a bound is updated on a skipped mid.
        #[allow(clippy::needless_range_loop)]
        for index in 0..bounds.len() {
            let Some(step) = bounds[index].lo.checked_add(U256::from(1u8)) else {
                continue;
            };
            if bounds[index].steps >= BISECT_STEPS || bounds[index].hi <= step {
                continue;
            }
            let mid = bounds[index].lo + (bounds[index].hi - bounds[index].lo) / U256::from(2u8);
            let pool = bounds[index].pool;
            let dir_index = bounds[index].dir;
            let prepared = {
                let plan = &plans[pool];
                let Some(detail) = plan.detail.as_ref() else {
                    bounds[index].steps = BISECT_STEPS;
                    continue;
                };
                let dir = &plan.directions[dir_index];
                prepare(detail, dir.token_in, dir.token_out, mid, plan.block)
            };
            let Prep::Ready(prepared) = prepared else {
                bounds[index].hi = mid;
                bounds[index].steps = bounds[index].steps.saturating_add(1);
                continue;
            };
            let block = plans[pool].block;
            staged.push((index, mid, block, prepared));
        }
        let keys: Vec<(Address, u64)> = staged
            .iter()
            .filter(|(_, _, _, prepared)| prepared.target != QUOTE_LENS)
            .map(|(_, _, block, prepared)| (prepared.target, *block))
            .collect();
        quoters.warm(rpc, &keys)?;
        for (index, mid, block, prepared) in staged {
            if prepared.target != QUOTE_LENS && !quoters.has_cached(prepared.target, block) {
                bounds[index].hi = mid;
                bounds[index].steps = bounds[index].steps.saturating_add(1);
                continue;
            }
            which.push((index, mid));
            calls.push(RawCall {
                block,
                target: prepared.target,
                data: prepared.data,
                method: prepared.method,
                token_in: prepared.token_in,
                fund_eth: prepared.fund_eth,
            });
        }
        if calls.is_empty() {
            break;
        }
        let outs = dispatch_raw(rpc, quoters, &calls)?;
        for ((index, mid), out) in which.into_iter().zip(outs) {
            let bound = &mut bounds[index];
            bound.steps = bound.steps.saturating_add(1);
            if let Some(amount) = out.amount_out {
                bound.lo = mid;
                bound.lo_out = Some(amount);
            } else {
                bound.hi = mid;
            }
        }
    }
    for bound in bounds {
        let already = plans[bound.pool].directions[bound.dir]
            .samples
            .iter()
            .any(|sample| sample.amount_in == bound.lo);
        let tokens = (
            plans[bound.pool].directions[bound.dir].token_in,
            plans[bound.pool].directions[bound.dir].token_out,
        );
        let detail = if bound.lo.is_zero() || already {
            None
        } else {
            plans[bound.pool].detail.clone()
        };
        let dir = &mut plans[bound.pool].directions[bound.dir];
        dir.larger_input_reverted = true;
        if bound.lo.is_zero() {
            continue;
        }
        dir.chain_max_in = Some(bound.lo);
        let Some(detail) = detail else {
            continue;
        };
        let mut sample = sample_at(&detail, tokens.0, tokens.1, "chain_max", None, bound.lo);
        sample.chain_out = bound.lo_out;
        sample.attempted = true;
        if sample.chain_out.is_none() {
            sample.chain_error = Some("bisection lost the quote".into());
        }
        dir.samples.push(sample);
    }
    for plan in plans.iter_mut() {
        for dir in &mut plan.directions {
            if dir.chain_max_in.is_some() {
                continue;
            }
            if let Some(top) = dir.samples.last() {
                if top.role == "grid" && top.chain_out.is_some() {
                    dir.chain_max_in = Some(top.amount_in);
                }
            }
        }
    }
    Ok(())
}

pub(super) struct CallChunk {
    pub(super) block: u64,
    pub(super) gas: u64,
    pub(super) indices: Vec<usize>,
    pub(super) overrides: Value,
}

pub(super) fn dispatch_raw(
    rpc: &Rpc,
    quoters: &mut Quoters,
    calls: &[RawCall],
) -> eyre::Result<Vec<ChainOut>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let mut outs = vec![
        ChainOut {
            amount_out: None,
            error: Some("unrun".into()),
        };
        calls.len()
    ];
    let mut runnable = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if call.block == 0 {
            outs[index] = ChainOut {
                amount_out: None,
                error: Some("snapshot block is 0".into()),
            };
            continue;
        }
        if call.token_in == Address::ZERO && !call.fund_eth {
            outs[index] = ChainOut {
                amount_out: None,
                error: Some("token".into()),
            };
            continue;
        }
        if call.token_in != Address::ZERO
            && slot_for(rpc, quoters, call.token_in, call.block)?.is_none()
        {
            outs[index] = ChainOut {
                amount_out: None,
                error: Some("balance slot".into()),
            };
            continue;
        }
        runnable.push(index);
    }
    let mut order: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for index in runnable {
        order.entry(calls[index].block).or_default().push(index);
    }
    let mut chunks = Vec::new();
    for (block, indices) in order {
        for chunk in indices.chunks(CALLS_PER_BATCH) {
            chunks.push(CallChunk {
                block,
                gas: CALL_GAS,
                indices: chunk.to_vec(),
                overrides: chunk_override(quoters, calls, block, chunk)?,
            });
        }
    }
    if chunks.is_empty() {
        return Ok(outs);
    }
    let groups = run_chunks(rpc, calls, &chunks)?;
    for (chunk, raws) in chunks.iter().zip(groups) {
        if raws.len() != chunk.indices.len() {
            return Err(eyre::eyre!("quote batch length"));
        }
        for (index, raw) in chunk.indices.iter().zip(raws) {
            outs[*index] = interpret(calls[*index].method, raw);
        }
    }
    Ok(outs)
}

pub(super) fn slot_for(
    rpc: &Rpc,
    quoters: &mut Quoters,
    token: Address,
    block: u64,
) -> eyre::Result<Option<B256>> {
    let key = (token, block);
    if let Some(found) = quoters.slots.get(&key) {
        return Ok(*found);
    }
    let found = probe_balance_slot(rpc, token, block)?;
    quoters.slots.insert(key, found);
    Ok(found)
}

pub(super) fn probe_balance_slot(
    rpc: &Rpc,
    token: Address,
    block: u64,
) -> eyre::Result<Option<B256>> {
    let mut diff = serde_json::Map::new();
    let mut keys = Vec::with_capacity(10);
    for slot in 0..10u64 {
        let key = balance_slot(QUOTE_LENS, slot);
        let pattern = probe_pattern(slot);
        diff.insert(hex_prefix(key.as_slice()), json!(word_hex(pattern)));
        keys.push((key, pattern));
    }
    let mut token_override = serde_json::Map::new();
    token_override.insert("stateDiff".into(), Value::Object(diff));
    let mut accounts = serde_json::Map::new();
    accounts.insert(token.to_string(), Value::Object(token_override));
    let overrides = Value::Object(accounts);
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    push_addr(&mut data, QUOTE_LENS);
    let call = rpc.eth_call_block(&data, Some(token), 1_000_000, block, &overrides)?;
    if call.reverted || call.bytes.len() < 32 {
        return Ok(None);
    }
    let got = U256::from_be_slice(&call.bytes[..32]);
    Ok(keys
        .into_iter()
        .find(|(_, pattern)| *pattern == got)
        .map(|(key, _)| key))
}

pub(super) fn chunk_override(
    quoters: &Quoters,
    calls: &[RawCall],
    block: u64,
    indices: &[usize],
) -> eyre::Result<Value> {
    let mut funded = BTreeMap::new();
    let mut fund_eth = false;
    for index in indices {
        let call = &calls[*index];
        fund_eth |= call.fund_eth;
        if call.token_in == Address::ZERO {
            continue;
        }
        let slot = quoters
            .slots
            .get(&(call.token_in, block))
            .copied()
            .flatten()
            .ok_or_else(|| eyre::eyre!("balance slot missing"))?;
        funded.insert(call.token_in, slot);
    }
    Ok(execution_override(fund_eth, &funded))
}

pub(super) fn execution_override(fund_eth: bool, funded: &BTreeMap<Address, B256>) -> Value {
    let mut lens = serde_json::Map::new();
    lens.insert("code".into(), json!(format!("0x{}", LENS_RUNTIME.trim())));
    if fund_eth {
        lens.insert("balance".into(), json!(word_hex(fund_balance())));
    }
    let mut accounts = serde_json::Map::new();
    accounts.insert(QUOTE_LENS.to_string(), Value::Object(lens));
    let balance = word_hex(fund_balance());
    for (token, slot) in funded {
        let mut diff = serde_json::Map::new();
        diff.insert(hex_prefix(slot.as_slice()), json!(balance.clone()));
        let mut token_override = serde_json::Map::new();
        token_override.insert("stateDiff".into(), Value::Object(diff));
        accounts.insert(token.to_string(), Value::Object(token_override));
    }
    Value::Object(accounts)
}

pub(super) fn balance_slot(holder: Address, slot: u64) -> B256 {
    let mut encoded = [0u8; 64];
    encoded[12..32].copy_from_slice(holder.as_slice());
    encoded[32..].copy_from_slice(&U256::from(slot).to_be_bytes::<32>());
    alloy_primitives::keccak256(encoded)
}

pub(super) fn probe_pattern(slot: u64) -> U256 {
    let mut bytes = [0x51u8; 32];
    bytes[31] = u8::try_from(slot).unwrap_or(0);
    U256::from_be_bytes(bytes)
}

pub(super) fn fund_balance() -> U256 {
    U256::from(1u8) << 128
}

pub(super) fn word_hex(value: U256) -> String {
    format!(
        "0x{}",
        alloy_primitives::hex::encode(value.to_be_bytes::<32>())
    )
}

/// Run multicalls on a few threads. Each thread has its own `Rpc` clone so the
/// request id is not shared. The HTTP client pool is shared.
pub(super) fn run_chunks(
    rpc: &Rpc,
    calls: &[RawCall],
    chunks: &[CallChunk],
) -> eyre::Result<Vec<Vec<RawOut>>> {
    let count = chunks.len();
    if count == 0 {
        return Ok(Vec::new());
    }
    let workers = count.min(QUOTE_IN_FLIGHT);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let next = &next;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let rpc = rpc.clone();
            handles.push(
                scope.spawn(move || -> eyre::Result<Vec<(usize, Vec<RawOut>)>> {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= count {
                            break;
                        }
                        let chunk = &chunks[index];
                        let prepared: Vec<(Address, &[u8])> = chunk
                            .indices
                            .iter()
                            .map(|call| (calls[*call].target, calls[*call].data.as_slice()))
                            .collect();
                        let raws =
                            execute(&rpc, chunk.block, &prepared, chunk.gas, &chunk.overrides)?;
                        done.push((index, raws));
                    }
                    Ok(done)
                }),
            );
        }
        let mut groups = (0..count).map(|_| Vec::new()).collect::<Vec<_>>();
        let mut filled = vec![false; count];
        for handle in handles {
            let done = match handle.join() {
                Ok(result) => result?,
                Err(_) => return Err(eyre::eyre!("quote thread panicked")),
            };
            for (index, raws) in done {
                groups[index] = raws;
                filled[index] = true;
            }
        }
        if filled.iter().any(|done| !done) {
            return Err(eyre::eyre!("quote chunk dropped"));
        }
        Ok(groups)
    })
}

pub(super) fn execute(
    rpc: &Rpc,
    block: u64,
    calls: &[(Address, &[u8])],
    gas: u64,
    overrides: &Value,
) -> eyre::Result<Vec<RawOut>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let data = encode_aggregate3(calls);
    let call = rpc.eth_call_block(&data, Some(multicall()), gas, block, overrides)?;
    if call.reverted {
        return split_or_fail(rpc, block, calls, gas, overrides, &call.message);
    }
    match decode_aggregate(&call.bytes) {
        Some(inners) if inners.len() == calls.len() => Ok(inners
            .into_iter()
            .map(|(ok, bytes)| RawOut {
                ok,
                bytes,
                error: None,
            })
            .collect()),
        _ => split_or_fail(rpc, block, calls, gas, overrides, "aggregate decode"),
    }
}

pub(super) fn split_or_fail(
    rpc: &Rpc,
    block: u64,
    calls: &[(Address, &[u8])],
    gas: u64,
    overrides: &Value,
    message: &str,
) -> eyre::Result<Vec<RawOut>> {
    if calls.len() == 1 {
        return Ok(vec![RawOut {
            ok: false,
            bytes: Vec::new(),
            error: Some(clip(message)),
        }]);
    }
    let mid = calls.len() / 2;
    let mut outs = execute(rpc, block, &calls[..mid], gas, overrides)?;
    outs.extend(execute(rpc, block, &calls[mid..], gas, overrides)?);
    Ok(outs)
}
