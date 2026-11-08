//! On-chain quote samples and the curve-fit extract.
//!
//! The state pipeline calls the protocol quoter and stores the samples next to
//! the snapshot. The curve stage runs after that. This module also copies the
//! stored samples into a JSONL file and can load a file written by an older
//! export back into the catalog.
//!
//! The fit domain is a fraction of the largest input that still has a price.
//! Constant-product pools have no hard maximum, so the top of the domain is
//! the input that buys 99% of the output reserve. A concentrated book ends at
//! the last ladder step. Balancer and Curve search locally toward 99% of the
//! output balance. Samples are those fractions, not absolute token amounts.
//! Dust is geometric. The upper end is closer to linear, because price impact
//! and tick crosses pile up there.
//!
//! Chain amounts come from the protocol quoter via Multicall3 at the mutable
//! snapshot block. `local` is this crate's quote of the same snapshot and is
//! recomputed when the file is written. It is not stored.

use crate::catalog::{
    kind_name, AERO, BALANCER_V2, BALANCER_V3, METRIC, PANCAKE_V2, PANCAKE_V3, SUSHI_V2, SUSHI_V3,
    UNI_V2, UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};
use crate::detail::{product_fee_pips, PoolDetail, Quote};
use crate::state::{
    curve_is_current, curve_job, is_canonical, join_detail, keep_or_drop, quote_is_current,
    unix_now, CurveRecord, PoolState, QuoteRecord, WorkJob, QUOTE_VERSION,
};
use crate::Catalog;
use crate::EntityStore;
use crate::Rpc;
use alloy_primitives::{address, Address, B256, U256};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");
const UNI_V2_ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
const SUSHI_V2_ROUTER: Address = address!("d9e1cE17f2641f24aE83637ab66a2cca9C378B9F");
const PANCAKE_V2_ROUTER: Address = address!("EfF92A263d31888d860bD50809A8D171709b7b1c");
const UNI_V3_QUOTER: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const PANCAKE_V3_QUOTER: Address = address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");
const SUSHI_V3_QUOTER: Address = address!("64e8802FE490fa7cc61d3463958199161Bb608A7");
const UNI_V4_QUOTER: Address = address!("99bd42B2B22e531f283b7F8a86dFf631EB16A9D1");
const BALANCER_V3_ROUTER: Address = address!("ae563e3f8219521950555f5962419c8919758ea2");

const SEL_AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
const SEL_GET_AMOUNTS: [u8; 4] = [0xd0, 0x6c, 0xa6, 0x1f];
const SEL_V3_QUOTE: [u8; 4] = [0xc6, 0xa5, 0x02, 0x6a];
const SEL_V4_QUOTE: [u8; 4] = [0xaa, 0x9d, 0x21, 0xcb];
const SEL_DY_INT: [u8; 4] = [0x5e, 0x0d, 0x44, 0x3f];
const SEL_DY_UINT: [u8; 4] = [0x55, 0x6d, 0x6e, 0x9f];
const SEL_BAL3: [u8; 4] = [0x3e, 0xbc, 0x54, 0xe5];
const SEL_BAL2: [u8; 4] = [0xf8, 0x4d, 0x06, 0x6e];

/// Fractions of `max_in`. The first four are geometric. The rest sit where
/// the book bends.
const GRID: &[(&str, u32, u32)] = &[
    ("0.0001", 1, 10_000),
    ("0.001", 1, 1_000),
    ("0.01", 1, 100),
    ("0.05", 1, 20),
    ("0.1", 1, 10),
    ("0.25", 1, 4),
    ("0.5", 1, 2),
    ("0.8", 4, 5),
    ("0.95", 19, 20),
    ("1", 1, 1),
];

const MAX_LEGS: usize = 4;
const BISECT_STEPS: u8 = 8;
const PAGE: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    V2,
    V3,
    V4,
    Curve,
    Bal2,
    Bal3,
}

fn batch_of(family: Family) -> (usize, u64) {
    match family {
        Family::V2 => (16, 12_000_000),
        Family::Curve => (8, 20_000_000),
        Family::V3 | Family::V4 => (2, 50_000_000),
        Family::Bal2 | Family::Bal3 => (4, 30_000_000),
    }
}

/// Pools written, plus chain-quote counts. `resumed` were already in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FitReport {
    pub pools: u64,
    pub resumed: u64,
    pub chain_ok: u64,
    pub chain_fail: u64,
    pub chain_absent: u64,
}

struct Quoters {
    code: BTreeMap<(Address, u64), bool>,
}

impl Quoters {
    fn has_code(&mut self, rpc: &Rpc, to: Address, block: u64) -> eyre::Result<bool> {
        if let Some(present) = self.code.get(&(to, block)).copied() {
            return Ok(present);
        }
        let present = rpc.code_len(to, Some(block))? > 0;
        self.code.insert((to, block), present);
        Ok(present)
    }
}

struct Sample {
    role: &'static str,
    fraction: Option<&'static str>,
    amount_in: U256,
    local_out: Option<U256>,
    local_fee: Option<U256>,
    local_exhausted: Option<bool>,
    local_error: Option<String>,
    chain_out: Option<U256>,
    chain_error: Option<String>,
    quoter: Option<Address>,
    method: Option<&'static str>,
    attempted: bool,
    alt: Option<Vec<u8>>,
    alt_method: Option<&'static str>,
}

struct Direction {
    token_in: Address,
    token_out: Address,
    max_in: U256,
    max_source: &'static str,
    chain_max_in: Option<U256>,
    larger_input_reverted: bool,
    samples: Vec<Sample>,
}

struct PoolPlan {
    kind: u8,
    id: B256,
    block: u64,
    generation: u64,
    truncated: bool,
    tick_count: u32,
    bitmap: B256,
    factory: Option<Address>,
    identity_fee: Option<u32>,
    identity_spacing: Option<i32>,
    created_block: Option<u64>,
    canonical: Option<bool>,
    curve_tag: Option<u8>,
    curve_version: Option<u32>,
    skip_reason: Option<u8>,
    pairs_capped: bool,
    missing: Option<String>,
    detail: Option<PoolDetail>,
    directions: Vec<Direction>,
}

struct Prepared {
    family: Family,
    target: Address,
    method: &'static str,
    data: Vec<u8>,
    alt: Option<Vec<u8>>,
    alt_method: Option<&'static str>,
}

enum Prep {
    Ready(Prepared),
    Skip(String),
}

#[derive(Clone)]
struct RawCall {
    block: u64,
    family: Family,
    target: Address,
    data: Vec<u8>,
    method: &'static str,
}

struct RawOut {
    ok: bool,
    bytes: Vec<u8>,
    error: Option<String>,
}

#[derive(Clone)]
struct ChainOut {
    amount_out: Option<U256>,
    error: Option<String>,
}

struct Bound {
    pool: usize,
    dir: usize,
    lo: U256,
    hi: U256,
    lo_out: Option<U256>,
    steps: u8,
}

