//! Stableswap, crypto, and flat-pair candidates.

use crate::eval::{dxp_of, stable_flat_quote, unscale_out};
use crate::formula::Formula;
use crate::predict::{crypto_scale, crypto_y3, flat_scale, stable_swap_out, worst_bps, Predict};
use crate::select::sample::*;
use crate::select::score::*;
use crate::units::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn xp_of(balance: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        balance * prec
    } else {
        fd(&(balance * rate), &wad()) * prec
    }
}

pub(crate) fn choices_for(rates: &[BigInt]) -> Vec<Vec<BigInt>> {
    let grid = precisions();
    rates
        .iter()
        .map(|rate| {
            if rate.is_zero() || rate == &wad() {
                grid.clone()
            } else {
                vec![BigInt::one()]
            }
        })
        .collect()
}

// The snapshot does not store an off-peg multiplier.
pub(crate) fn offpeg_list() -> Vec<BigInt> {
    [2, 5, 10, 20, 50, 100, 200, 500, 1000]
        .into_iter()
        .map(|factor| &fee_denom() * bi(factor))
        .collect()
}

pub(crate) fn stable_quote(
    balances: &[BigInt],
    rates: &[BigInt],
    precs: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    offpeg: &BigInt,
    rows: &[(BigInt, BigInt)],
) -> Option<Scored> {
    let xp: Vec<BigInt> = balances
        .iter()
        .zip(rates)
        .zip(precs)
        .map(|((balance, rate), prec)| xp_of(balance, rate, prec))
        .collect();
    score_predict(
        Formula::Stable {
            xp,
            amp: amp.clone(),
            i: i as i64,
            j: j as i64,
            fee: fee.clone(),
            denom: fee_denom(),
            offpeg: offpeg.clone(),
            rate_i: rates[i].clone(),
            rate_j: rates[j].clone(),
            prec_i: precs[i].clone(),
            prec_j: precs[j].clone(),
        },
        rows,
    )
}

pub(crate) fn stable_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let amp = amount_or_zero(state.get("amp"))?;
    let mut fee = amount_or_zero(state.get("fee"))?;
    if fee > fee_denom() {
        fee = BigInt::zero();
    }
    if amp <= BigInt::zero() {
        return Ok(None);
    }
    let mut rates = Vec::new();
    let mut balances = Vec::new();
    for coin in coins {
        rates.push(coin_field(coin, "rate")?);
        balances.push(coin_field(coin, "balance")?);
    }
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let choices = choices_for(&rates);
    let flat = best_over(coins.len(), &choices, |precs| {
        stable_quote(
            &balances,
            &rates,
            precs,
            &amp,
            i,
            j,
            &fee,
            &BigInt::zero(),
            rows,
        )
    });
    if fee.is_zero() || flat.as_ref().is_some_and(|item| item.0 <= 10) {
        return Ok(flat);
    }
    let mut best = flat;
    for offpeg in offpeg_list() {
        let got = best_over(coins.len(), &choices, |precs| {
            stable_quote(&balances, &rates, precs, &amp, i, j, &fee, &offpeg, rows)
        });
        let Some(got) = got else {
            continue;
        };
        if worse(&best, got.0) {
            best = Some(got);
        }
        if best.as_ref().is_some_and(|item| item.0 <= 10) {
            return Ok(best);
        }
    }
    let _ = amp;
    Ok(best)
}

pub(crate) fn stable_mixed_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let amp = amount_or_zero(state.get("amp"))?;
    let mut fee = amount_or_zero(state.get("fee"))?;
    if fee > fee_denom() {
        fee = BigInt::zero();
    }
    if amp <= BigInt::zero() || coins.len() != 4 {
        return Ok(None);
    }
    let mut rates = Vec::new();
    let mut balances = Vec::new();
    for coin in coins {
        rates.push(coin_field(coin, "rate")?);
        balances.push(coin_field(coin, "balance")?);
    }
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let choices = choices_for(&rates);
    let mut width = BigInt::one();
    for choice in &choices {
        width *= choice.len();
    }
    if width <= bi(64) || width > bi(256) {
        return Ok(None);
    }
    let (train, hold) = split_holdout(direction)?;
    let target_owned;
    let target: &[(BigInt, BigInt)] = if train.len() >= 2 {
        &train
    } else {
        target_owned = rows.to_vec();
        &target_owned
    };
    let mut best: Option<(i64, BigInt, Formula)> = None;
    for_product(&choices, |precs| {
        let xp: Vec<BigInt> = balances
            .iter()
            .zip(&rates)
            .zip(precs)
            .map(|((balance, rate), prec)| xp_of(balance, rate, prec))
            .collect();
        if xp.iter().any(|value| value <= &BigInt::zero()) {
            return;
        }
        let spread = fd(
            &(&xp.iter().max().unwrap().clone() * bi(1000)),
            xp.iter().min().unwrap(),
        );
        let formula = Formula::Stable {
            xp,
            amp: amp.clone(),
            i: i as i64,
            j: j as i64,
            fee: fee.clone(),
            denom: fee_denom(),
            offpeg: BigInt::zero(),
            rate_i: rates[i].clone(),
            rate_j: rates[j].clone(),
            prec_i: precs[i].clone(),
            prec_j: precs[j].clone(),
        };
        let quote = formula.predict();
        let Some(score) = worst_bps(&|dx| quote(dx.clone()), target) else {
            return;
        };
        let replace = match &best {
            None => true,
            Some((best_score, best_spread, _)) => {
                score < *best_score || (score == *best_score && spread < *best_spread)
            }
        };
        if replace {
            best = Some((score, spread, formula));
        }
    });
    let Some((train_score, _, chosen)) = best else {
        return Ok(None);
    };
    if train_score > 10 {
        return Ok(None);
    }
    if !hold.is_empty() {
        let quote = chosen.predict();
        let hold_score = worst_bps(&|dx| quote(dx.clone()), &hold);
        if hold_score.is_none_or(|score| score > 10) {
            return Ok(None);
        }
    }
    let scored = if rows.is_empty() { target } else { rows };
    let quote = chosen.predict();
    let Some(score) = worst_bps(&|dx| quote(dx.clone()), scored) else {
        return Ok(None);
    };
    if score > 10 {
        return Ok(None);
    }
    Ok(Some((score, chosen)))
}

