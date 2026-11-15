//! A stored curve chosen from local quotes of one snapshot.
//!
//! A positive local quote is labeled for the selector. A snapshot with no
//! positive quote keeps the structural product formula, or a skip reason.

use super::formula::{addr, opt_u256};
use super::plan::{bigint_from_u256, direction_of, pairs_of, Direction, PAGE};
use crate::catalog::{kind_name, Catalog, METRIC};
use crate::state::{is_canonical, join_detail};
use alloy_primitives::Address;
use chain::detail::{product_fee_pips, PoolDetail};
use fitter::{choose, encode_fit, labels_of, Formula, StoredFit};
use num_bigint::BigInt;
use serde_json::{json, Value};

pub(crate) fn curve_payload(detail: &PoolDetail) -> Vec<u8> {
    if let PoolDetail::Concentrated(pool) = detail {
        if pool.sqrt_price_x96.is_zero() {
            return encode_fit(5, &[]);
        }
    }
    if !is_canonical(detail) {
        return encode_fit(crate::state::skip_reason(detail), &[]);
    }
    encode_fit(0, &structural_fits(detail))
}

pub(super) fn structural_fits(detail: &PoolDetail) -> Vec<StoredFit> {
    let PoolDetail::ConstantProduct(pool) = detail else {
        return Vec::new();
    };
    let fee = BigInt::from(product_fee_pips(pool.kind, pool.fee_pips));
    let denom = BigInt::from(1_000_000u32);
    let reserve0 = bigint_from_u256(pool.reserve0);
    let reserve1 = bigint_from_u256(pool.reserve1);
    vec![
        product_fit(
            pool.token0,
            pool.token1,
            reserve0.clone(),
            reserve1.clone(),
            fee.clone(),
            denom.clone(),
        ),
        product_fit(pool.token1, pool.token0, reserve1, reserve0, fee, denom),
    ]
}

pub(super) fn product_fit(
    token_in: Address,
    token_out: Address,
    reserve_in: BigInt,
    reserve_out: BigInt,
    fee: BigInt,
    denom: BigInt,
) -> StoredFit {
    StoredFit {
        token_in,
        token_out,
        score: 0,
        name: "product".to_string(),
        formula: Formula::Product {
            reserve_in,
            reserve_out,
            fee,
            denom,
        },
    }
}

/// Choose each direction from local quotes of `detail`.
///
/// The selector's label field is named `chain_out`. The amounts written there
/// are `PoolDetail::quote` results. When no direction has a positive local
/// quote, the payload is the structural product formula or a skip reason.
pub(crate) fn local_curve_payload(detail: &PoolDetail) -> Result<Vec<u8>, String> {
    let state = state_json(detail);
    let pairs = pairs_of(detail);
    let mut fits = Vec::new();
    for (token_in, token_out) in pairs {
        let built = direction_of(detail, token_in, token_out);
        let direction = selector_direction(&built);
        let rows = labels_of(&direction)?;
        if rows.is_empty() {
            continue;
        }
        let Some(chosen) = choose(&state, &direction, &rows)? else {
            continue;
        };
        fits.push(StoredFit {
            token_in,
            token_out,
            score: chosen.score,
            name: chosen.name.to_string(),
            formula: chosen.formula,
        });
    }
    if fits.is_empty() {
        return Ok(curve_payload(detail));
    }
    Ok(encode_fit(0, &fits))
}

/// One direction, with local quote amounts in the selector's label field.
pub(super) fn selector_direction(direction: &Direction) -> Value {
    let samples = direction
        .samples
        .iter()
        .filter_map(|sample| {
            let out = sample.local_out.filter(|out| !out.is_zero())?;
            Some(json!({
                "role": sample.role,
                "fraction": sample.fraction,
                "amount_in": sample.amount_in.to_string(),
                "chain_out": out.to_string(),
            }))
        })
        .collect::<Vec<_>>();
    json!({
        "token_in": addr(direction.token_in),
        "token_out": addr(direction.token_out),
        "max_in": direction.max_in.to_string(),
        "samples": samples,
    })
}

/// Replace every stored curve with the current fitter.
pub fn refit_curves(catalog: &Catalog) -> eyre::Result<u64> {
    let mut wrote = 0_u64;
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, PAGE)?;
            if page.is_empty() {
                break;
            }
            let full = page.len() == PAGE;
            after = page.last().map(|row| row.id);
            let started = std::time::Instant::now();
            let mut curves = Vec::new();
            for row in &page {
                let state = catalog.read_state(row.kind, row.id)?;
                let (Some(immutable), Some(mutable)) =
                    (state.immutable.as_ref(), state.mutable.as_ref())
                else {
                    continue;
                };
                let detail =
                    match join_detail(immutable, &mutable.body, &state.ticks, row.kind, row.id) {
                        Ok(detail) => detail,
                        Err(_) => continue,
                    };
                let curve = crate::state::curve_for_snapshot(&detail, mutable.generation)
                    .map_err(|err| eyre::eyre!(err))?;
                curves.push(curve);
            }
            let n = curves.len() as u64;
            catalog.put_curves(&curves)?;
            wrote += n;
            observability::info!(
                kind = kind_name(kind),
                wrote = n,
                ms = started.elapsed().as_millis() as u64,
                curves = wrote,
                "refit"
            );
            if !full {
                break;
            }
        }
    }
    Ok(wrote)
}

pub(super) fn state_json(detail: &PoolDetail) -> Value {
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
            "scaling": pool.scaling.iter().map(|value| value.to_string()).collect::<Vec<_>>(),
            "rates": pool.rates.iter().map(|value| value.to_string()).collect::<Vec<_>>(),
            "stable": pool.flags & chain::detail::FLAG_STABLE != 0,
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
            "crypto": pool.flags & chain::detail::FLAG_CRYPTO != 0,
            "flags": pool.flags,
            "coins": pool.coins.iter().map(|coin| json!({
                "token": addr(coin.token),
                "balance": coin.balance.to_string(),
                "rate": coin.rate.to_string(),
                "price_scale": coin.price_scale.to_string(),
                "precision": coin.precision.to_string(),
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