/// Copy stored quote samples and a fresh local quote into `out`.
///
/// The file is rewritten. Chain amounts are the samples the state pipeline
/// stored at the snapshot block. Pools with no quote row are still written,
/// with `error` set, so the extract lists the whole catalog.
pub fn export_curve_fit(catalog: &Catalog, out: &Path) -> eyre::Result<FitReport> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = out.with_extension("jsonl.partial");
    let mut file = File::create(&tmp)?;
    writeln!(file, "{}", serde_json::to_string(&header_value())?).map_err(io::Error::other)?;
    let mut report = FitReport::default();
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, PAGE)?;
            if page.is_empty() {
                break;
            }
            let full = page.len() == PAGE;
            after = page.last().map(|row| row.id);
            let started = Instant::now();
            for row in &page {
                let state = catalog.read_state(row.kind, row.id)?;
                let stored = state.quotes.clone();
                let mut plan = plan_of(row.kind, row.id, state);
                match stored {
                    Some(quote) => {
                        if let Err(err) = apply_stored(&mut plan, &quote) {
                            plan.missing = Some(err);
                        }
                    }
                    None if plan.missing.is_none() => {
                        plan.missing = Some("quotes not ingested".into());
                    }
                    None => {}
                }
                write_pool(&mut file, &plan, &mut report)?;
            }
            file.flush()?;
            file.sync_all()?;
            tracing::info!(
                kind = kind_name(kind),
                wrote = page.len(),
                ms = started.elapsed().as_millis() as u64,
                pools = report.pools,
                chain_ok = report.chain_ok,
                chain_fail = report.chain_fail,
                "fit export"
            );
            if !full {
                break;
            }
        }
    }
    std::fs::rename(&tmp, out)?;
    Ok(report)
}

/// Store quote rows from a JSONL extract. Lines already match a stored row
/// are counted in `resumed` and left in place. A partial last line is skipped.
pub fn import_curve_fit(catalog: &Catalog, path: &Path) -> eyre::Result<FitReport> {
    let text = std::fs::read_to_string(path)?;
    let mut report = FitReport::default();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("record").and_then(Value::as_str) != Some("pool") {
            continue;
        }
        let Some(record) = record_from_line(&value) else {
            report.chain_fail += 1;
            continue;
        };
        if let Some(stored) = catalog
            .read_state(record.kind, record.id)
            .ok()
            .and_then(|state| state.quotes)
        {
            if stored.generation == record.generation
                && stored.version == record.version
                && stored.block == record.block
                && stored.payload == record.payload
            {
                report.resumed += 1;
                continue;
            }
        }
        match catalog.put_quote(&record) {
            Ok(()) => {
                report.pools += 1;
                count_line(&value, &mut report);
            }
            Err(_) => report.chain_absent += 1,
        }
    }
    Ok(report)
}