pub(crate) fn fee_words(state: &Value) -> Result<(BigInt, BigInt, BigInt), String> {
    let mid = amount_or(
        state.get("mid_fee"),
        amount_or(state.get("fee"), BigInt::zero())?,
    )?;
    let out_fee = amount_or(state.get("out_fee"), mid.clone())?;
    let fee_gamma = amount_or_zero(state.get("fee_gamma"))?;
    Ok((mid, out_fee, fee_gamma))
}

pub(crate) fn balances_of(coins: &[Value]) -> Result<Vec<BigInt>, String> {
    let mut balances = Vec::new();
    for coin in coins {
        balances.push(coin_field(coin, "balance")?);
    }
    Ok(balances)
}

pub(crate) fn crypto2_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let ann = amount_or_zero(state.get("amp"))?;
    let gamma = amount_or_zero(state.get("gamma"))?;
    let d = amount_or_zero(state.get("d"))?;
    if ann <= BigInt::zero()
        || gamma <= BigInt::zero()
        || d <= BigInt::zero()
        || ann > pow10(12)
        || gamma > pow10(20)
    {
        return Ok(None);
    }
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let balances = balances_of(coins)?;
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let (mid, out_fee, fee_gamma) = fee_words(state)?;
    let choices = vec![precisions(), precisions()];
    let got = best_over(2, &choices, |precs| {
        let scale = crypto_scale(
            &ann,
            &gamma,
            &d,
            &balances[0],
            &balances[1],
            &precs[0],
            &precs[1],
        )?;
        if scale <= BigInt::zero() {
            return None;
        }
        score_predict(
            Formula::Crypto2 {
                balances: balances.clone(),
                precs: vec![precs[0].clone(), precs[1].clone()],
                scale,
                ann: ann.clone(),
                gamma: gamma.clone(),
                d: d.clone(),
                i: i as i64,
                j: j as i64,
                mid: mid.clone(),
                out_fee: out_fee.clone(),
                fee_gamma: fee_gamma.clone(),
            },
            rows,
        )
    });
    Ok(got)
}

pub(crate) fn flat_pair_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let amp = amount_or_zero(state.get("amp"))?;
    let d = amount_or_zero(state.get("d"))?;
    if amp <= BigInt::zero() || d <= BigInt::zero() || amp > pow10(12) {
        return Ok(None);
    }
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let balances = balances_of(coins)?;
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let (mid, out_fee, fee_gamma) = fee_words(state)?;
    let choices = vec![precisions(), precisions()];
    let got = best_over(2, &choices, |precs| {
        let scale = flat_scale(&amp, &d, &balances[0], &balances[1], &precs[0], &precs[1])?;
        if scale <= BigInt::zero() {
            return None;
        }
        let mut best: Option<Scored> = None;
        for flat_fee in [false, true] {
            let formula = Formula::FlatPair {
                balances: balances.clone(),
                precs: vec![precs[0].clone(), precs[1].clone()],
                scale: scale.clone(),
                amp: amp.clone(),
                d: d.clone(),
                i: i as i64,
                j: j as i64,
                mid: mid.clone(),
                out_fee: out_fee.clone(),
                fee_gamma: fee_gamma.clone(),
                flat_fee,
            };
            let quote = formula.predict();
            let Some(score) = worst_bps(&|dx| quote(dx.clone()), rows) else {
                continue;
            };
            if worse(&best, score) {
                best = Some((score, formula));
            }
            if score <= 10 {
                return best;
            }
        }
        best
    });
    Ok(got)
}

pub(crate) fn price_scales(coins: &[Value]) -> Result<Option<(BigInt, BigInt)>, String> {
    if coins.len() < 2 {
        return Ok(None);
    }
    let left = coin_field(&coins[0], "price_scale")?;
    let right = coin_field(&coins[1], "price_scale")?;
    let limit = pow10(40);
    if left <= BigInt::zero() || right <= BigInt::zero() || left > limit || right > limit {
        return Ok(None);
    }
    Ok(Some((left, right)))
}

pub(crate) fn crypto3_candidate(
    state: &Value,
    i: usize,
    j: usize,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let ann = amount_or_zero(state.get("amp"))?;
    let gamma = amount_or_zero(state.get("gamma"))?;
    let d = amount_or_zero(state.get("d"))?;
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    if ann <= BigInt::zero()
        || gamma <= BigInt::zero()
        || d <= BigInt::zero()
        || ann > pow10(12)
        || gamma > pow10(20)
    {
        return Ok(None);
    }
    let Some(scales) = price_scales(coins)? else {
        return Ok(None);
    };
    let balances = balances_of(coins)?;
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let (mid, out_fee, fee_gamma) = fee_words(state)?;
    let choices = vec![precisions(), precisions(), precisions()];
    let got = best_over(3, &choices, |precs| {
        let precs = (precs[0].clone(), precs[1].clone(), precs[2].clone());
        let predict = crypto3_predict(
            &balances, &scales, &ann, &gamma, &d, i, j, precs, &mid, &out_fee, &fee_gamma,
        );
        score_predict(predict, rows)
    });
    Ok(got)
}

