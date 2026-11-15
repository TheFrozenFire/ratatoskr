//! Product, hyperbola, ladder, balancer, and bin candidates.

use crate::formula::Formula;
use crate::select::curve::scale_candidate;
use crate::select::sample::*;
use crate::select::score::*;
use crate::units::*;
use num_bigint::BigInt;
use num_traits::{One, Signed, Zero};
use serde_json::Value;

pub(crate) fn hyperbola_candidate(rows: &[(BigInt, BigInt)]) -> Option<Scored> {
    let (x1, y1) = rows.first()?;
    let (x2, y2) = rows.last()?;
    score_predict(
        Formula::Hyperbola {
            x1: x1.clone(),
            y1: y1.clone(),
            x2: x2.clone(),
            y2: y2.clone(),
        },
        rows,
    )
}

pub(crate) fn ladder_candidate(
    state: &Value,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (Some(fee), Some(sqrt_price), Some(liquidity), Some(tick)) = (
        whole(state.get("fee_pips"))?,
        amount(state.get("sqrt_price_x96"))?,
        amount(state.get("liquidity"))?,
        whole(state.get("tick"))?,
    ) else {
        return Ok(None);
    };
    let (Some(token0), Some(token1)) = (
        state.get("token0").and_then(Value::as_str),
        state.get("token1").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let (Some(tin), Some(tout)) = (
        direction.get("token_in").and_then(Value::as_str),
        direction.get("token_out").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let zero_for_one = if tin.eq_ignore_ascii_case(token0) && tout.eq_ignore_ascii_case(token1) {
        true
    } else if tin.eq_ignore_ascii_case(token1) && tout.eq_ignore_ascii_case(token0) {
        false
    } else {
        return Ok(None);
    };
    let Some(raw_ticks) = state.get("ticks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut ticks = Vec::new();
    for word in raw_ticks {
        let Some(word) = word.as_object() else {
            return Err("tick".to_string());
        };
        let (Some(tick_at), Some(net)) =
            (whole(word.get("tick"))?, amount(word.get("liquidity_net"))?)
        else {
            return Err("tick".to_string());
        };
        ticks.push((tick_at, net));
    }
    Ok(score_predict(
        Formula::Ladder {
            sqrt_price,
            liquidity,
            tick,
            fee,
            ticks,
            zero_for_one,
        },
        rows,
    ))
}

pub(crate) fn product_candidate(
    state: &Value,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (Some(reserve0), Some(reserve1), Some(fee)) = (
        amount(state.get("reserve0"))?,
        amount(state.get("reserve1"))?,
        whole(state.get("fee_pips_used"))?,
    ) else {
        return Ok(None);
    };
    let (Some(token0), Some(token1), Some(tin), Some(tout)) = (
        state.get("token0").and_then(Value::as_str),
        state.get("token1").and_then(Value::as_str),
        direction.get("token_in").and_then(Value::as_str),
        direction.get("token_out").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let (reserve_in, reserve_out) =
        if tin.eq_ignore_ascii_case(token0) && tout.eq_ignore_ascii_case(token1) {
            (reserve0, reserve1)
        } else if tin.eq_ignore_ascii_case(token1) && tout.eq_ignore_ascii_case(token0) {
            (reserve1, reserve0)
        } else {
            return Ok(None);
        };
    Ok(score_predict(
        Formula::Product {
            reserve_in,
            reserve_out,
            fee,
            denom: bi(1_000_000),
        },
        rows,
    ))
}

pub(crate) fn weighted_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let balances = amount_vec(state.get("balances"))?;
    let weights = amount_vec(state.get("weights"))?;
    let fee = amount_or_zero(state.get("fee"))?;
    if weights.len() != balances.len() || fee > wad() || i >= balances.len() || j >= balances.len()
    {
        return Ok(None);
    }
    let (balance_in, balance_out) = (balances[i].clone(), balances[j].clone());
    let (weight_in, weight_out) = (weights[i].clone(), weights[j].clone());
    Ok(score_predict(
        Formula::Weighted {
            balance_in,
            balance_out,
            weight_in,
            weight_out,
            fee,
        },
        rows,
    ))
}

pub(crate) fn balancer_stable_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let balances = amount_vec(state.get("balances"))?;
    let amp_word = amount_or_zero(state.get("amp"))?;
    let precision = nonzero_amount(state.get("amp_precision"), BigInt::one())?;
    let fee = amount_or_zero(state.get("fee"))?;
    if amp_word <= BigInt::zero()
        || fee > wad()
        || balances.iter().any(|balance| balance <= &BigInt::zero())
        || i >= balances.len()
        || j >= balances.len()
    {
        return Ok(None);
    }
    let amp = fd(&amp_word, &precision);
    let choices = vec![precisions(); balances.len()];
    Ok(best_over(balances.len(), &choices, |precs| {
        let xp: Vec<BigInt> = balances
            .iter()
            .zip(precs)
            .map(|(balance, prec)| balance * prec)
            .collect();
        score_predict(
            Formula::BalancerStable {
                xp,
                amp: amp.clone(),
                i: i as i64,
                j: j as i64,
                fee: fee.clone(),
                prec_i: precs[i].clone(),
                prec_j: precs[j].clone(),
            },
            rows,
        )
    }))
}

pub(crate) fn balancer_scale_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let balances = amount_vec(state.get("balances"))?;
    let amp_word = amount_or_zero(state.get("amp"))?;
    let precision = nonzero_amount(state.get("amp_precision"), BigInt::one())?;
    let fee = amount_or_zero(state.get("fee"))?;
    if amp_word <= BigInt::zero()
        || fee > wad()
        || balances.iter().any(|balance| balance <= &BigInt::zero())
    {
        return Ok(None);
    }
    let amp = fd(&amp_word, &precision);
    let rates = vec![BigInt::zero(); balances.len()];
    scale_candidate(
        &balances,
        &amp,
        i,
        j,
        &fee,
        &rates,
        direction,
        rows,
        &wad(),
        true,
    )
}

pub(crate) fn bin_candidate(
    state: &Value,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let reference = amount(state.get("reference_x64"))?.unwrap_or_else(BigInt::zero);
    if reference <= BigInt::zero() {
        return Ok(None);
    }
    let (Some(token0), Some(token1), Some(tin), Some(tout)) = (
        state.get("token0").and_then(Value::as_str),
        state.get("token1").and_then(Value::as_str),
        direction.get("token_in").and_then(Value::as_str),
        direction.get("token_out").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let (selling0, scale_key) =
        if tin.eq_ignore_ascii_case(token0) && tout.eq_ignore_ascii_case(token1) {
            (true, "scale1")
        } else if tin.eq_ignore_ascii_case(token1) && tout.eq_ignore_ascii_case(token0) {
            (false, "scale0")
        } else {
            return Ok(None);
        };
    let scale_out = amount(state.get(scale_key))?.unwrap_or_else(BigInt::zero);
    let scale_in_key = if selling0 { "scale0" } else { "scale1" };
    let scale_in = amount(state.get(scale_in_key))?.unwrap_or_else(BigInt::zero);
    // The oracle price is whole tokens. Fold the scale ratio into the reference the quote divides by.
    let reference = if scale_in.is_positive() && scale_out.is_positive() {
        (&reference * &scale_in) / &scale_out
    } else {
        reference
    };
    let bin_index = whole(state.get("bin_index"))?;
    let mut scaled_out = BigInt::zero();
    let mut length = BigInt::zero();
    if let (Some(bins), Some(bin_index)) = (state.get("bins").and_then(Value::as_array), bin_index)
    {
        for word in bins {
            let Some(word) = word.as_object() else {
                continue;
            };
            if whole(word.get("index"))?.as_ref() != Some(&bin_index) {
                continue;
            }
            length = whole(word.get("length_e6"))?.unwrap_or_else(BigInt::zero);
            let key = if selling0 {
                "token1_scaled"
            } else {
                "token0_scaled"
            };
            scaled_out = amount(word.get(key))?.unwrap_or_else(BigInt::zero);
            break;
        }
    }
    let distance = whole(state.get("distance_e6"))?.unwrap_or_else(BigInt::zero);
    let position = amount(state.get("position"))?.unwrap_or_else(BigInt::zero);
    Ok(score_predict(
        Formula::Bins {
            reference,
            distance,
            length,
            position,
            selling0,
            scaled_out,
            scale_out,
        },
        rows,
    ))
}