pub(crate) async fn drive_quotes<S: EntityStore>(
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
                follow.push(curve_job(job.kind, job.id, mutable.generation));
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
            };
            quote_page(&rpc, &mut quoters, &mut plans)?;
            Ok::<_, eyre::Report>(plans)
        })
        .await
        {
            Ok(Ok(quoted)) => plans = quoted,
            Ok(Err(err)) => {
                let error = crate::rpc::redact_secret(&format!("{err:#}"));
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
        let record = QuoteRecord {
            kind: plan.kind,
            id: plan.id,
            generation,
            version: QUOTE_VERSION,
            block: plan.block,
            payload: payload_of(&plan),
        };
        if let Err(err) = store.put_quote(&record).await {
            keep_or_drop(
                job,
                err.to_string(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        if !curve_is_current(curve.as_ref(), generation) {
            follow.push(curve_job(job.kind, job.id, generation));
        }
        done.push((job.stage, job.kind, job.id));
    }
    store.finish_work(&done, &failed).await?;
    store.enqueue_work(&follow, unix_now()).await?;
    Ok(())
}

fn header_value() -> Value {
    json!({
        "record": "header",
        "schema": 1,
        "units": "Raw token amounts. Decimals are not stored. Do not assume 18.",
        "ground_truth": "chain_out is the protocol quoter at block, stored by the state pipeline before the curve is fit. local_out is recomputed from the stored snapshot when this file is written and is not ground truth.",
        "domain": "max_in is the top of the fit domain. cp_99 buys 99% of the output reserve. ladder is the last step of the concentrated book. balance_search bisects the local quote toward 99% of the output balance, capped at 1000x the input balance. balance is the input balance when that search cannot run. uninitialized and empty_book and none have no samples.",
        "grid": "Fractions of max_in: 0.0001, 0.001, 0.01, 0.05, 0.1, 0.25, 0.5, 0.8, 0.95, 1. A fraction that rounds to 0 is omitted. Two fractions that land on the same amount keep the larger fraction.",
        "chain_max": "If the fraction-1 quote reverts, an 8-step bisection records the largest accepted input. larger_input_reverted is then true. If fraction 1 is accepted, chain_max_in equals that input and larger_input_reverted is false: the domain top worked, and a constant-product pool still has no hard maximum.",
        "pairs": "Every ordered pair when the pool has at most 4 tokens. A larger pool keeps the first 4 and sets pairs_capped.",
        "resume": "The state pipeline is the source of the chain samples. This file is rewritten from the catalog. A repeated pool id is a torn line; keep the last copy.",
        "gaps": {
            "v2_fork": "No shared router. The Uniswap router hardcodes a 0.3% fee.",
            "v3_fork": "No confirmed quoter.",
            "aero": "No confirmed mainnet quoter. Slipstream quotes by tick spacing.",
            "metric": "No on-chain quoter. local_out is a marginal-price estimate."
        },
        "quoters": {
            "uniswap_v2": { "address": addr(UNI_V2_ROUTER), "method": "getAmountsOut" },
            "sushi_v2": { "address": addr(SUSHI_V2_ROUTER), "method": "getAmountsOut" },
            "pancake_v2": { "address": addr(PANCAKE_V2_ROUTER), "method": "getAmountsOut" },
            "uniswap_v3": { "address": addr(UNI_V3_QUOTER), "method": "quoteExactInputSingle" },
            "pancake_v3": { "address": addr(PANCAKE_V3_QUOTER), "method": "quoteExactInputSingle" },
            "sushi_v3": { "address": addr(SUSHI_V3_QUOTER), "method": "quoteExactInputSingle" },
            "uniswap_v4": { "address": addr(UNI_V4_QUOTER), "method": "quoteExactInputSingle" },
            "balancer_v2": { "method": "queryBatchSwap" },
            "balancer_v3": { "address": addr(BALANCER_V3_ROUTER), "method": "querySwapSingleTokenExactIn" },
            "curve": { "method": "get_dy", "int128": "0x5e0d443f", "uint256": "0x556d6e9f" },
            "multicall3": addr(MULTICALL3)
        }
    })
}

fn plan_of(kind: u8, id: B256, state: PoolState) -> PoolPlan {
    let (block, generation, truncated, tick_count, bitmap) = match &state.mutable {
        Some(row) => (
            row.block,
            row.generation,
            row.truncated,
            row.tick_count,
            row.bitmap,
        ),
        None => (0, 0, false, 0, B256::ZERO),
    };
    let (curve_tag, curve_version, skip_reason) = curve_meta(state.curve.as_ref());
    let venue = state.venue.clone();
    let mut plan = PoolPlan {
        kind,
        id,
        block,
        generation,
        truncated,
        tick_count,
        bitmap,
        factory: venue.as_ref().map(|row| row.factory),
        identity_fee: venue.as_ref().and_then(|row| row.fee),
        identity_spacing: venue.as_ref().and_then(|row| row.tick_spacing),
        created_block: venue.as_ref().and_then(|row| row.block_number),
        canonical: None,
        curve_tag,
        curve_version,
        skip_reason,
        pairs_capped: false,
        missing: None,
        detail: None,
        directions: Vec::new(),
    };
    let (immutable, mutable) = match (&state.immutable, &state.mutable) {
        (Some(immutable), Some(mutable)) => (immutable, mutable),
        _ => {
            plan.missing = Some("missing snapshot".into());
            return plan;
        }
    };
    let detail = match join_detail(immutable, &mutable.body, &state.ticks, kind, id) {
        Ok(detail) => detail,
        Err(err) => {
            plan.missing = Some(err);
            return plan;
        }
    };
    plan.canonical = Some(is_canonical(&detail));
    let (capped, pairs) = pairs_of(&detail);
    plan.pairs_capped = capped;
    plan.directions = pairs
        .into_iter()
        .map(|(token_in, token_out)| direction_of(&detail, token_in, token_out))
        .collect();
    plan.detail = Some(detail);
    plan
}

fn curve_meta(curve: Option<&CurveRecord>) -> (Option<u8>, Option<u32>, Option<u8>) {
    let Some(curve) = curve else {
        return (None, None, None);
    };
    let tag = curve.payload.first().copied();
    let skip = if tag == Some(0) {
        Some(curve.payload.get(1).copied().unwrap_or(0))
    } else {
        None
    };
    (tag, Some(curve.version), skip)
}

fn pairs_of(detail: &PoolDetail) -> (bool, Vec<(Address, Address)>) {
    let mut tokens = legs(detail);
    let capped = tokens.len() > MAX_LEGS;
    tokens.truncate(MAX_LEGS);
    let mut pairs = Vec::new();
    for (i, token_in) in tokens.iter().enumerate() {
        for (j, token_out) in tokens.iter().enumerate() {
            if i != j {
                pairs.push((*token_in, *token_out));
            }
        }
    }
    (capped, pairs)
}

fn legs(detail: &PoolDetail) -> Vec<Address> {
    match detail {
        PoolDetail::ConstantProduct(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Concentrated(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Balancer(pool) => pool.tokens.clone(),
        PoolDetail::Curve(pool) => pool.coins.iter().map(|coin| coin.token).collect(),
        PoolDetail::Metric(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Failed(_) => Vec::new(),
    }
}

fn direction_of(detail: &PoolDetail, token_in: Address, token_out: Address) -> Direction {
    let (max_in, max_source) = domain_max(detail, token_in, token_out);
    let mut samples = Vec::new();
    if !max_in.is_zero() {
        for (fraction, amount) in grid_amounts(max_in) {
            samples.push(sample_at(
                detail,
                token_in,
                token_out,
                "grid",
                Some(fraction),
                amount,
            ));
        }
    }
    Direction {
        token_in,
        token_out,
        max_in,
        max_source,
        chain_max_in: None,
        larger_input_reverted: false,
        samples,
    }
}

fn sample_at(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    role: &'static str,
    fraction: Option<&'static str>,
    amount_in: U256,
) -> Sample {
    let mut sample = Sample {
        role,
        fraction,
        amount_in,
        local_out: None,
        local_fee: None,
        local_exhausted: None,
        local_error: None,
        chain_out: None,
        chain_error: None,
        quoter: None,
        method: None,
        attempted: false,
        alt: None,
        alt_method: None,
    };
    match detail.quote(token_in, token_out, amount_in) {
        Ok(quote) => fill_local(&mut sample, &quote),
        Err(err) => sample.local_error = Some(err),
    }
    sample
}

fn fill_local(sample: &mut Sample, quote: &Quote) {
    sample.local_out = Some(quote.amount_out);
    sample.local_fee = Some(quote.fee_paid);
    sample.local_exhausted = Some(quote.exhausted);
}

fn domain_max(detail: &PoolDetail, token_in: Address, token_out: Address) -> (U256, &'static str) {
    match detail {
        PoolDetail::ConstantProduct(pool) => cp_max(pool, token_in, token_out),
        PoolDetail::Concentrated(pool) => {
            let zero_for_one = token_in == pool.token0 && token_out == pool.token1;
            let one_for_zero = token_in == pool.token1 && token_out == pool.token0;
            if !zero_for_one && !one_for_zero {
                return (U256::ZERO, "none");
            }
            concentrated_max(pool, zero_for_one)
        }
        PoolDetail::Balancer(_) | PoolDetail::Curve(_) => {
            let Some((balance_in, balance_out)) = balances(detail, token_in, token_out) else {
                return (U256::ZERO, "none");
            };
            search_output(detail, token_in, token_out, balance_in, balance_out)
        }
        PoolDetail::Metric(pool) => metric_max(detail, pool, token_in, token_out),
        PoolDetail::Failed(_) => (U256::ZERO, "none"),
    }
}

fn cp_max(
    pool: &crate::detail::ConstantProduct,
    token_in: Address,
    token_out: Address,
) -> (U256, &'static str) {
    let (reserve_in, reserve_out) = if token_in == pool.token0 && token_out == pool.token1 {
        (pool.reserve0, pool.reserve1)
    } else if token_in == pool.token1 && token_out == pool.token0 {
        (pool.reserve1, pool.reserve0)
    } else {
        return (U256::ZERO, "none");
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return (U256::ZERO, "none");
    }
    let fee = product_fee_pips(pool.kind, pool.fee_pips);
    if fee >= 1_000_000 {
        return (U256::ZERO, "none");
    }
    let den = U256::from(1_000_000u32 - fee);
    let Some(scaled) = reserve_in
        .checked_mul(U256::from(99u32))
        .and_then(|value| value.checked_mul(U256::from(1_000_000u32)))
    else {
        return (U256::ZERO, "overflow");
    };
    ((scaled + den - U256::from(1u8)) / den, "cp_99")
}

fn concentrated_max(
    pool: &crate::detail::Concentrated,
    zero_for_one: bool,
) -> (U256, &'static str) {
    if pool.sqrt_price_x96.is_zero() {
        return (U256::ZERO, "uninitialized");
    }
    let Ok(ladder) = crate::detail::concentrated_ladder(pool) else {
        return (U256::ZERO, "uninitialized");
    };
    let steps = if zero_for_one {
        &ladder.zero_for_one
    } else {
        &ladder.one_for_zero
    };
    match steps.last() {
        Some(step) if !step.input.is_zero() => (step.input, "ladder"),
        _ => (U256::ZERO, "empty_book"),
    }
}

fn balances(detail: &PoolDetail, token_in: Address, token_out: Address) -> Option<(U256, U256)> {
    match detail {
        PoolDetail::Balancer(pool) => {
            let input = pool.tokens.iter().position(|token| *token == token_in)?;
            let output = pool.tokens.iter().position(|token| *token == token_out)?;
            Some((*pool.balances.get(input)?, *pool.balances.get(output)?))
        }
        PoolDetail::Curve(pool) => {
            let input = pool.coins.iter().position(|coin| coin.token == token_in)?;
            let output = pool.coins.iter().position(|coin| coin.token == token_out)?;
            Some((pool.coins[input].balance, pool.coins[output].balance))
        }
        _ => None,
    }
}

fn search_output(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    balance_in: U256,
    balance_out: U256,
) -> (U256, &'static str) {
    if balance_in.is_zero() || balance_out.is_zero() {
        return (U256::ZERO, "none");
    }
    if detail.quote(token_in, token_out, U256::from(1u8)).is_err() {
        return (balance_in, "balance");
    }
    let cap = balance_in.saturating_mul(U256::from(1000u32));
    let target = balance_out.saturating_mul(U256::from(99u8)) / U256::from(100u8);
    let found = bisect_local(detail, token_in, token_out, cap, |quote| {
        quote.exhausted || (!target.is_zero() && quote.amount_out >= target)
    });
    match found {
        Some(value) => (value, "balance_search"),
        None => (cap, "balance"),
    }
}

fn metric_max(
    detail: &PoolDetail,
    pool: &crate::detail::MetricPool,
    token_in: Address,
    token_out: Address,
) -> (U256, &'static str) {
    let selling_token0 = token_in == pool.token0 && token_out == pool.token1;
    let selling_token1 = token_in == pool.token1 && token_out == pool.token0;
    if !selling_token0 && !selling_token1 {
        return (U256::ZERO, "none");
    }
    if detail.quote(token_in, token_out, U256::from(1u8)).is_err() {
        return (U256::ZERO, "none");
    }
    let bin = pool.bins.iter().find(|bin| bin.index == pool.bin_index);
    let (scale, scaled) = if selling_token0 {
        (pool.scale0, bin.map(|bin| bin.token0_scaled).unwrap_or(0))
    } else {
        (pool.scale1, bin.map(|bin| bin.token1_scaled).unwrap_or(0))
    };
    let base = if scaled == 0 || scale.is_zero() {
        U256::from(1_000_000_000_000_000_000u64)
    } else {
        U256::from(scaled) / scale
    };
    let base = if base.is_zero() {
        U256::from(1u8)
    } else {
        base
    };
    let cap = base.saturating_mul(U256::from(1000u32));
    match bisect_local(detail, token_in, token_out, cap, |quote| quote.exhausted) {
        Some(value) => (value, "balance_search"),
        None => (cap, "balance"),
    }
}

fn bisect_local(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    cap: U256,
    done: impl Fn(&Quote) -> bool,
) -> Option<U256> {
    if cap.is_zero() {
        return None;
    }
    let mut lo = U256::from(1u8);
    let mut hi = cap;
    let mut best = None;
    for _ in 0..256 {
        if lo >= hi {
            break;
        }
        let mid = lo + (hi - lo) / U256::from(2u8);
        if mid.is_zero() {
            break;
        }
        match detail.quote(token_in, token_out, mid) {
            Ok(quote) if done(&quote) => {
                best = Some(mid);
                hi = mid;
            }
            Ok(_) => lo = mid.saturating_add(U256::from(1u8)),
            Err(_) => {
                if mid <= lo {
                    break;
                }
                hi = mid;
            }
        }
    }
    best
}

fn grid_amounts(max_in: U256) -> Vec<(&'static str, U256)> {
    let mut out: Vec<(&'static str, U256)> = Vec::new();
    for &(fraction, num, den) in GRID {
        let amount = match max_in.checked_mul(U256::from(num)) {
            Some(value) => value / U256::from(den),
            None => max_in,
        };
        if amount.is_zero() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.1 == amount {
                last.0 = fraction;
                continue;
            }
        }
        out.push((fraction, amount));
    }
    out
}

fn quote_page(rpc: &Rpc, quoters: &mut Quoters, plans: &mut [PoolPlan]) -> eyre::Result<()> {
    let mut calls = Vec::new();
    let mut back: Vec<(usize, usize, usize)> = Vec::new();
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
                    rpc,
                    quoters,
                )? {
                    Prep::Skip(reason) => sample.chain_error = Some(reason),
                    Prep::Ready(prepared) => {
                        sample.quoter = Some(prepared.target);
                        sample.method = Some(prepared.method);
                        sample.alt = prepared.alt;
                        sample.alt_method = prepared.alt_method;
                        sample.attempted = true;
                        calls.push(RawCall {
                            block,
                            family: prepared.family,
                            target: prepared.target,
                            data: prepared.data,
                            method: prepared.method,
                        });
                        back.push((pool_index, dir_index, sample_index));
                    }
                }
            }
        }
        plan.detail = Some(detail);
    }
    apply_calls(rpc, plans, &calls, &back)?;
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
                    family: Family::Curve,
                    target,
                    data,
                    method,
                });
                back.push((pool_index, dir_index, sample_index));
            }
        }
    }
    apply_calls(rpc, plans, &calls, &back)?;
    bisect(rpc, quoters, plans)
}