pub(crate) fn crypto3_predict(
    balances: &[BigInt],
    scales: &(BigInt, BigInt),
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    i: usize,
    j: usize,
    precs: (BigInt, BigInt, BigInt),
    mid: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> Formula {
    Formula::Crypto3 {
        balances: balances.to_vec(),
        precs: vec![precs.0, precs.1, precs.2],
        scales: vec![scales.0.clone(), scales.1.clone()],
        ann: ann.clone(),
        gamma: gamma.clone(),
        d: d.clone(),
        i: i as i64,
        j: j as i64,
        mid: mid.clone(),
        out_fee: out_fee.clone(),
        fee_gamma: fee_gamma.clone(),
    }
}

pub(crate) fn crypto3_at(
    balances: &[BigInt],
    scales: &(BigInt, BigInt),
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    i: usize,
    j: usize,
    prec: &BigInt,
    mid: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
    rows: &[(BigInt, BigInt)],
) -> (Option<i64>, Formula) {
    let formula = crypto3_predict(
        balances,
        scales,
        ann,
        gamma,
        d,
        i,
        j,
        (prec.clone(), prec.clone(), prec.clone()),
        mid,
        out_fee,
        fee_gamma,
    );
    let quote = formula.predict();
    (worst_bps(&|dx| quote(dx.clone()), rows), formula)
}

pub(crate) fn ann_anchors() -> Vec<BigInt> {
    vec![
        bi(2700),
        bi(10_000),
        bi(100_000),
        bi(400_000),
        bi(1_707_629),
        bi(2_700_000),
        pow10(7),
        pow10(8),
        bi(270_000_000),
    ]
}

pub(crate) fn min_ann() -> BigInt {
    bi(2700)
}

pub(crate) fn max_ann() -> BigInt {
    bi(270_000_000)
}

pub(crate) fn crypto3_fit_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let gamma = amount_or_zero(state.get("gamma"))?;
    let d = amount_or_zero(state.get("d"))?;
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    if gamma <= BigInt::zero() || d <= BigInt::zero() || gamma > pow10(20) {
        return Ok(None);
    }
    let Some(scales) = price_scales(coins)? else {
        return Ok(None);
    };
    let balances = balances_of(coins)?;
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let (mid, out_fee, fee_gamma) = fee_words(state)?;
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    let mut best: Option<(i64, BigInt, BigInt)> = None;
    let record = |best: &mut Option<(i64, BigInt, BigInt)>, ann: BigInt, prec: &BigInt| {
        let (score, _) = crypto3_at(
            &balances, &scales, &ann, &gamma, &d, i, j, prec, &mid, &out_fee, &fee_gamma, target,
        );
        let Some(score) = score else { return };
        if best.as_ref().map(|item| score < item.0).unwrap_or(true) {
            *best = Some((score, ann, prec.clone()));
        }
    };
    for prec in precisions() {
        for ann in ann_anchors() {
            record(&mut best, ann, &prec);
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                break;
            }
        }
        if best.as_ref().is_some_and(|item| item.0 <= 10) {
            break;
        }
        let mut left = min_ann();
        let mut right = max_ann();
        for _step in 0..24 {
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                break;
            }
            if &right - &left < max_bi(fd(&right, &bi(200)), bi(10)) {
                break;
            }
            let (first, second) = ternary_points(&left, &right);
            let (left_score, _) = crypto3_at(
                &balances, &scales, &first, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma,
                target,
            );
            let (right_score, _) = crypto3_at(
                &balances, &scales, &second, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma,
                target,
            );
            if left_score.is_none() && right_score.is_none() {
                break;
            }
            if left_strict(left_score, right_score) {
                right = second;
                if let Some(left_score) = left_score {
                    if best
                        .as_ref()
                        .map(|item| left_score < item.0)
                        .unwrap_or(true)
                    {
                        best = Some((left_score, first, prec.clone()));
                    }
                }
            } else {
                left = first;
                if let Some(right_score) = right_score {
                    if best
                        .as_ref()
                        .map(|item| right_score < item.0)
                        .unwrap_or(true)
                    {
                        best = Some((right_score, second, prec.clone()));
                    }
                }
            }
        }
    }
    let Some((train_score, ann, prec)) = best else {
        return Ok(None);
    };
    if train_score > 10 {
        return Ok(None);
    }
    if !hold.is_empty() {
        let (hold_score, _) = crypto3_at(
            &balances, &scales, &ann, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma, &hold,
        );
        if hold_score.is_none_or(|score| score > 10) {
            return Ok(None);
        }
    }
    let (score, predict) = crypto3_at(
        &balances, &scales, &ann, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma, rows,
    );
    match score {
        Some(score) if score <= 10 => Ok(Some((score, predict))),
        _ => Ok(None),
    }
}

pub(crate) fn scaled_xp(
    balances: &[BigInt],
    precs: &[BigInt],
    scales: &(BigInt, BigInt),
) -> Vec<BigInt> {
    vec![
        &balances[0] * &precs[0],
        fd(&(&balances[1] * &scales.0), &wad()) * &precs[1],
        fd(&(&balances[2] * &scales.1), &wad()) * &precs[2],
    ]
}

pub(crate) fn spread_of(xp: &[BigInt]) -> Option<BigInt> {
    let max = xp.iter().max()?;
    let min = xp.iter().min()?;
    if min <= &BigInt::zero() {
        return None;
    }
    Some(fd(&(max * &bi(1000)), min))
}

pub(crate) fn reserve_gap(
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    xp: &[BigInt],
) -> Option<(BigInt, BigInt)> {
    let mut total = BigInt::zero();
    let mut worst = BigInt::zero();
    for index in 0..3 {
        let y = crypto_y3(ann, gamma, xp, d, index as i64)?;
        if xp[index] <= BigInt::zero() {
            return None;
        }
        let gap = (&y - &xp[index]).abs();
        total += &gap;
        let rel = fd(&(&gap * pow10(12)), &xp[index]);
        if rel > worst {
            worst = rel;
        }
    }
    Some((total, worst))
}

