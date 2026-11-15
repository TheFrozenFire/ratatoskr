//! JSONL copy of the catalog's stored quote samples.
//!
//! Chain amounts in the file are the samples already stored. Local amounts are
//! recomputed from the snapshot when the file is written.

use super::curve::state_json;
use super::formula::addr;
use super::plan::{plan_of, sample_at, Direction, PoolPlan, Sample, PAGE};
use super::{multicall, QUOTE_LENS};
use crate::catalog::{kind_name, Catalog, METRIC};
use crate::state::{QuoteRecord, CURVE_VERSION, QUOTE_VERSION};
use alloy_primitives::{Address, U256};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FitReport {
    pub pools: u64,
    pub resumed: u64,
    pub chain_ok: u64,
    pub chain_fail: u64,
    pub chain_absent: u64,
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
            observability::info!(
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
/// Drop quote rows whose samples stopped on a transport or decode failure.
///
/// A protocol revert, a missing quoter, and an amount the quoter cannot
/// accept are finished reads and stay. The curve and the snapshot stay.
pub fn drop_partial_quotes(catalog: &Catalog) -> eyre::Result<u64> {
    let mut dropped = 0_u64;
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, PAGE)?;
            if page.is_empty() {
                break;
            }
            let full = page.len() == PAGE;
            after = page.last().map(|row| row.id);
            for row in &page {
                let state = catalog.read_state(row.kind, row.id)?;
                let Some(quote) = state.quotes else {
                    continue;
                };
                if payload_is_partial(&quote.payload) && catalog.delete_quote(row.kind, row.id)? {
                    dropped += 1;
                }
            }
            if !full {
                break;
            }
        }
    }
    Ok(dropped)
}

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

pub(super) fn header_value() -> Value {
    json!({
        "record": "header",
        "schema": 2,
        "curve_version": CURVE_VERSION,
        "fits": "Each pool carries fits: the closed form stored for that direction. score is the all-label worst basis points. formula.kind is the stored formula. A direction with no fit is absent. Skip reasons are not formulas.",
        "units": "Raw token amounts. Decimals are not stored. Do not assume 18.",
        "ground_truth": "chain_out is an execution sample at the stored snapshot block. The lens swaps and returns the output-token balance delta. The state pipeline does not fetch new samples and does not fit curves to them. A stored curve is chosen from local quotes of the ingested snapshot. local_out is recomputed from that snapshot when this file is written.",
        "domain": "max_in is the top of the fit domain. cp_99 buys 99% of the output reserve. ladder is the last step of the concentrated book. balance_search bisects the local quote toward 99% of the output balance, capped at 1000x the input balance. balance is the input balance when that search cannot run. uninitialized and empty_book and none have no samples.",
        "grid": "Fractions of max_in: 0.0001, 0.001, 0.01, 0.05, 0.1, 0.25, 0.5, 0.8, 0.95, 1. A fraction that rounds to 0 is omitted. Two fractions that land on the same amount keep the larger fraction.",
        "chain_max": "If the fraction-1 quote reverts, an 8-step bisection records the largest accepted input. larger_input_reverted is then true. If fraction 1 is accepted, chain_max_in equals that input and larger_input_reverted is false: the domain top worked, and a constant-product pool still has no hard maximum.",
        "pairs": "Every ordered pair when the pool has at most 4 tokens. A larger pool keeps the first 4 and sets pairs_capped.",
        "resume": "Stored chain samples are copied out of the catalog. The state pipeline does not fetch new ones. A repeated pool id is a torn line; keep the last copy.",
        "gaps": {
            "v2_fork": "A fork is swapped on the pair. The fee is the pair's fee, or 30 bps (Pancake 25 bps) when the fee view was unset.",
            "v3_fork": "A fork is swapped on the pool. Callbacks are uniswapV3SwapCallback, pancakeV3SwapCallback, and algebraSwapCallback.",
            "uniswap_v4": "The swap is unlocked on the PoolManager. A hook that reverts fails the quote.",
            "snapshot": "eth_call uses the stored snapshot block. Block 0 is not quoted."
        },
        "quoters": quoter_header()
    })
}