fn apply_calls(
    rpc: &Rpc,
    plans: &mut [PoolPlan],
    calls: &[RawCall],
    back: &[(usize, usize, usize)],
) -> eyre::Result<()> {
    let outs = dispatch_raw(rpc, calls)?;
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

fn bisect(rpc: &Rpc, quoters: &mut Quoters, plans: &mut [PoolPlan]) -> eyre::Result<()> {
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
                prepare(
                    detail,
                    dir.token_in,
                    dir.token_out,
                    mid,
                    plan.block,
                    rpc,
                    quoters,
                )?
            };
            let Prep::Ready(prepared) = prepared else {
                bounds[index].hi = mid;
                bounds[index].steps = bounds[index].steps.saturating_add(1);
                continue;
            };
            let block = plans[pool].block;
            which.push((index, mid));
            calls.push(RawCall {
                block,
                family: prepared.family,
                target: prepared.target,
                data: prepared.data,
                method: prepared.method,
            });
        }
        if calls.is_empty() {
            break;
        }
        let outs = dispatch_raw(rpc, &calls)?;
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

fn prepare(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    amount: U256,
    block: u64,
    rpc: &Rpc,
    quoters: &mut Quoters,
) -> eyre::Result<Prep> {
    if block == 0 {
        return Ok(Prep::Skip("snapshot block is 0".into()));
    }
    let built = match detail {
        PoolDetail::ConstantProduct(pool) => {
            prepare_product(pool.kind, token_in, token_out, amount)
        }
        PoolDetail::Concentrated(pool) => prepare_concentrated(pool, token_in, token_out, amount),
        PoolDetail::Balancer(pool) => prepare_balancer(pool, token_in, token_out, amount),
        PoolDetail::Curve(pool) => prepare_curve(pool, token_in, token_out, amount),
        PoolDetail::Metric(_) => Err("metric has no on-chain quoter".into()),
        PoolDetail::Failed(_) => Err("pool read failed".into()),
    };
    let prepared = match built {
        Ok(prepared) => prepared,
        Err(reason) => return Ok(Prep::Skip(reason)),
    };
    if !quoters.has_code(rpc, prepared.target, block)? {
        return Ok(Prep::Skip("quoter has no code at this block".into()));
    }
    Ok(Prep::Ready(prepared))
}

fn prepare_product(
    kind: u8,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    let target = match kind {
        UNI_V2 => UNI_V2_ROUTER,
        SUSHI_V2 => SUSHI_V2_ROUTER,
        PANCAKE_V2 => PANCAKE_V2_ROUTER,
        V2_FORK => {
            return Err("no shared quoter for a v2 fork; fee is per pair".into());
        }
        _ => return Err("no v2 quoter".into()),
    };
    Ok(ready(
        Family::V2,
        target,
        "getAmountsOut",
        encode_amounts_out(amount, token_in, token_out),
    ))
}

fn prepare_concentrated(
    pool: &crate::detail::Concentrated,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    let zero_for_one = if token_in == pool.token0 && token_out == pool.token1 {
        true
    } else if token_in == pool.token1 && token_out == pool.token0 {
        false
    } else {
        return Err("token".into());
    };
    match pool.kind {
        UNI_V3 | PANCAKE_V3 | SUSHI_V3 => {
            let target = match pool.kind {
                UNI_V3 => UNI_V3_QUOTER,
                PANCAKE_V3 => PANCAKE_V3_QUOTER,
                _ => SUSHI_V3_QUOTER,
            };
            let fee = pool.fee_pips.ok_or_else(|| "fee unread".to_string())?;
            Ok(ready(
                Family::V3,
                target,
                "quoteExactInputSingle",
                encode_v3_quote(token_in, token_out, amount, fee),
            ))
        }
        UNI_V4 => {
            let fee = pool.fee_pips.ok_or_else(|| "fee unread".to_string())?;
            if !fits_i24(pool.tick_spacing) {
                return Err("tick spacing".into());
            }
            if amount > U256::from(u128::MAX) {
                return Err("amount exceeds uint128".into());
            }
            Ok(ready(
                Family::V4,
                UNI_V4_QUOTER,
                "quoteExactInputSingle",
                encode_v4_quote(
                    pool.token0,
                    pool.token1,
                    fee,
                    pool.tick_spacing,
                    pool.hooks,
                    zero_for_one,
                    amount,
                ),
            ))
        }
        V3_FORK => Err("no confirmed quoter for this v3 fork".into()),
        AERO => Err("no confirmed quoter for aerodrome on mainnet".into()),
        _ => Err("no concentrated quoter".into()),
    }
}