pub(crate) fn search_consistent_ann(
    gamma: &BigInt,
    d: &BigInt,
    xp: &[BigInt],
    stored_amp: &BigInt,
) -> Option<(BigInt, BigInt, BigInt)> {
    let mut best: Option<(BigInt, BigInt, BigInt)> = None;
    let take = |best: &mut Option<(BigInt, BigInt, BigInt)>, ann: BigInt| {
        if ann < min_ann() || ann > max_ann() {
            return;
        }
        let Some((total, rel)) = reserve_gap(&ann, gamma, d, xp) else {
            return;
        };
        let replace = match &*best {
            None => true,
            Some((best_rel, best_total, _)) => {
                &rel < best_rel || (&rel == best_rel && &total < best_total)
            }
        };
        if replace {
            *best = Some((rel, total, ann));
        }
    };
    for ann in ann_anchors() {
        take(&mut best, ann);
    }
    for factor in [bi(1), bi(27), a_multiplier(), &bi(27) * &a_multiplier()] {
        take(&mut best, stored_amp * &factor);
    }
    let mut left = min_ann();
    let mut right = max_ann();
    for _step in 0..28 {
        if &right - &left < max_bi(fd(&right, &bi(500)), bi(4)) {
            break;
        }
        let (first, second) = ternary_points(&left, &right);
        let left_rel = reserve_gap(&first, gamma, d, xp).map(|gap| gap.1);
        let right_rel = reserve_gap(&second, gamma, d, xp).map(|gap| gap.1);
        if left_rel.is_none() && right_rel.is_none() {
            break;
        }
        let left_better = right_rel.is_none()
            || left_rel
                .as_ref()
                .is_some_and(|left| right_rel.as_ref().is_none_or(|right| left <= right));
        if left_better {
            right = second;
            take(&mut best, first);
        } else {
            left = first;
            take(&mut best, second);
        }
    }
    let ann = best.as_ref()?.2.clone();
    let span = max_bi(fd(&ann, &bi(20)), bi(20));
    let mut left = max_bi(min_ann(), &ann - &span);
    let mut right = min_bi(max_ann(), &ann + &span);
    for _step in 0..24 {
        if &right - &left < bi(4) {
            break;
        }
        let (first, second) = ternary_points(&left, &right);
        let left_total = reserve_gap(&first, gamma, d, xp).map(|gap| gap.0);
        let right_total = reserve_gap(&second, gamma, d, xp).map(|gap| gap.0);
        if left_total.is_none() && right_total.is_none() {
            break;
        }
        let left_better = right_total.is_none()
            || left_total
                .as_ref()
                .is_some_and(|left| right_total.as_ref().is_none_or(|right| left <= right));
        if left_better {
            right = second;
            take(&mut best, first);
        } else {
            left = first;
            take(&mut best, second);
        }
    }
    best
}

pub(crate) fn crypto3_consistent_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let gamma = amount_or_zero(state.get("gamma"))?;
    let d = amount_or_zero(state.get("d"))?;
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    if gamma <= BigInt::zero() || d <= BigInt::zero() || gamma > pow10(20) || coins.len() != 3 {
        return Ok(None);
    }
    let Some(scales) = price_scales(coins)? else {
        return Ok(None);
    };
    let balances = balances_of(coins)?;
    if balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let stored_amp = amount_or_zero(state.get("amp"))?;
    let grid = precisions();
    let choices = vec![grid.clone(), grid.clone(), grid];
    let mut ranked = Vec::new();
    for_product(&choices, |precs| {
        let xp = scaled_xp(&balances, precs, &scales);
        let Some(spread) = spread_of(&xp) else {
            return;
        };
        ranked.push((spread, precs.to_vec(), xp));
    });
    ranked.sort_by(|left, right| left.0.cmp(&right.0));
    let mut best: Option<(BigInt, BigInt, BigInt, Vec<BigInt>)> = None;
    for (_spread, precs, xp) in ranked.into_iter().take(4) {
        let Some((rel, total, ann)) = search_consistent_ann(&gamma, &d, &xp, &stored_amp) else {
            continue;
        };
        if rel > pow10(6) {
            continue;
        }
        let replace = match &best {
            None => true,
            Some((best_rel, best_total, _, _)) => {
                &rel < best_rel || (&rel == best_rel && &total < best_total)
            }
        };
        if replace {
            best = Some((rel, total, ann, precs));
        }
    }
    let Some((_rel, _total, ann, precs)) = best else {
        return Ok(None);
    };
    let (mid, out_fee, fee_gamma) = fee_words(state)?;
    let (train, hold) = split_holdout(direction)?;
    let precs = (precs[0].clone(), precs[1].clone(), precs[2].clone());
    if !hold.is_empty() {
        let predict = crypto3_predict(
            &balances,
            &scales,
            &ann,
            &gamma,
            &d,
            i,
            j,
            precs.clone(),
            &mid,
            &out_fee,
            &fee_gamma,
        );
        let quote = predict.predict();
        if worst_bps(&|dx| quote(dx.clone()), &hold).is_none_or(|score| score > 10) {
            return Ok(None);
        }
    }
    let scored = if rows.is_empty() {
        train
    } else {
        rows.to_vec()
    };
    let predict = crypto3_predict(
        &balances, &scales, &ann, &gamma, &d, i, j, precs, &mid, &out_fee, &fee_gamma,
    );
    let quote = predict.predict();
    match worst_bps(&|dx| quote(dx.clone()), &scored) {
        Some(score) if score <= 10 => Ok(Some((score, predict))),
        _ => Ok(None),
    }
}

pub(crate) fn stable_scale_predict(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    which: usize,
    ppm: &BigInt,
    denom: &BigInt,
    fee_on_input: bool,
) -> Formula {
    let mut scaled = xp.to_vec();
    scaled[which] = fd(&(&scaled[which] * ppm), &bi(10_000));
    Formula::StableScale {
        xp: scaled,
        amp: amp.clone(),
        i: i as i64,
        j: j as i64,
        fee: fee.clone(),
        rates: rates.to_vec(),
        which: which as u32,
        ppm: ppm.clone(),
        denom: denom.clone(),
        fee_on_input,
    }
}

pub(crate) fn stable_scale_at(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    which: usize,
    ppm: &BigInt,
    rows: &[(BigInt, BigInt)],
    denom: &BigInt,
    fee_on_input: bool,
) -> (Option<i64>, Formula) {
    let formula = stable_scale_predict(xp, amp, i, j, fee, rates, which, ppm, denom, fee_on_input);
    let quote = formula.predict();
    (worst_bps(&|dx| quote(dx.clone()), rows), formula)
}