pub(super) fn quoter_header() -> Value {
    json!({
        "lens": { "address": addr(QUOTE_LENS), "method": "swap" },
        "multicall3": addr(multicall())
    })
}

pub(super) fn payload_is_partial(payload: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else {
        return true;
    };
    let Some(directions) = value.get("directions").and_then(Value::as_array) else {
        return true;
    };
    directions.iter().any(|dir| {
        dir.get("samples")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|sample| {
                sample
                    .get("chain_error")
                    .and_then(Value::as_str)
                    .is_some_and(retryable_quote_error)
            })
    })
}

pub(super) fn retryable_quote_error(error: &str) -> bool {
    matches!(
        error,
        "Unexpected error" | "rpc error" | "aggregate decode" | "quote failed"
    ) || error.starts_with("bisection lost")
}

pub(super) fn payload_of(plan: &PoolPlan) -> Vec<u8> {
    let body = json!({
        "pairs_capped": plan.pairs_capped,
        "directions": plan.directions.iter().map(stored_direction).collect::<Vec<_>>(),
    });
    serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec())
}

pub(super) fn stored_direction(dir: &Direction) -> Value {
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

pub(super) fn stored_sample(sample: &Sample) -> Value {
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

pub(super) fn apply_stored(plan: &mut PoolPlan, quote: &QuoteRecord) -> Result<(), String> {
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

pub(super) fn fill_chain(slot: &mut Sample, sample: &Value) {
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

pub(super) fn method_static(name: Option<&str>) -> Option<&'static str> {
    match name {
        Some("getAmountsOut") => Some("getAmountsOut"),
        Some("quoteExactInputSingle") => Some("quoteExactInputSingle"),
        Some("get_dy_int128") => Some("get_dy_int128"),
        Some("get_dy_uint256") => Some("get_dy_uint256"),
        Some("queryBatchSwap") => Some("queryBatchSwap"),
        Some("querySwapSingleTokenExactIn") => Some("querySwapSingleTokenExactIn"),
        Some("swap") => Some("swap"),
        _ => None,
    }
}

pub(super) fn record_from_line(value: &Value) -> Option<QuoteRecord> {
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

pub(super) fn strip_direction(value: &Value) -> Value {
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

pub(super) fn strip_sample(value: &Value) -> Value {
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

pub(super) fn count_line(value: &Value, report: &mut FitReport) {
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

pub(super) fn parse_address(value: Option<&Value>) -> Result<Address, String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| "address".to_string())?
        .parse()
        .map_err(|_| "address".to_string())
}

pub(super) fn required_u256(value: Option<&Value>) -> Result<U256, String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| "amount".to_string())?
        .parse()
        .map_err(|_| "amount".to_string())
}

pub(super) fn optional_u256(value: Option<&Value>) -> Result<Option<U256>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(required_u256(Some(value))?)),
    }
}

pub(super) fn write_pool(
    file: &mut File,
    plan: &PoolPlan,
    report: &mut FitReport,
) -> io::Result<()> {
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

pub(super) fn pool_json(plan: &PoolPlan) -> Value {
    json!({
        "record": "pool",
        "schema": 2,
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
        "curve_version": plan.curve_version,
        "skip_reason": plan.skip_reason,
        "fits": plan.fits,
        "ticks_truncated": plan.truncated,
        "tick_count": plan.tick_count,
        "bitmap_fingerprint": format!("{}", plan.bitmap),
        "pairs_capped": plan.pairs_capped,
        "error": plan.missing,
        "state": plan.detail.as_ref().map(state_json),
        "directions": plan.directions.iter().map(direction_json).collect::<Vec<_>>(),
    })
}

pub(super) fn direction_json(dir: &Direction) -> Value {
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

pub(super) fn sample_json(sample: &Sample) -> Value {
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