fn prepare_balancer(
    pool: &crate::detail::BalancerPool,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    match pool.kind {
        BALANCER_V2 => Ok(ready(
            Family::Bal2,
            pool.vault,
            "queryBatchSwap",
            encode_bal2(pool.id, token_in, token_out, amount),
        )),
        BALANCER_V3 => Ok(ready(
            Family::Bal3,
            BALANCER_V3_ROUTER,
            "querySwapSingleTokenExactIn",
            encode_bal3(pool.pool, token_in, token_out, amount),
        )),
        _ => Err("no balancer quoter".into()),
    }
}

fn prepare_curve(
    pool: &crate::detail::CurvePool,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    let input = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_in)
        .ok_or("token")?;
    let output = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_out)
        .ok_or("token")?;
    let preferred_uint = pool.coins_indexed_by_uint;
    let mut prepared = ready(
        Family::Curve,
        pool.pool,
        if preferred_uint {
            "get_dy_uint256"
        } else {
            "get_dy_int128"
        },
        encode_get_dy(preferred_uint, input as u64, output as u64, amount),
    );
    prepared.alt = Some(encode_get_dy(
        !preferred_uint,
        input as u64,
        output as u64,
        amount,
    ));
    prepared.alt_method = Some(if preferred_uint {
        "get_dy_int128"
    } else {
        "get_dy_uint256"
    });
    Ok(prepared)
}

fn ready(family: Family, target: Address, method: &'static str, data: Vec<u8>) -> Prepared {
    Prepared {
        family,
        target,
        method,
        data,
        alt: None,
        alt_method: None,
    }
}

fn fits_i24(value: i32) -> bool {
    (-8_388_608..8_388_608).contains(&value)
}

fn dispatch_raw(rpc: &Rpc, calls: &[RawCall]) -> eyre::Result<Vec<ChainOut>> {
    let mut order: BTreeMap<(u64, Family), Vec<usize>> = BTreeMap::new();
    for (index, call) in calls.iter().enumerate() {
        order
            .entry((call.block, call.family))
            .or_default()
            .push(index);
    }
    let mut outs = vec![
        ChainOut {
            amount_out: None,
            error: Some("unrun".into()),
        };
        calls.len()
    ];
    for ((block, family), indices) in order {
        let (size, gas) = batch_of(family);
        for chunk in indices.chunks(size) {
            let prepared: Vec<(Address, &[u8])> = chunk
                .iter()
                .map(|index| (calls[*index].target, calls[*index].data.as_slice()))
                .collect();
            let raws = execute(rpc, Some(block), &prepared, gas)?;
            for (index, raw) in chunk.iter().zip(raws) {
                outs[*index] = interpret(calls[*index].method, raw);
            }
        }
    }
    Ok(outs)
}

fn execute(
    rpc: &Rpc,
    block: Option<u64>,
    calls: &[(Address, &[u8])],
    gas: u64,
) -> eyre::Result<Vec<RawOut>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let data = encode_aggregate3(calls);
    let call = rpc.eth_call_at(&data, Some(MULTICALL3), gas, block)?;
    if call.reverted {
        return split_or_fail(rpc, block, calls, gas, &call.message);
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
        _ => split_or_fail(rpc, block, calls, gas, "aggregate decode"),
    }
}

fn split_or_fail(
    rpc: &Rpc,
    block: Option<u64>,
    calls: &[(Address, &[u8])],
    gas: u64,
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
    let mut outs = execute(rpc, block, &calls[..mid], gas)?;
    outs.extend(execute(rpc, block, &calls[mid..], gas)?);
    Ok(outs)
}

fn interpret(method: &str, raw: RawOut) -> ChainOut {
    if let Some(error) = raw.error {
        return ChainOut {
            amount_out: None,
            error: Some(error),
        };
    }
    if !raw.ok {
        return ChainOut {
            amount_out: None,
            error: Some(revert_text(&raw.bytes)),
        };
    }
    let amount = match method {
        "getAmountsOut" => decode_uint_array(&raw.bytes).and_then(|words| words.get(1).copied()),
        "queryBatchSwap" => decode_negative(&raw.bytes, 1),
        _ => first_word(&raw.bytes),
    };
    match amount {
        Some(amount_out) => ChainOut {
            amount_out: Some(amount_out),
            error: None,
        },
        None => ChainOut {
            amount_out: None,
            error: Some("return".into()),
        },
    }
}

fn first_word(bytes: &[u8]) -> Option<U256> {
    let word = bytes.get(..32)?;
    Some(U256::from_be_slice(word))
}

fn decode_uint_array(data: &[u8]) -> Option<Vec<U256>> {
    let offset = as_usize(read_word(data, 0)?)?;
    let len = as_usize(read_word(data, offset)?)?;
    let start = offset.checked_add(32)?;
    let mut words = Vec::with_capacity(len);
    for index in 0..len {
        let at = start.checked_add(index.checked_mul(32)?)?;
        words.push(read_word(data, at)?);
    }
    Some(words)
}

fn decode_negative(data: &[u8], index: usize) -> Option<U256> {
    let words = decode_uint_array(data)?;
    let word = *words.get(index)?;
    let bytes = word.to_be_bytes::<32>();
    if bytes[0] & 0x80 == 0 {
        return None;
    }
    Some((!word).saturating_add(U256::from(1u8)))
}

fn decode_aggregate(data: &[u8]) -> Option<Vec<(bool, Vec<u8>)>> {
    let base = as_usize(read_word(data, 0)?)?;
    let len = as_usize(read_word(data, base)?)?;
    let table = base.checked_add(32)?;
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let rel = as_usize(read_word(data, table + index * 32)?)?;
        let element = table.checked_add(rel)?;
        let success = !read_word(data, element)?.is_zero();
        let bytes_rel = as_usize(read_word(data, element + 32)?)?;
        let bytes_at = element.checked_add(bytes_rel)?;
        let n = as_usize(read_word(data, bytes_at)?)?;
        let start = bytes_at.checked_add(32)?;
        let end = start.checked_add(n)?;
        out.push((success, data.get(start..end)?.to_vec()));
    }
    Some(out)
}

fn read_word(data: &[u8], at: usize) -> Option<U256> {
    let end = at.checked_add(32)?;
    Some(U256::from_be_slice(data.get(at..end)?))
}

fn as_usize(value: U256) -> Option<usize> {
    if value > U256::from(usize::MAX) {
        return None;
    }
    Some(value.to::<usize>())
}

fn revert_text(data: &[u8]) -> String {
    if data.len() >= 68 && data.starts_with(&[0x08, 0xc3, 0x79, 0xa0]) {
        if let Some(len) = as_usize(read_word(data, 36).unwrap_or(U256::ZERO)) {
            let start = 68usize;
            let end = start.saturating_add(len).min(data.len());
            if let Ok(text) = std::str::from_utf8(&data[start..end]) {
                return clip(text);
            }
        }
    }
    if data.is_empty() {
        return "reverted".into();
    }
    clip(&format!(
        "reverted {}",
        hex_prefix(&data[..data.len().min(32)])
    ))
}

fn clip(text: &str) -> String {
    let text = if text.contains("/v2/") {
        "rpc error"
    } else {
        text
    };
    text.chars().take(160).collect()
}

fn payload_of(plan: &PoolPlan) -> Vec<u8> {
    let body = json!({
        "pairs_capped": plan.pairs_capped,
        "directions": plan.directions.iter().map(stored_direction).collect::<Vec<_>>(),
    });
    serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec())
}

fn stored_direction(dir: &Direction) -> Value {
    json!({
        "token_in": addr(dir.token_in),
        "token_out": addr(dir.token_out),
        "max_in": dir.max_in.to_string(),
        "max_source": dir.max_source,
        "chain_max_in": dir.chain_max_in.map(|value| value.to_string()),
        "larger_input_reverted": dir.larger_input_reverted,
        "samples": dir.samples.iter().map(stored_sample).collect::<Vec<_>>(),
    })
}