pub(crate) fn scale_window(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    target: &[(BigInt, BigInt)],
    anchors: &[i64],
    lo: i64,
    hi: i64,
    steps: usize,
    stop_gap: i64,
    denom: &BigInt,
    fee_on_input: bool,
    prefer_left: bool,
) -> Option<(i64, usize, BigInt)> {
    let mut best: Option<(i64, usize, BigInt)> = None;
    let take = |best: &mut Option<(i64, usize, BigInt)>, which: usize, ppm: BigInt| {
        let (score, _) = stable_scale_at(
            xp,
            amp,
            i,
            j,
            fee,
            rates,
            which,
            &ppm,
            target,
            denom,
            fee_on_input,
        );
        let Some(score) = score else { return };
        if best.as_ref().map(|item| score < item.0).unwrap_or(true) {
            *best = Some((score, which, ppm));
        }
    };
    for which in [i, j] {
        for ppm in anchors {
            take(&mut best, which, bi(*ppm));
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                return best;
            }
        }
        let mut left = bi(lo);
        let mut right = bi(hi);
        for _step in 0..steps {
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                return best;
            }
            if &right - &left < bi(stop_gap) {
                break;
            }
            let (first, second) = ternary_points(&left, &right);
            let (left_score, _) = stable_scale_at(
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                which,
                &first,
                target,
                denom,
                fee_on_input,
            );
            let (right_score, _) = stable_scale_at(
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                which,
                &second,
                target,
                denom,
                fee_on_input,
            );
            if left_score.is_none() && right_score.is_none() {
                break;
            }
            let left_better = match (left_score, right_score) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(left), Some(right)) => {
                    if prefer_left {
                        left <= right
                    } else {
                        left < right
                    }
                }
            };
            if left_better {
                right = second;
                if let Some(left_score) = left_score {
                    if best
                        .as_ref()
                        .map(|item| left_score < item.0)
                        .unwrap_or(true)
                    {
                        best = Some((left_score, which, first));
                    }
                }
            } else {
                left = first;
                if let Some(right_score) = right_score {
                    if best
                        .as_ref()
                        .map(|item| right_score < item.0)
                        .unwrap_or(true)
                    {
                        best = Some((right_score, which, second));
                    }
                }
            }
        }
    }
    best
}

pub(crate) fn finish_scale(
    best: Option<(i64, usize, BigInt)>,
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    hold: &[(BigInt, BigInt)],
    rows: &[(BigInt, BigInt)],
    denom: &BigInt,
    fee_on_input: bool,
) -> Option<Scored> {
    let (train_score, which, ppm) = best?;
    if train_score > 10 {
        return None;
    }
    if !hold.is_empty() {
        let (hold_score, _) = stable_scale_at(
            xp,
            amp,
            i,
            j,
            fee,
            rates,
            which,
            &ppm,
            hold,
            denom,
            fee_on_input,
        );
        if hold_score.is_none_or(|score| score > 10) {
            return None;
        }
    }
    let (score, predict) = stable_scale_at(
        xp,
        amp,
        i,
        j,
        fee,
        rates,
        which,
        &ppm,
        rows,
        denom,
        fee_on_input,
    );
    let score = score?;
    if score > 10 {
        None
    } else {
        Some((score, predict))
    }
}

pub(crate) const NEAR_ANCHORS: [i64; 9] =
    [10000, 9800, 10200, 9600, 10400, 9400, 10600, 9000, 11000];

pub(crate) const WIDE_ANCHORS: [i64; 8] = [8000, 12000, 7000, 13000, 6000, 15000, 5000, 20000];

pub(crate) fn scale_candidate(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    direction: &Value,
    rows: &[(BigInt, BigInt)],
    denom: &BigInt,
    fee_on_input: bool,
) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    let narrow = scale_window(
        xp,
        amp,
        i,
        j,
        fee,
        rates,
        target,
        &NEAR_ANCHORS,
        9000,
        11000,
        18,
        5,
        denom,
        fee_on_input,
        false,
    );
    if let Some(accepted) = finish_scale(
        narrow,
        xp,
        amp,
        i,
        j,
        fee,
        rates,
        &hold,
        rows,
        denom,
        fee_on_input,
    ) {
        return Ok(Some(accepted));
    }
    let wide = scale_window(
        xp,
        amp,
        i,
        j,
        fee,
        rates,
        target,
        &WIDE_ANCHORS,
        5000,
        20000,
        22,
        8,
        denom,
        fee_on_input,
        true,
    );
    Ok(finish_scale(
        wide,
        xp,
        amp,
        i,
        j,
        fee,
        rates,
        &hold,
        rows,
        denom,
        fee_on_input,
    ))
}

pub(crate) fn curve_book(
    state: &Value,
) -> Result<Option<(Vec<BigInt>, BigInt, BigInt, Vec<BigInt>)>, String> {
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let amp = amount_or_zero(state.get("amp"))?;
    let mut fee = amount_or_zero(state.get("fee"))?;
    if fee > fee_denom() {
        fee = BigInt::zero();
    }
    let mut rates = Vec::new();
    let mut balances = Vec::new();
    for coin in coins {
        rates.push(coin_field(coin, "rate")?);
        balances.push(coin_field(coin, "balance")?);
    }
    if amp <= BigInt::zero() || balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let mut xp = Vec::new();
    for (balance, rate) in balances.iter().zip(&rates) {
        xp.push(if rate.is_zero() {
            balance.clone()
        } else {
            fd(&(balance * rate), &wad())
        });
    }
    Ok(Some((xp, amp, fee, rates)))
}

pub(crate) fn stable_scale_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some((xp, amp, fee, rates)) = curve_book(state)? else {
        return Ok(None);
    };
    scale_candidate(
        &xp,
        &amp,
        i,
        j,
        &fee,
        &rates,
        direction,
        rows,
        &fee_denom(),
        false,
    )
}