fn stored_sample(sample: &Sample) -> Value {
    json!({
        "role": sample.role,
        "fraction": sample.fraction,
        "amount_in": sample.amount_in.to_string(),
        "chain_out": sample.chain_out.map(|value| value.to_string()),
        "chain_error": sample.chain_error,
        "quoter": sample.quoter.map(addr),
        "method": sample.method,
    })
}

fn apply_stored(plan: &mut PoolPlan, quote: &QuoteRecord) -> Result<(), String> {
    if quote.block != plan.block
        || quote.generation != plan.generation
        || quote.version != QUOTE_VERSION
    {
        return Err("stored quotes do not match the snapshot".into());
    }
    let value: Value = serde_json::from_slice(&quote.payload).map_err(|err| err.to_string())?;
    let Some(detail) = plan.detail.clone() else {
        return Err("missing snapshot".into());
    };
    let directions = value
        .get("directions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for stored in directions {
        let token_in = parse_address(stored.get("token_in"))?;
        let token_out = parse_address(stored.get("token_out"))?;
        let Some(dir_index) = plan
            .directions
            .iter()
            .position(|dir| dir.token_in == token_in && dir.token_out == token_out)
        else {
            continue;
        };
        plan.directions[dir_index].larger_input_reverted = stored
            .get("larger_input_reverted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        plan.directions[dir_index].chain_max_in = optional_u256(stored.get("chain_max_in"))?;
        let samples = stored
            .get("samples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for sample in samples {
            let amount = required_u256(sample.get("amount_in"))?;
            let role = sample.get("role").and_then(Value::as_str).unwrap_or("grid");
            if let Some(slot) = plan.directions[dir_index]
                .samples
                .iter_mut()
                .find(|slot| slot.amount_in == amount)
            {
                fill_chain(slot, &sample);
                continue;
            }
            if role != "chain_max" {
                continue;
            }
            let mut extra = sample_at(&detail, token_in, token_out, "chain_max", None, amount);
            fill_chain(&mut extra, &sample);
            plan.directions[dir_index].samples.push(extra);
        }
    }
    Ok(())
}

fn fill_chain(slot: &mut Sample, sample: &Value) {
    slot.chain_out = optional_u256(sample.get("chain_out")).ok().flatten();
    slot.chain_error = sample
        .get("chain_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    slot.quoter = sample
        .get("quoter")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok());
    slot.method = method_static(sample.get("method").and_then(Value::as_str));
    slot.attempted = slot.chain_out.is_none() && slot.quoter.is_some();
}

fn method_static(name: Option<&str>) -> Option<&'static str> {
    match name {
        Some("getAmountsOut") => Some("getAmountsOut"),
        Some("quoteExactInputSingle") => Some("quoteExactInputSingle"),
        Some("get_dy_int128") => Some("get_dy_int128"),
        Some("get_dy_uint256") => Some("get_dy_uint256"),
        Some("queryBatchSwap") => Some("queryBatchSwap"),
        Some("querySwapSingleTokenExactIn") => Some("querySwapSingleTokenExactIn"),
        _ => None,
    }
}

fn record_from_line(value: &Value) -> Option<QuoteRecord> {
    let kind = u8::try_from(value.get("kind_id").and_then(Value::as_u64)?).ok()?;
    let id = value.get("id").and_then(Value::as_str)?.parse().ok()?;
    let block = value.get("block").and_then(Value::as_u64)?;
    let generation = value.get("generation").and_then(Value::as_u64)?;
    let pairs_capped = value
        .get("pairs_capped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let directions = value.get("directions")?.as_array()?.clone();
    let stripped: Vec<_> = directions.iter().map(strip_direction).collect();
    let payload = serde_json::to_vec(&json!({
        "pairs_capped": pairs_capped,
        "directions": stripped,
    }))
    .ok()?;
    Some(QuoteRecord {
        kind,
        id,
        generation,
        version: QUOTE_VERSION,
        block,
        payload,
    })
}

fn strip_direction(value: &Value) -> Value {
    let samples = value
        .get("samples")
        .and_then(Value::as_array)
        .map(|samples| samples.iter().map(strip_sample).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({
        "token_in": value.get("token_in").and_then(Value::as_str).unwrap_or(""),
        "token_out": value.get("token_out").and_then(Value::as_str).unwrap_or(""),
        "max_in": value.get("max_in").and_then(Value::as_str).unwrap_or("0"),
        "max_source": value.get("max_source").and_then(Value::as_str).unwrap_or("none"),
        "chain_max_in": value.get("chain_max_in").and_then(Value::as_str),
        "larger_input_reverted": value.get("larger_input_reverted").and_then(Value::as_bool).unwrap_or(false),
        "samples": samples,
    })
}

fn strip_sample(value: &Value) -> Value {
    json!({
        "role": value.get("role").and_then(Value::as_str).unwrap_or("grid"),
        "fraction": value.get("fraction"),
        "amount_in": value.get("amount_in").and_then(Value::as_str).unwrap_or("0"),
        "chain_out": value.get("chain_out").and_then(Value::as_str),
        "chain_error": value.get("chain_error").and_then(Value::as_str),
        "quoter": value.get("quoter").and_then(Value::as_str),
        "method": value.get("method").and_then(Value::as_str),
    })
}

fn count_line(value: &Value, report: &mut FitReport) {
    let Some(directions) = value.get("directions").and_then(Value::as_array) else {
        return;
    };
    for dir in directions {
        let Some(samples) = dir.get("samples").and_then(Value::as_array) else {
            continue;
        };
        for sample in samples {
            if sample.get("chain_out").and_then(Value::as_str).is_some() {
                report.chain_ok += 1;
            } else if sample.get("quoter").and_then(Value::as_str).is_some() {
                report.chain_fail += 1;
            } else if sample.get("chain_error").and_then(Value::as_str).is_some() {
                report.chain_absent += 1;
            }
        }
    }
}

fn parse_address(value: Option<&Value>) -> Result<Address, String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| "address".to_string())?
        .parse()
        .map_err(|_| "address".to_string())
}

fn required_u256(value: Option<&Value>) -> Result<U256, String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| "amount".to_string())?
        .parse()
        .map_err(|_| "amount".to_string())
}

fn optional_u256(value: Option<&Value>) -> Result<Option<U256>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(required_u256(Some(value))?)),
    }
}

fn write_pool(file: &mut File, plan: &PoolPlan, report: &mut FitReport) -> io::Result<()> {
    let line = serde_json::to_string(&pool_json(plan)).map_err(io::Error::other)?;
    writeln!(file, "{line}")?;
    report.pools += 1;
    for dir in &plan.directions {
        for sample in &dir.samples {
            if sample.chain_out.is_some() {
                report.chain_ok += 1;
            } else if sample.attempted {
                report.chain_fail += 1;
            } else if sample.chain_error.is_some() {
                report.chain_absent += 1;
            }
        }
    }
    Ok(())
}

fn pool_json(plan: &PoolPlan) -> Value {
    json!({
        "record": "pool",
        "schema": 1,
        "kind": kind_name(plan.kind),
        "kind_id": plan.kind,
        "id": format!("{}", plan.id),
        "block": plan.block,
        "generation": plan.generation,
        "factory": plan.factory.map(addr),
        "identity_fee": plan.identity_fee,
        "identity_tick_spacing": plan.identity_spacing,
        "created_block": plan.created_block,
        "canonical": plan.canonical,
        "curve_tag": plan.curve_tag,
        "curve_version": plan.curve_version,
        "skip_reason": plan.skip_reason,
        "ticks_truncated": plan.truncated,
        "tick_count": plan.tick_count,
        "bitmap_fingerprint": format!("{}", plan.bitmap),
        "pairs_capped": plan.pairs_capped,
        "error": plan.missing,
        "state": plan.detail.as_ref().map(state_json),
        "directions": plan.directions.iter().map(direction_json).collect::<Vec<_>>(),
    })
}

fn direction_json(dir: &Direction) -> Value {
    json!({
        "token_in": addr(dir.token_in),
        "token_out": addr(dir.token_out),
        "max_in": dir.max_in.to_string(),
        "max_source": dir.max_source,
        "chain_max_in": dir.chain_max_in.map(|value| value.to_string()),
        "larger_input_reverted": dir.larger_input_reverted,
        "samples": dir.samples.iter().map(sample_json).collect::<Vec<_>>(),
    })
}

fn sample_json(sample: &Sample) -> Value {
    json!({
        "role": sample.role,
        "fraction": sample.fraction,
        "amount_in": sample.amount_in.to_string(),
        "local_out": sample.local_out.map(|value| value.to_string()),
        "local_fee": sample.local_fee.map(|value| value.to_string()),
        "local_exhausted": sample.local_exhausted,
        "local_error": sample.local_error,
        "chain_out": sample.chain_out.map(|value| value.to_string()),
        "chain_error": sample.chain_error,
        "quoter": sample.quoter.map(addr),
        "method": sample.method,
    })
}

fn state_json(detail: &PoolDetail) -> Value {
    match detail {
        PoolDetail::ConstantProduct(pool) => json!({
            "shape": "constant_product",
            "pair": addr(pool.pair),
            "token0": addr(pool.token0),
            "token1": addr(pool.token1),
            "reserve0": pool.reserve0.to_string(),
            "reserve1": pool.reserve1.to_string(),
            "fee_pips_stored": pool.fee_pips,
            "fee_pips_used": product_fee_pips(pool.kind, pool.fee_pips),
            "timestamp": pool.timestamp,
            "flags": pool.flags,
        }),
        PoolDetail::Concentrated(pool) => json!({
            "shape": "concentrated",
            "home": addr(pool.home),
            "hooks": addr(pool.hooks),
            "token0": addr(pool.token0),
            "token1": addr(pool.token1),
            "fee_pips": pool.fee_pips,
            "protocol_fee": pool.protocol_fee,
            "tick_spacing": pool.tick_spacing,
            "sqrt_price_x96": pool.sqrt_price_x96.to_string(),
            "tick": pool.tick,
            "liquidity": pool.liquidity.to_string(),
            "flags": pool.flags,
            "ticks": pool.ticks.iter().map(|tick| json!({
                "tick": tick.tick,
                "liquidity_gross": tick.liquidity_gross.to_string(),
                "liquidity_net": tick.liquidity_net.to_string(),
            })).collect::<Vec<_>>(),
        }),
        PoolDetail::Balancer(pool) => json!({
            "shape": "balancer",
            "vault": addr(pool.vault),
            "pool": addr(pool.pool),
            "fee": opt_u256(pool.fee),
            "amp": pool.amp.to_string(),
            "amp_precision": pool.amp_precision.to_string(),
            "tokens": pool.tokens.iter().copied().map(addr).collect::<Vec<_>>(),
            "balances": pool.balances.iter().map(|value| value.to_string()).collect::<Vec<_>>(),
            "weights": pool.weights.iter().map(|value| value.to_string()).collect::<Vec<_>>(),
            "stable": pool.flags & crate::detail::FLAG_STABLE != 0,
            "flags": pool.flags,
        }),
        PoolDetail::Curve(pool) => json!({
            "shape": "curve",
            "pool": addr(pool.pool),
            "amp": pool.amp.to_string(),
            "fee": opt_u256(pool.fee),
            "gamma": pool.gamma.to_string(),
            "d": pool.d.to_string(),
            "mid_fee": pool.mid_fee.to_string(),
            "out_fee": pool.out_fee.to_string(),
            "fee_gamma": pool.fee_gamma.to_string(),
            "coins_indexed_by_uint": pool.coins_indexed_by_uint,
            "crypto": pool.flags & crate::detail::FLAG_CRYPTO != 0,
            "flags": pool.flags,
            "coins": pool.coins.iter().map(|coin| json!({
                "token": addr(coin.token),
                "balance": coin.balance.to_string(),
                "rate": coin.rate.to_string(),
                "price_scale": coin.price_scale.to_string(),
            })).collect::<Vec<_>>(),
        }),
        PoolDetail::Metric(pool) => json!({
            "shape": "metric",
            "pool": addr(pool.pool),
            "pause_level": pool.pause_level,
            "bin_index": pool.bin_index,
            "position": pool.position.to_string(),
            "distance_e6": pool.distance_e6,
            "spread_fee_e6": pool.spread_fee_e6,
            "notional_fee_e8": pool.notional_fee_e8,
            "asymmetry_x8": pool.asymmetry_x8,
            "scale0": pool.scale0.to_string(),
            "scale1": pool.scale1.to_string(),
            "lowest_bin": pool.lowest_bin,
            "highest_bin": pool.highest_bin,
            "token0": addr(pool.token0),
            "token1": addr(pool.token1),
            "reference_x64": pool.reference_x64.to_string(),
            "provider": addr(pool.provider),
            "flags": pool.flags,
            "bins": pool.bins.iter().map(|bin| json!({
                "index": bin.index,
                "token0_scaled": bin.token0_scaled.to_string(),
                "token1_scaled": bin.token1_scaled.to_string(),
                "length_e6": bin.length_e6,
                "add_fee_buy_e6": bin.add_fee_buy_e6,
                "add_fee_sell_e6": bin.add_fee_sell_e6,
            })).collect::<Vec<_>>(),
        }),
        PoolDetail::Failed(pool) => json!({
            "shape": "failed",
            "home": addr(pool.home),
            "flags": pool.flags,
        }),
    }
}

fn opt_u256(value: Option<U256>) -> Value {
    match value {
        Some(value) => Value::String(value.to_string()),
        None => Value::Null,
    }
}

fn addr(value: Address) -> String {
    format!("{value}")
}

fn push_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

fn push_addr(out: &mut Vec<u8>, value: Address) {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_slice());
    out.extend_from_slice(&word);
}

fn push_bool(out: &mut Vec<u8>, value: bool) {
    push_u256(out, U256::from(u8::from(value)));
}

fn push_i32_word(out: &mut Vec<u8>, value: i32) {
    let mut word = if value < 0 { [0xffu8; 32] } else { [0u8; 32] };
    word[28..].copy_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&word);
}

fn push_bytes_tail(out: &mut Vec<u8>, data: &[u8]) {
    push_u256(out, U256::from(data.len()));
    out.extend_from_slice(data);
    let pad = (32 - (data.len() % 32)) % 32;
    out.extend(std::iter::repeat_n(0u8, pad));
}

fn encode_amounts_out(amount: U256, token_in: Address, token_out: Address) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_GET_AMOUNTS);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(0x40u64));
    push_u256(&mut out, U256::from(2u8));
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    out
}

fn encode_v3_quote(token_in: Address, token_out: Address, amount: U256, fee: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_V3_QUOTE);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(fee));
    push_u256(&mut out, U256::ZERO);
    out
}

fn encode_v4_quote(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    zero_for_one: bool,
    amount: U256,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_V4_QUOTE);
    push_u256(&mut out, U256::from(0x20u64));
    push_addr(&mut out, currency0);
    push_addr(&mut out, currency1);
    push_u256(&mut out, U256::from(fee));
    push_i32_word(&mut out, tick_spacing);
    push_addr(&mut out, hooks);
    push_bool(&mut out, zero_for_one);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(0x100u64));
    push_u256(&mut out, U256::ZERO);
    out
}