pub(crate) fn stable_output_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some((xp, amp, fee, rates)) = curve_book(state)? else {
        return Ok(None);
    };
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    let mut best: Option<(i64, BigInt)> = None;
    let score_of = |best: &mut Option<(i64, BigInt)>, ppm: &BigInt| -> Option<i64> {
        let stored = ppm.clone();
        let xp = xp.clone();
        let amp = amp.clone();
        let fee = fee.clone();
        let rates = rates.clone();
        let predict: Predict = Arc::new(move |dx: BigInt| {
            let dy = stable_flat_quote(&xp, &amp, i, j, &fee, &rates, &dx)?;
            Some(fd(&(&dy * &stored), &bi(10_000)))
        });
        let score = worst_bps(&|dx| predict(dx.clone()), target)?;
        if best.as_ref().map(|item| score < item.0).unwrap_or(true) {
            *best = Some((score, ppm.clone()));
        }
        Some(score)
    };
    for ppm in [10000, 9980, 10020, 9960, 10040, 9940, 10060, 9900, 10100] {
        score_of(&mut best, &bi(ppm));
        if best.as_ref().is_some_and(|item| item.0 <= 10) {
            break;
        }
    }
    if best.as_ref().is_none_or(|item| item.0 > 10) {
        let mut left = bi(9000);
        let mut right = bi(11000);
        for _step in 0..16 {
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                break;
            }
            if &right - &left < bi(4) {
                break;
            }
            let (first, second) = ternary_points(&left, &right);
            let left_score = score_of(&mut best, &first);
            let right_score = score_of(&mut best, &second);
            if left_score.is_none() && right_score.is_none() {
                break;
            }
            if right_score.is_none()
                || left_score.is_some_and(|left| right_score.is_none_or(|right| left <= right))
            {
                right = second;
            } else {
                left = first;
            }
        }
    }
    let Some((train_score, ppm)) = best else {
        return Ok(None);
    };
    if train_score > 10 {
        return Ok(None);
    }
    let formula = Formula::StableOutput {
        xp: xp.clone(),
        amp: amp.clone(),
        i: i as i64,
        j: j as i64,
        fee: fee.clone(),
        rates: rates.clone(),
        ppm,
    };
    let quote = formula.predict();
    if !hold.is_empty() && worst_bps(&|dx| quote(dx.clone()), &hold).is_none_or(|score| score > 10)
    {
        return Ok(None);
    }
    match worst_bps(&|dx| quote(dx.clone()), rows) {
        Some(score) if score <= 10 => Ok(Some((score, formula))),
        _ => Ok(None),
    }
}

pub(crate) fn refine_precs(
    balances: &[BigInt],
    rates: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    target: &[(BigInt, BigInt)],
) -> Option<(i64, Vec<BigInt>, Vec<BigInt>)> {
    let choices = choices_for(rates);
    let mut best: Option<(i64, Vec<BigInt>, Vec<BigInt>)> = None;
    let walk = |prefix: &mut Vec<BigInt>, best: &mut Option<(i64, Vec<BigInt>, Vec<BigInt>)>| {
        fn rec(
            choices: &[Vec<BigInt>],
            balances: &[BigInt],
            rates: &[BigInt],
            amp: &BigInt,
            i: usize,
            j: usize,
            fee: &BigInt,
            target: &[(BigInt, BigInt)],
            prefix: &mut Vec<BigInt>,
            best: &mut Option<(i64, Vec<BigInt>, Vec<BigInt>)>,
        ) {
            if prefix.len() == balances.len() {
                let xp: Vec<BigInt> = balances
                    .iter()
                    .zip(rates)
                    .zip(prefix.iter())
                    .map(|((balance, rate), prec)| xp_of(balance, rate, prec))
                    .collect();
                let prec_i = prefix[i].clone();
                let prec_j = prefix[j].clone();
                let rate_i = rates[i].clone();
                let rate_j = rates[j].clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let denom = fee_denom();
                let i_bi = bi(i as i64);
                let j_bi = bi(j as i64);
                let quoted = xp.clone();
                let predict: Predict = Arc::new(move |dx: BigInt| {
                    let paid = dxp_of(&dx, &rate_i, &prec_i);
                    let dy = stable_swap_out(
                        &quoted,
                        amp.clone(),
                        i_bi.clone(),
                        j_bi.clone(),
                        paid,
                        fee.clone(),
                        denom.clone(),
                        false,
                        BigInt::zero(),
                    )?;
                    Some(unscale_out(&dy, &rate_j, &prec_j))
                });
                let Some(score) = worst_bps(&|dx| predict(dx.clone()), target) else {
                    return;
                };
                if best.as_ref().map(|item| score < item.0).unwrap_or(true) {
                    *best = Some((score, prefix.clone(), xp));
                }
                return;
            }
            let options = choices[prefix.len()].clone();
            for prec in options {
                prefix.push(prec);
                rec(
                    choices, balances, rates, amp, i, j, fee, target, prefix, best,
                );
                prefix.pop();
            }
        }
        rec(
            choices.as_slice(),
            balances,
            rates,
            amp,
            i,
            j,
            fee,
            target,
            prefix,
            best,
        );
    };
    walk(&mut Vec::new(), &mut best);
    best
}

pub(crate) struct RelHit {
    pub(crate) rel: BigRational,
    pub(crate) formula: Formula,
}

pub(crate) fn rel_better(best: &Option<RelHit>, rel: &BigRational) -> bool {
    best.as_ref().map(|item| rel < &item.rel).unwrap_or(true)
}

pub(crate) fn coord_search(
    mut score_of: impl FnMut(&[BigInt]) -> Option<RelHit>,
    start: &[BigInt],
    bounds: &[(BigInt, BigInt)],
    steps: usize,
    rounds: usize,
) -> Option<RelHit> {
    let mut best: Option<RelHit> = None;
    let mut params = start.to_vec();
    for _round in 0..rounds {
        for (idx, (lo0, hi0)) in bounds.iter().enumerate() {
            let mut left = lo0.clone();
            let mut right = hi0.clone();
            for _step in 0..steps {
                if &right - &left < bi(4) {
                    break;
                }
                let (first, second) = ternary_points(&left, &right);
                let mut left_try = params.clone();
                let mut right_try = params.clone();
                left_try[idx] = first.clone();
                right_try[idx] = second.clone();
                let left_got = score_of(&left_try);
                let right_got = score_of(&right_try);
                let left_score = left_got.as_ref().map(|hit| hit.rel.clone());
                let right_score = right_got.as_ref().map(|hit| hit.rel.clone());
                if let Some(hit) = left_got {
                    if rel_better(&best, &hit.rel) {
                        best = Some(hit);
                    }
                }
                if let Some(hit) = right_got {
                    if rel_better(&best, &hit.rel) {
                        best = Some(hit);
                    }
                }
                if left_score.is_none() && right_score.is_none() {
                    break;
                }
                let left_better = right_score.is_none()
                    || left_score
                        .as_ref()
                        .is_some_and(|left| right_score.as_ref().is_none_or(|right| left <= right));
                if left_better {
                    right = second;
                    params[idx] = first;
                } else {
                    left = first;
                    params[idx] = second;
                }
                if best
                    .as_ref()
                    .is_some_and(|hit| hit.rel == BigRational::zero())
                {
                    return best;
                }
            }
        }
    }
    best
}