fn encode_get_dy(uint_index: bool, i: u64, j: u64, amount: U256) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if uint_index {
        &SEL_DY_UINT
    } else {
        &SEL_DY_INT
    });
    push_u256(&mut out, U256::from(i));
    push_u256(&mut out, U256::from(j));
    push_u256(&mut out, amount);
    out
}

fn encode_bal3(pool: Address, token_in: Address, token_out: Address, amount: U256) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_BAL3);
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    push_addr(&mut out, Address::ZERO);
    push_u256(&mut out, U256::from(0xc0u64));
    push_u256(&mut out, U256::ZERO);
    out
}

fn encode_bal2(pool_id: B256, token_in: Address, token_out: Address, amount: U256) -> Vec<u8> {
    let mut step = Vec::new();
    step.extend_from_slice(pool_id.as_slice());
    push_u256(&mut step, U256::ZERO);
    push_u256(&mut step, U256::from(1u8));
    push_u256(&mut step, amount);
    push_u256(&mut step, U256::from(0xa0u64));
    push_u256(&mut step, U256::ZERO);
    let mut swaps = Vec::new();
    push_u256(&mut swaps, U256::from(1u8));
    push_u256(&mut swaps, U256::from(0x20u64));
    swaps.extend_from_slice(&step);
    let mut assets = Vec::new();
    push_u256(&mut assets, U256::from(2u8));
    push_addr(&mut assets, token_in);
    push_addr(&mut assets, token_out);
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_BAL2);
    push_u256(&mut out, U256::ZERO);
    push_u256(&mut out, U256::from(0xe0u64));
    push_u256(&mut out, U256::from(0xe0u64 + swaps.len() as u64));
    push_addr(&mut out, Address::ZERO);
    push_bool(&mut out, false);
    push_addr(&mut out, Address::ZERO);
    push_bool(&mut out, false);
    out.extend_from_slice(&swaps);
    out.extend_from_slice(&assets);
    out
}

fn encode_aggregate3(calls: &[(Address, &[u8])]) -> Vec<u8> {
    let mut elements = Vec::with_capacity(calls.len());
    for (target, data) in calls {
        let mut element = Vec::new();
        push_addr(&mut element, *target);
        push_bool(&mut element, true);
        push_u256(&mut element, U256::from(0x60u64));
        push_bytes_tail(&mut element, data);
        elements.push(element);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_AGGREGATE3);
    push_u256(&mut out, U256::from(0x20u64));
    push_u256(&mut out, U256::from(calls.len()));
    let mut cursor = calls.len() * 32;
    let mut tails = Vec::new();
    for element in &elements {
        push_u256(&mut out, U256::from(cursor));
        cursor += element.len();
        tails.extend_from_slice(element);
    }
    out.extend_from_slice(&tails);
    out
}

fn hex_prefix(bytes: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::UNI_V2;
    use crate::detail::ConstantProduct;
    use alloy_primitives::address;

    fn hex_of(bytes: &[u8]) -> String {
        hex_prefix(bytes)
    }

    #[test]
    fn cp_99_buys_ninety_nine_percent_of_the_output() {
        let pool = ConstantProduct {
            kind: UNI_V2,
            id: B256::ZERO,
            pair: Address::ZERO,
            token0: address!("0000000000000000000000000000000000000001"),
            token1: address!("0000000000000000000000000000000000000002"),
            reserve0: U256::from(1_000u64),
            reserve1: U256::from(1_000u64),
            fee_pips: Some(0),
            timestamp: 0,
            flags: 0,
        };
        let (max_in, source) = cp_max(&pool, pool.token0, pool.token1);
        assert_eq!(source, "cp_99");
        assert_eq!(max_in, U256::from(99_000u64));
        let quote = PoolDetail::ConstantProduct(pool.clone())
            .quote(pool.token0, pool.token1, max_in)
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(990u64));
    }

    #[test]
    fn grid_is_increasing_and_ends_at_the_domain() {
        let max_in = U256::from(1_000_000_000u64);
        let amounts = grid_amounts(max_in);
        assert_eq!(amounts.len(), GRID.len());
        assert_eq!(amounts.first().unwrap().1, max_in / U256::from(10_000u32));
        assert_eq!(amounts.last().unwrap().1, max_in);
        for pair in amounts.windows(2) {
            assert!(pair[0].1 < pair[1].1);
        }
        let tiny = grid_amounts(U256::from(50u64));
        let mut seen = tiny.clone();
        seen.dedup_by_key(|(_, amount)| *amount);
        assert_eq!(seen.len(), tiny.len());
        assert_eq!(tiny.last().unwrap().0, "1");
    }

    #[test]
    fn encodings_match_cast() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let amount = U256::from(1000u64);
        assert_eq!(
            hex_of(&encode_amounts_out(amount, weth, usdc)),
            "0xd06ca61f00000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert_eq!(
            hex_of(&encode_v3_quote(weth, usdc, amount, 3000)),
            "0xc6a5026a000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000003e80000000000000000000000000000000000000000000000000000000000000bb80000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex_of(&encode_v4_quote(
                address!("0000000000000000000000000000000000000001"),
                weth,
                3000,
                60,
                address!("0000000000000000000000000000000000000002"),
                true,
                amount,
            )),
            "0xaa9d21cb00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000000000000000bb8000000000000000000000000000000000000000000000000000000000000003c0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex_of(&encode_get_dy(false, 0, 1, amount)),
            "0x5e0d443f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e8"
        );
        assert_eq!(
            hex_of(&encode_get_dy(true, 0, 1, amount)),
            "0x556d6e9f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e8"
        );
        assert_eq!(
            hex_of(&encode_bal3(
                address!("bA1333333333a1BA1108E8412f11850A5C319bA9"),
                weth,
                usdc,
                amount,
            )),
            "0x3ebc54e5000000000000000000000000ba1333333333a1ba1108e8412f11850a5c319ba9000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000003e8000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000000"
        );
        let amounts = encode_amounts_out(amount, weth, usdc);
        let agg = encode_aggregate3(&[(
            address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
            amounts.as_slice(),
        )]);
        assert_eq!(
            hex_of(&agg),
            "0x82ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a4d06ca61f00000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000"
        );
        let mut id = B256::ZERO;
        id[31] = 0xab;
        assert_eq!(
            hex_of(&encode_bal2(id, weth, usdc, amount)),
            "0xf84d066e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000ab0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[test]
    fn aggregate_return_decodes_success_and_revert() {
        let mut data = Vec::new();
        push_u256(&mut data, U256::from(0x20u64));
        push_u256(&mut data, U256::from(2u8));
        push_u256(&mut data, U256::from(0x40u64));
        push_u256(&mut data, U256::from(0xc0u64));
        push_bool(&mut data, true);
        push_u256(&mut data, U256::from(0x40u64));
        push_u256(&mut data, U256::from(32u8));
        push_u256(&mut data, U256::from(42u8));
        push_bool(&mut data, false);
        push_u256(&mut data, U256::from(0x40u64));
        push_u256(&mut data, U256::ZERO);
        let decoded = decode_aggregate(&data).unwrap();
        assert!(decoded[0].0);
        assert_eq!(first_word(&decoded[0].1), Some(U256::from(42u8)));
        assert!(!decoded[1].0);
        assert!(decoded[1].1.is_empty());
        let amounts = {
            let mut raw = Vec::new();
            push_u256(&mut raw, U256::from(0x20u64));
            push_u256(&mut raw, U256::from(2u8));
            push_u256(&mut raw, U256::from(5u8));
            push_u256(&mut raw, U256::from(9u8));
            raw
        };
        let out = interpret(
            "getAmountsOut",
            RawOut {
                ok: true,
                bytes: amounts,
                error: None,
            },
        );
        assert_eq!(out.amount_out, Some(U256::from(9u8)));
    }
}