pub(crate) const JOINT_EXPS: [i32; 5] = [-2, -1, 0, 1, 2];

pub(crate) fn i64s(ppm: &[BigInt]) -> Option<Vec<i64>> {
    ppm.iter().map(|item| item.to_i64()).collect()
}

pub(crate) fn stable_ppms_predict(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    ppms: &[BigInt],
) -> Formula {
    let scaled: Vec<BigInt> = xp
        .iter()
        .zip(ppms)
        .map(|(value, ppm)| fd(&(value * ppm), &bi(10_000)))
        .collect();
    Formula::StablePpms {
        xp: scaled,
        amp: amp.clone(),
        i: i as i64,
        j: j as i64,
        fee: fee.clone(),
        rates: rates.to_vec(),
        ppms: ppms.to_vec(),
    }
}

pub(crate) fn joint_amps(amp0: &BigInt) -> Vec<BigInt> {
    let seeds = [
        amp0.clone(),
        fd(amp0, &bi(10)),
        amp0 * bi(10),
        bi(100),
        bi(1000),
        bi(10_000),
        bi(100_000),
    ];
    let mut amps = Vec::new();
    for amp in seeds {
        if amp.is_positive() && !amps.contains(&amp) {
            amps.push(amp);
        }
    }
    amps
}

pub(crate) fn joint_ppm(exp: i32) -> BigInt {
    pow10((4 + exp) as u32)
}

pub(crate) fn score_ppms(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    ppms: &[BigInt],
    target: &[(BigInt, BigInt)],
) -> Option<i64> {
    let quote = stable_ppms_predict(xp, amp, i, j, fee, rates, ppms).predict();
    worst_bps(&|dx| quote(dx.clone()), target)
}

pub(crate) struct JointFound {
    score: i64,
    amp: BigInt,
    ppms: Vec<BigInt>,
}

pub(crate) fn refine_joint(
    xp: &[BigInt],
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    target: &[(BigInt, BigInt)],
    mut best: JointFound,
    steps: usize,
    gap: i64,
    stop_at_bar: bool,
) -> JointFound {
    let mut chosen = best.ppms.clone();
    let amp = best.amp.clone();
    for _round in 0..2 {
        for coin in 0..xp.len() {
            let center = chosen[coin].clone();
            let mut left = max_bi(BigInt::one(), fd(&center, &bi(4)));
            let mut right = &center * bi(4);
            let mut local: Option<(i64, BigInt)> = None;
            for _step in 0..steps {
                if &right - &left < bi(gap) {
                    break;
                }
                let (first, second) = ternary_points(&left, &right);
                let mut left_trial = chosen.clone();
                let mut right_trial = chosen.clone();
                left_trial[coin] = first.clone();
                right_trial[coin] = second.clone();
                let left_score = score_ppms(xp, &amp, i, j, fee, rates, &left_trial, target);
                let right_score = score_ppms(xp, &amp, i, j, fee, rates, &right_trial, target);
                if left_score.is_none() && right_score.is_none() {
                    break;
                }
                let left_better = right_score.is_none()
                    || left_score.is_some_and(|left_score| {
                        right_score.is_none_or(|right_score| left_score <= right_score)
                    });
                if left_better {
                    right = second;
                    if let Some(score) = left_score {
                        if local.as_ref().map(|item| score < item.0).unwrap_or(true) {
                            local = Some((score, first));
                        }
                    }
                } else {
                    left = first;
                    if let Some(score) = right_score {
                        if local.as_ref().map(|item| score < item.0).unwrap_or(true) {
                            local = Some((score, second));
                        }
                    }
                }
                if stop_at_bar && local.as_ref().is_some_and(|item| item.0 <= 10) {
                    break;
                }
            }
            let Some((local_score, ppm)) = local else {
                continue;
            };
            chosen[coin] = ppm;
            if local_score < best.score {
                best = JointFound {
                    score: local_score,
                    amp: amp.clone(),
                    ppms: chosen.clone(),
                };
            }
            if stop_at_bar && best.score <= 10 {
                return best;
            }
        }
    }
    best
}

pub(crate) fn remember(best: &mut Option<JointFound>, score: i64, amp: &BigInt, ppms: Vec<BigInt>) {
    if best.as_ref().map(|item| score < item.score).unwrap_or(true) {
        *best = Some(JointFound {
            score,
            amp: amp.clone(),
            ppms,
        });
    }
}

pub(crate) fn joint_search(
    xp: &[BigInt],
    amp0: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    target: &[(BigInt, BigInt)],
) -> Option<JointFound> {
    let n = xp.len();
    let mut best: Option<JointFound> = None;
    for amp in joint_amps(amp0) {
        let base = vec![bi(10_000); n];
        if let Some(score) = score_ppms(xp, &amp, i, j, fee, rates, &base, target) {
            remember(&mut best, score, &amp, base);
        }
        if best.as_ref().is_some_and(|item| item.score <= 10) {
            return best;
        }
        for left_coin in 0..n {
            for right_coin in (left_coin + 1)..n {
                for left_exp in JOINT_EXPS {
                    for right_exp in JOINT_EXPS {
                        let mut trial = vec![bi(10_000); n];
                        trial[left_coin] = joint_ppm(left_exp);
                        trial[right_coin] = joint_ppm(right_exp);
                        let Some(score) = score_ppms(xp, &amp, i, j, fee, rates, &trial, target)
                        else {
                            continue;
                        };
                        remember(&mut best, score, &amp, trial);
                        if best.as_ref().is_some_and(|item| item.score <= 10) {
                            return best;
                        }
                    }
                }
            }
        }
    }
    best.map(|found| refine_joint(xp, i, j, fee, rates, target, found, 14, 8, true))
}

pub(crate) fn joint_best(
    xp: &[BigInt],
    amp0: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    target: &[(BigInt, BigInt)],
) -> Option<JointFound> {
    let n = xp.len();
    let mut best: Option<JointFound> = None;
    for amp in joint_amps(amp0) {
        let mut seen = HashSet::new();
        let mut points = vec![vec![bi(10_000); n]];
        for left_coin in 0..n {
            for right_coin in (left_coin + 1)..n {
                for left_exp in JOINT_EXPS {
                    for right_exp in JOINT_EXPS {
                        let mut trial = vec![bi(10_000); n];
                        trial[left_coin] = joint_ppm(left_exp);
                        trial[right_coin] = joint_ppm(right_exp);
                        points.push(trial);
                    }
                }
            }
        }
        for trial in points {
            if !seen.insert(trial.clone()) {
                continue;
            }
            let Some(score) = score_ppms(xp, &amp, i, j, fee, rates, &trial, target) else {
                continue;
            };
            remember(&mut best, score, &amp, trial);
        }
    }
    best.map(|found| refine_joint(xp, i, j, fee, rates, target, found, 18, 4, false))
}

pub(crate) fn joint_state(
    state: &Value,
) -> Result<Option<(Vec<BigInt>, BigInt, BigInt, Vec<BigInt>)>, String> {
    let Some(coins) = state.get("coins").and_then(Value::as_array) else {
        return Ok(None);
    };
    if coins.len() < 3 {
        return Ok(None);
    }
    curve_book(state)
}

pub(crate) fn finish_joint(
    found: Option<JointFound>,
    xp: &[BigInt],
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    hold: &[(BigInt, BigInt)],
    rows: &[(BigInt, BigInt)],
) -> Option<Scored> {
    let found = found?;
    if found.score > 10 {
        return None;
    }
    gated(
        &stable_ppms_predict(xp, &found.amp, i, j, fee, rates, &found.ppms),
        hold,
        rows,
    )
}

pub(crate) fn stable_joint_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some((xp, amp, fee, rates)) = joint_state(state)? else {
        return Ok(None);
    };
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    Ok(finish_joint(
        joint_search(&xp, &amp, i, j, &fee, &rates, target),
        &xp,
        i,
        j,
        &fee,
        &rates,
        &hold,
        rows,
    ))
}

pub(crate) fn stable_joint_best_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some((xp, amp, fee, rates)) = joint_state(state)? else {
        return Ok(None);
    };
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    Ok(finish_joint(
        joint_best(&xp, &amp, i, j, &fee, &rates, target),
        &xp,
        i,
        j,
        &fee,
        &rates,
        &hold,
        rows,
    ))
}

pub(crate) fn stable_refine_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let coins = state
        .get("coins")
        .and_then(Value::as_array)
        .ok_or_else(|| "coins".to_string())?;
    let amp0 = amount_or_zero(state.get("amp"))?;
    let mut fee0 = amount_or_zero(state.get("fee"))?;
    if fee0 > fee_denom() {
        fee0 = BigInt::zero();
    }
    let mut rates = Vec::new();
    let mut balances = Vec::new();
    for coin in coins {
        rates.push(coin_field(coin, "rate")?);
        balances.push(coin_field(coin, "balance")?);
    }
    if amp0 <= BigInt::zero() || balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows).to_vec();
    let Some((_stored, precs, base_xp)) =
        refine_precs(&balances, &rates, &amp0, i, j, &fee0, &target)
    else {
        return Ok(None);
    };
    let amp_lo = max_bi(BigInt::one(), fd(&(&amp0 * bi(9)), &bi(10)));
    let amp_hi = max_bi(fd(&(&amp0 * bi(11)), &bi(10)), &amp0 + bi(4));
    let fee_hi = max_bi(&fee0 * bi(4), bi(8_000_000));
    let mut best: Option<RelHit> = None;
    let mut offpegs = vec![BigInt::zero()];
    offpegs.extend(offpeg_list());
    for offpeg in &offpegs {
        let score_of = |ppm: &[BigInt]| -> Option<RelHit> {
            if ppm.len() != 5 {
                return None;
            }
            let (amp, ppm_i, ppm_j, out_ppm, fee) = (
                ppm[0].clone(),
                ppm[1].clone(),
                ppm[2].clone(),
                ppm[3].clone(),
                ppm[4].clone(),
            );
            if amp <= BigInt::zero()
                || ppm_i <= BigInt::zero()
                || ppm_j <= BigInt::zero()
                || out_ppm <= BigInt::zero()
                || fee.is_negative()
                || fee > fee_denom()
            {
                return None;
            }
            let mut xp = base_xp.clone();
            xp[i] = fd(&(&base_xp[i] * &ppm_i), &bi(10_000));
            xp[j] = fd(&(&base_xp[j] * &ppm_j), &bi(10_000));
            let formula = Formula::StableRefine {
                xp,
                amp,
                i: i as i64,
                j: j as i64,
                fee,
                offpeg: offpeg.clone(),
                rate_i: rates[i].clone(),
                prec_i: precs[i].clone(),
                ppm_i,
                ppm_j,
                rate_j: rates[j].clone(),
                prec_j: precs[j].clone(),
                out_ppm,
            };
            Some(RelHit {
                rel: worst_rel(&formula.predict(), &target)?,
                formula,
            })
        };
        for start_fee in [fee0.clone(), max_bi(fd(&fee0, &bi(4)), BigInt::zero())] {
            let start = [amp0.clone(), bi(10_000), bi(10_000), bi(10_000), start_fee];
            let bounds = [
                (amp_lo.clone(), amp_hi.clone()),
                (bi(9000), bi(11_000)),
                (bi(9000), bi(11_000)),
                (bi(9900), bi(10_100)),
                (BigInt::zero(), fee_hi.clone()),
            ];
            if let Some(got) = coord_search(score_of, &start, &bounds, 14, 2) {
                if rel_better(&best, &got.rel) {
                    best = Some(got);
                }
            }
            if best.as_ref().is_some_and(|hit| hit.rel.is_zero()) {
                break;
            }
        }
        if best.as_ref().is_some_and(|hit| hit.rel.is_zero()) {
            break;
        }
    }
    Ok(best.and_then(|hit| accept_cap(hit.formula, &hold, rows)))
}
