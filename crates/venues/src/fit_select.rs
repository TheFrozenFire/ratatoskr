//! Pool-direction selector. The search order matches `cas/select_fit.py`.

#![allow(dead_code)]
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use bigdecimal::{BigDecimal, Context, RoundingMode};
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::sync::Arc;

use crate::fit_formula::Formula;
use crate::fit_math::{
    added_residual, bin_out, chord_quote, concentrated_out, crypto2_out, crypto3_out, crypto_scale,
    crypto_y3, cubic_quote, flat_pair_out, flat_scale, hyperbola_out, product_out, residual_quote,
    stable_swap_out, weighted_out, worst_bps, Predict, A_MULTIPLIER, DUST, FEE_1E10, PRECISIONS, WAD,
};

type Scored = (i64, Formula);

pub struct Chosen {
    pub score: i64,
    pub name: &'static str,
    pub formula: Formula,
}

struct Found {
    score: i64,
    rank: i64,
    name: &'static str,
    formula: Formula,
}

const PREC_40: NonZeroU64 = NonZeroU64::new(40).unwrap();

// Holdout fractions are exactly these strings.
const HOLD_FRACTIONS: [&str; 2] = ["0.25", "0.8"];

fn wad() -> BigInt {
    (*WAD).clone()
}

fn fee_denom() -> BigInt {
    (*FEE_1E10).clone()
}

fn a_multiplier() -> BigInt {
    (*A_MULTIPLIER).clone()
}

fn dust() -> BigInt {
    (*DUST).clone()
}

fn precisions() -> Vec<BigInt> {
    PRECISIONS.iter().cloned().collect()
}

fn bi(value: i64) -> BigInt {
    BigInt::from(value)
}

fn pow10(exp: u32) -> BigInt {
    let exp = exp.min(4_096);
    let mut acc = BigInt::one();
    let mut base = BigInt::from(10);
    let mut left = exp;
    while left > 0 {
        if left & 1 == 1 {
            acc *= &base;
        }
        left >>= 1;
        if left > 0 {
            base = &base * &base;
        }
    }
    acc
}

fn rat_i(value: i64) -> BigRational {
    BigRational::from_integer(bi(value))
}

fn fd(numer: &BigInt, denom: &BigInt) -> BigInt {
    if denom.is_zero() {
        BigInt::zero()
    } else {
        numer.div_floor(denom)
    }
}

fn trunc_rat(value: &BigRational) -> BigInt {
    value.numer() / value.denom()
}

fn ctx_at(precision: NonZeroU64) -> Context {
    Context::new(precision, RoundingMode::HalfEven)
}

fn dec_ctx() -> Context {
    ctx_at(PREC_40)
}

fn rank_of(name: &str) -> i64 {
    match name {
        "product" => 0,
        "ladder" => 1,
        "stable" => 2,
        "two-coin" => 3,
        "flat-pair" => 4,
        "three-coin" => 5,
        "weighted" => 6,
        "balancer-stable" => 7,
        "bins" => 8,
        "hyperbola" => 9,
        "chord" => 10,
        "cubic" => 11,
        "residual" => 12,
        "cap" => 13,
        _ => i64::MAX,
    }
}

fn consider(found: &mut Vec<Found>, name: &'static str, got: &Option<Scored>) {
    let Some((score, formula)) = got else {
        return;
    };
    found.push(Found {
        score: *score,
        rank: rank_of(name),
        name,
        formula: formula.clone(),
    });
}

fn score_predict(formula: Formula, rows: &[(BigInt, BigInt)]) -> Option<Scored> {
    let score = worst_bps(formula.predict().as_ref(), rows)?;
    Some((score, formula))
}

pub(crate) fn eval_formula(formula: &Formula, amount: &BigInt) -> Option<BigInt> {
    formula.predict()(amount.clone())
}

impl Formula {
    pub(crate) fn predict(&self) -> Predict {
        match self {
            Self::Hyperbola { x1, y1, x2, y2 } => {
                let (x1, y1, x2, y2) = (x1.clone(), y1.clone(), x2.clone(), y2.clone());
                Arc::new(move |dx: BigInt| hyperbola_out(&x1, &y1, &x2, &y2, &dx))
            }
            Self::Stable {
                xp,
                amp,
                i,
                j,
                fee,
                denom,
                offpeg,
                rate_i,
                rate_j,
                prec_i,
                prec_j,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let denom = denom.clone();
                let offpeg = offpeg.clone();
                let rate_i = rate_i.clone();
                let rate_j = rate_j.clone();
                let prec_i = prec_i.clone();
                let prec_j = prec_j.clone();
                let i_bi = bi(*i);
                let j_bi = bi(*j);
                Arc::new(move |dx: BigInt| {
                    let dxp = dxp_of(&dx, &rate_i, &prec_i);
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        i_bi.clone(),
                        j_bi.clone(),
                        dxp,
                        fee.clone(),
                        denom.clone(),
                        false,
                        offpeg.clone(),
                    )?;
                    Some(unscale_out(&dy, &rate_j, &prec_j))
                })
            }
            Self::Crypto2 {
                balances,
                precs,
                scale,
                ann,
                gamma,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scale = scale.clone();
                let ann = ann.clone();
                let gamma = gamma.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                Arc::new(move |dx: BigInt| {
                    crypto2_out(
                        &balances,
                        &precs,
                        &scale,
                        &ann,
                        &gamma,
                        &d,
                        i_idx,
                        j_idx,
                        &dx,
                        &mid,
                        &out_fee,
                        &fee_gamma,
                    )
                })
            }
            Self::FlatPair {
                balances,
                precs,
                scale,
                amp,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
                flat_fee,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scale = scale.clone();
                let amp = amp.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                let flat_fee = *flat_fee;
                Arc::new(move |dx: BigInt| {
                    flat_pair_out(
                        &balances,
                        &precs,
                        &scale,
                        &amp,
                        &d,
                        i_idx,
                        j_idx,
                        &dx,
                        &mid,
                        &out_fee,
                        &fee_gamma,
                        flat_fee,
                    )
                })
            }
            Self::Crypto3 {
                balances,
                precs,
                scales,
                ann,
                gamma,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scales = scales.clone();
                let ann = ann.clone();
                let gamma = gamma.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                Arc::new(move |dx: BigInt| {
                    crypto3_out(
                        &balances,
                        &precs,
                        &scales,
                        &ann,
                        &gamma,
                        &d,
                        i_idx,
                        j_idx,
                        &dx,
                        &mid,
                        &out_fee,
                        &fee_gamma,
                    )
                })
            }
            Self::StableScale {
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                which,
                ppm,
                denom,
                fee_on_input,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppm = ppm.clone();
                let denom = denom.clone();
                let Some(i) = rate_index(rates.len(), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len(), *j) else {
                    return Arc::new(|_| None);
                };
                let which = *which as usize;
                let fee_on_input = *fee_on_input;
                Arc::new(move |dx: BigInt| {
                    if fee_on_input {
                        let paid = if which == i { fd(&(&dx * &ppm), &bi(10_000)) } else { dx };
                        let mut dy = stable_swap_out(
                            &xp,
                            amp.clone(),
                            bi(i as i64),
                            bi(j as i64),
                            paid,
                            fee.clone(),
                            denom.clone(),
                            true,
                            BigInt::zero(),
                        )?;
                        if which == j {
                            dy = fd(&(&dy * &bi(10_000)), &ppm);
                        }
                        return Some(dy);
                    }
                    let mut paid = if rates[i].is_zero() { dx } else { fd(&(&dx * &rates[i]), &wad()) };
                    if which == i {
                        paid = fd(&(&paid * &ppm), &bi(10_000));
                    }
                    let mut dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i as i64),
                        bi(j as i64),
                        paid,
                        fee.clone(),
                        denom.clone(),
                        false,
                        BigInt::zero(),
                    )?;
                    if which == j {
                        dy = fd(&(&dy * &bi(10_000)), &ppm);
                    }
                    if rates[j].is_zero() {
                        Some(dy)
                    } else {
                        Some(fd(&(&dy * &wad()), &rates[j]))
                    }
                })
            }
            Self::StableOutput { xp, amp, i, j, fee, rates, ppm } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppm = ppm.clone();
                let Some(i) = rate_index(rates.len(), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len(), *j) else {
                    return Arc::new(|_| None);
                };
                Arc::new(move |dx: BigInt| {
                    let dy = stable_flat_quote(&xp, &amp, i, j, &fee, &rates, &dx)?;
                    Some(fd(&(&dy * &ppm), &bi(10_000)))
                })
            }
            Self::StablePpms { xp, amp, i, j, fee, rates, ppms } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppms = ppms.clone();
                let Some(i) = rate_index(rates.len().min(ppms.len()), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len().min(ppms.len()), *j) else {
                    return Arc::new(|_| None);
                };
                Arc::new(move |dx: BigInt| {
                    let mut paid = if rates[i].is_zero() { dx } else { fd(&(&dx * &rates[i]), &wad()) };
                    paid = fd(&(&paid * &ppms[i]), &bi(10_000));
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i as i64),
                        bi(j as i64),
                        paid,
                        fee.clone(),
                        fee_denom(),
                        false,
                        BigInt::zero(),
                    )?;
                    let dy = fd(&(&dy * bi(10_000)), &ppms[j]);
                    if rates[j].is_zero() {
                        Some(dy)
                    } else {
                        Some(fd(&(&dy * &wad()), &rates[j]))
                    }
                })
            }
            Self::StableRefine {
                xp,
                amp,
                i,
                j,
                fee,
                offpeg,
                rate_i,
                prec_i,
                ppm_i,
                ppm_j,
                rate_j,
                prec_j,
                out_ppm,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let offpeg = offpeg.clone();
                let rate_i = rate_i.clone();
                let prec_i = prec_i.clone();
                let ppm_i = ppm_i.clone();
                let ppm_j = ppm_j.clone();
                let rate_j = rate_j.clone();
                let prec_j = prec_j.clone();
                let out_ppm = out_ppm.clone();
                let i = *i;
                let j = *j;
                Arc::new(move |dx: BigInt| {
                    let paid = if rate_i.is_zero() {
                        fd(&(&dx * &prec_i * &ppm_i), &bi(10_000))
                    } else {
                        fd(&(fd(&(&dx * &rate_i), &wad()) * &prec_i * &ppm_i), &bi(10_000))
                    };
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i),
                        bi(j),
                        paid,
                        fee.clone(),
                        fee_denom(),
                        false,
                        offpeg.clone(),
                    )?;
                    let scaled = fd(&fd(&(&dy * bi(10_000)), &ppm_j), &prec_j);
                    let scaled = fd(&(&scaled * &out_ppm), &bi(10_000));
                    if rate_j.is_zero() {
                        Some(scaled)
                    } else {
                        Some(fd(&(&scaled * &wad()), &rate_j))
                    }
                })
            }
            Self::Product { reserve_in, reserve_out, fee, denom } => {
                let reserve_in = reserve_in.clone();
                let reserve_out = reserve_out.clone();
                let fee = fee.clone();
                let denom = denom.clone();
                Arc::new(move |dx: BigInt| product_out(&reserve_in, &reserve_out, &dx, &fee, &denom))
            }
            Self::Weighted {
                balance_in,
                balance_out,
                weight_in,
                weight_out,
                fee,
            } => {
                let balance_in = balance_in.clone();
                let balance_out = balance_out.clone();
                let weight_in = weight_in.clone();
                let weight_out = weight_out.clone();
                let fee = fee.clone();
                Arc::new(move |dx: BigInt| {
                    weighted_out(&balance_in, &balance_out, &weight_in, &weight_out, &dx, &fee)
                })
            }
            Self::BalancerStable { xp, amp, i, j, fee, prec_i, prec_j } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let prec_i = prec_i.clone();
                let prec_j = prec_j.clone();
                let i = *i;
                let j = *j;
                Arc::new(move |dx: BigInt| {
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i),
                        bi(j),
                        &dx * &prec_i,
                        fee.clone(),
                        wad(),
                        true,
                        BigInt::zero(),
                    )?;
                    Some(fd(&dy, &prec_j))
                })
            }
            Self::Ladder { sqrt_price, liquidity, tick, fee, ticks, zero_for_one } => {
                let sqrt_price = sqrt_price.clone();
                let liquidity = liquidity.clone();
                let tick = tick.clone();
                let fee = fee.clone();
                let ticks = ticks.clone();
                let zero_for_one = *zero_for_one;
                Arc::new(move |dx: BigInt| {
                    concentrated_out(&sqrt_price, &liquidity, &tick, &fee, &ticks, zero_for_one, &dx)
                })
            }
            Self::Bins {
                reference,
                distance,
                length,
                position,
                selling0,
                scaled_out,
                scale_out,
            } => {
                let reference = reference.clone();
                let distance = distance.clone();
                let length = length.clone();
                let position = position.clone();
                let selling0 = *selling0;
                let scaled_out = scaled_out.clone();
                let scale_out = scale_out.clone();
                Arc::new(move |dx: BigInt| {
                    bin_out(
                        &reference,
                        &distance,
                        &length,
                        &position,
                        &dx,
                        selling0,
                        &scaled_out,
                        &scale_out,
                    )
                })
            }
            Self::Chord { knots } => knot_predict(chord_quote(knots)),
            Self::Cubic { knots } => knot_predict(cubic_quote(knots)),
            Self::HyperResidual { knots } => knot_predict(residual_quote(knots)),
            Self::Added { base, knots, alpha } => {
                let built = added_residual(base.predict(), knots, alpha);
                knot_predict(built)
            }
            Self::ScaledResidual { knots, ppm } => {
                let Some(base) = residual_quote(knots) else {
                    return Arc::new(|_| None);
                };
                let ppm = ppm.clone();
                Arc::new(move |amount: BigInt| {
                    let value = base(amount)?;
                    Some(fd(&(&value * &ppm), &bi(10_000)))
                })
            }
            Self::Fillet { slope, cap, xa, xb } => fillet_quote(slope.clone(), cap.clone(), xa.clone(), xb.clone()),
            Self::Logistic { slope, rate, knee } => logistic_quote(slope, rate, knee),
            Self::Scaled { base, ppm } => scale_out(base.predict(), *ppm),
            Self::Wave { base, cap, x_last, a_ppm, b_ppm } => {
                wave_quote(base.predict(), cap, x_last, *a_ppm, *b_ppm)
            }
            Self::Identity => Arc::new(Some),
            Self::Lookup { pairs } => {
                let pairs = pairs.clone();
                Arc::new(move |dx: BigInt| pairs.iter().find(|(amount, _)| amount == &dx).map(|(_, out)| out.clone()))
            }
        }
    }
}

fn knot_predict(predict: Option<Predict>) -> Predict {
    predict.unwrap_or_else(|| Arc::new(|_| None))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                value != 0
            } else if let Some(value) = number.as_u64() {
                value != 0
            } else if let Some(value) = number.as_f64() {
                value != 0.0
            } else {
                true
            }
        }
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

pub fn amount(value: Option<&Value>) -> Result<Option<BigInt>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(text) => text
            .parse::<BigInt>()
            .map(Some)
            .map_err(|_| "amount must be a decimal string".to_string()),
        _ => Err("amount must be a decimal string".to_string()),
    }
}

fn amount_or_zero(value: Option<&Value>) -> Result<BigInt, String> {
    Ok(amount(value)?.unwrap_or_else(BigInt::zero))
}

/// `primary or fallback`, where zero and null fall through.
fn amount_or(primary: Option<&Value>, fallback: BigInt) -> Result<BigInt, String> {
    match amount(primary)? {
        Some(value) if !value.is_zero() => Ok(value),
        _ => Ok(fallback),
    }
}

pub fn whole(value: Option<&Value>) -> Result<Option<BigInt>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Bool(_) => Err("flag is not an integer".to_string()),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                return Ok(Some(BigInt::from(value)));
            }
            if let Some(value) = number.as_u64() {
                return Ok(Some(BigInt::from(value)));
            }
            let text = number.to_string();
            if text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                return Err("expected an integer".to_string());
            }
            text.parse::<BigInt>()
                .map(Some)
                .map_err(|_| "expected an integer".to_string())
        }
        Value::String(text) => text
            .parse::<BigInt>()
            .map(Some)
            .map_err(|_| "expected an integer".to_string()),
        _ => Err("expected an integer".to_string()),
    }
}

pub fn labels_of(direction: &Value) -> Result<Vec<(BigInt, BigInt)>, String> {
    let Some(samples) = direction.get("samples") else {
        return Err("samples must be a list".to_string());
    };
    let Some(samples) = samples.as_array() else {
        return Err("samples must be a list".to_string());
    };
    let mut rows = Vec::new();
    for sample in samples {
        let Some(sample) = sample.as_object() else {
            return Err("sample must be an object".to_string());
        };
        let x = amount(sample.get("amount_in"))?;
        let y = amount(sample.get("chain_out"))?;
        let (Some(x), Some(y)) = (x, y) else {
            continue;
        };
        if x <= BigInt::zero() || y <= BigInt::zero() {
            continue;
        }
        rows.push((x, y));
    }
    rows.sort();
    Ok(rows)
}

fn index_of(tokens: &[String], token: Option<&Value>) -> Option<usize> {
    let token = token?.as_str()?;
    let key = token.to_lowercase();
    tokens
        .iter()
        .position(|item| item.to_lowercase() == key)
}

fn string_tokens(coins: &[Value]) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for coin in coins {
        let Some(coin) = coin.as_object() else {
            continue;
        };
        let token = coin.get("token").and_then(Value::as_str)?;
        tokens.push(token.to_string());
    }
    Some(tokens)
}

fn require_object<'a>(value: &'a Value, message: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| message.to_string())
}

fn coin_field(coin: &Value, key: &str) -> Result<BigInt, String> {
    let coin = require_object(coin, "coin")?;
    amount_or_zero(coin.get(key))
}

fn split_holdout(direction: &Value) -> Result<(Vec<(BigInt, BigInt)>, Vec<(BigInt, BigInt)>), String> {
    let Some(samples) = direction.get("samples") else {
        return Err("samples must be a list".to_string());
    };
    let Some(samples) = samples.as_array() else {
        return Err("samples must be a list".to_string());
    };
    let mut train = Vec::new();
    let mut hold = Vec::new();
    for sample in samples {
        let Some(sample) = sample.as_object() else {
            return Err("sample must be an object".to_string());
        };
        let x = amount(sample.get("amount_in"))?;
        let y = amount(sample.get("chain_out"))?;
        let (Some(x), Some(y)) = (x, y) else {
            continue;
        };
        if x <= BigInt::zero() || y <= BigInt::zero() {
            continue;
        }
        let fraction = sample.get("fraction").and_then(Value::as_str);
        if fraction.is_some_and(|fraction| HOLD_FRACTIONS.contains(&fraction)) {
            hold.push((x, y));
        } else {
            train.push((x, y));
        }
    }
    train.sort();
    hold.sort();
    Ok((train, hold))
}

fn train_rows<'a>(
    train: &'a [(BigInt, BigInt)],
    rows: &'a [(BigInt, BigInt)],
) -> &'a [(BigInt, BigInt)] {
    if train.len() >= 2 {
        train
    } else {
        rows
    }
}

fn worse(best: &Option<Scored>, score: i64) -> bool {
    best.as_ref().map(|item| score < item.0).unwrap_or(true)
}

fn take_score(best: &mut Option<Scored>, got: Option<Scored>) {
    let Some(got) = got else {
        return;
    };
    if worse(best, got.0) {
        *best = Some(got);
    }
}

fn best_over(
    n: usize,
    choices: &[Vec<BigInt>],
    mut evaluate: impl FnMut(&[BigInt]) -> Option<Scored>,
) -> Option<Scored> {
    let mut best: Option<Scored> = None;
    if choices.iter().all(|choice| choice.len() == 1) {
        let precs: Vec<BigInt> = choices.iter().map(|choice| choice[0].clone()).collect();
        take_score(&mut best, evaluate(&precs));
        return best;
    }
    for prec in precisions() {
        if choices.iter().all(|choice| choice.iter().any(|item| item == &prec)) {
            let precs = vec![prec; n];
            take_score(&mut best, evaluate(&precs));
            if best.as_ref().is_some_and(|item| item.0 <= 10) {
                return best;
            }
        }
    }
    let mut width = BigInt::one();
    for choice in choices {
        width *= choice.len();
    }
    if width > bi(64) {
        return best;
    }
    fn walk<F>(
        n: usize,
        choices: &[Vec<BigInt>],
        prefix: &mut Vec<BigInt>,
        best: &mut Option<Scored>,
        evaluate: &mut F,
    ) where
        F: FnMut(&[BigInt]) -> Option<Scored>,
    {
        if best.as_ref().is_some_and(|item| item.0 <= 10) {
            return;
        }
        if prefix.len() == n {
            take_score(best, evaluate(prefix));
            return;
        }
        let options = choices[prefix.len()].clone();
        for prec in options {
            prefix.push(prec);
            walk(n, choices, prefix, best, evaluate);
            prefix.pop();
        }
    }
    walk(n, choices, &mut Vec::new(), &mut best, &mut evaluate);
    best
}

fn for_product(choices: &[Vec<BigInt>], mut visit: impl FnMut(&[BigInt])) {
    if choices.is_empty() || choices.iter().any(|choice| choice.is_empty()) {
        return;
    }
    let mut index = vec![0usize; choices.len()];
    loop {
        let current: Vec<BigInt> = index
            .iter()
            .enumerate()
            .map(|(coin, &at)| choices[coin][at].clone())
            .collect();
        visit(&current);
        let mut cursor = choices.len();
        loop {
            if cursor == 0 {
                return;
            }
            cursor -= 1;
            index[cursor] += 1;
            if index[cursor] < choices[cursor].len() {
                break;
            }
            index[cursor] = 0;
        }
    }
}

fn xp_of(balance: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        balance * prec
    } else {
        fd(&(balance * rate), &wad()) * prec
    }
}

fn dxp_of(dx: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        dx * prec
    } else {
        fd(&(dx * rate), &wad()) * prec
    }
}

fn unscale_out(dy: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        fd(dy, prec)
    } else {
        fd(&fd(&(dy * &wad()), rate), prec)
    }
}

fn choices_for(rates: &[BigInt]) -> Vec<Vec<BigInt>> {
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

fn hyperbola_candidate(rows: &[(BigInt, BigInt)]) -> Option<Scored> {
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

// The snapshot does not store an off-peg multiplier.
fn offpeg_list() -> Vec<BigInt> {
    [2, 5, 10, 20, 50, 100, 200, 500, 1000]
        .into_iter()
        .map(|factor| &fee_denom() * bi(factor))
        .collect()
}

fn stable_quote(
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

fn stable_candidate(
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
        stable_quote(&balances, &rates, precs, &amp, i, j, &fee, &BigInt::zero(), rows)
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

fn stable_mixed_candidate(
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
        let Some(score) = worst_bps(formula.predict().as_ref(), target) else {
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
        let hold_score = worst_bps(chosen.predict().as_ref(), &hold);
        if hold_score.is_none_or(|score| score > 10) {
            return Ok(None);
        }
    }
    let scored = if rows.is_empty() { target } else { rows };
    let Some(score) = worst_bps(chosen.predict().as_ref(), scored) else {
        return Ok(None);
    };
    if score > 10 {
        return Ok(None);
    }
    Ok(Some((score, chosen)))
}

fn fee_words(state: &Value) -> Result<(BigInt, BigInt, BigInt), String> {
    let mid = amount_or(state.get("mid_fee"), amount_or(state.get("fee"), BigInt::zero())?)?;
    let out_fee = amount_or(state.get("out_fee"), mid.clone())?;
    let fee_gamma = amount_or_zero(state.get("fee_gamma"))?;
    Ok((mid, out_fee, fee_gamma))
}

fn balances_of(coins: &[Value]) -> Result<Vec<BigInt>, String> {
    let mut balances = Vec::new();
    for coin in coins {
        balances.push(coin_field(coin, "balance")?);
    }
    Ok(balances)
}

fn crypto2_candidate(
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
        let scale = crypto_scale(&ann, &gamma, &d, &balances[0], &balances[1], &precs[0], &precs[1])?;
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

fn flat_pair_candidate(
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
            let Some(score) = worst_bps(formula.predict().as_ref(), rows) else {
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

fn price_scales(coins: &[Value]) -> Result<Option<(BigInt, BigInt)>, String> {
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

fn crypto3_candidate(
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

fn crypto3_predict(
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

fn crypto3_at(
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
    (worst_bps(formula.predict().as_ref(), rows), formula)
}

fn ann_anchors() -> Vec<BigInt> {
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

fn min_ann() -> BigInt {
    bi(2700)
}

fn max_ann() -> BigInt {
    bi(270_000_000)
}

fn max_bi(left: BigInt, right: BigInt) -> BigInt {
    if left > right { left } else { right }
}

fn min_bi(left: BigInt, right: BigInt) -> BigInt {
    if left < right { left } else { right }
}

fn ternary_points(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    let span = right - left;
    let third = fd(&span, &bi(3));
    (left + &third, right - third)
}

fn left_strict(left: Option<i64>, right: Option<i64>) -> bool {
    right.is_none() || left.is_some_and(|left| right.is_none_or(|right| left < right))
}

fn crypto3_fit_candidate(
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
                &balances, &scales, &first, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma, target,
            );
            let (right_score, _) = crypto3_at(
                &balances, &scales, &second, &gamma, &d, i, j, &prec, &mid, &out_fee, &fee_gamma, target,
            );
            if left_score.is_none() && right_score.is_none() {
                break;
            }
            if left_strict(left_score, right_score) {
                right = second;
                if let Some(left_score) = left_score {
                    if best.as_ref().map(|item| left_score < item.0).unwrap_or(true) {
                        best = Some((left_score, first, prec.clone()));
                    }
                }
            } else {
                left = first;
                if let Some(right_score) = right_score {
                    if best.as_ref().map(|item| right_score < item.0).unwrap_or(true) {
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

fn scaled_xp(balances: &[BigInt], precs: &[BigInt], scales: &(BigInt, BigInt)) -> Vec<BigInt> {
    vec![
        &balances[0] * &precs[0],
        fd(&(&balances[1] * &scales.0), &wad()) * &precs[1],
        fd(&(&balances[2] * &scales.1), &wad()) * &precs[2],
    ]
}

fn spread_of(xp: &[BigInt]) -> Option<BigInt> {
    let max = xp.iter().max()?;
    let min = xp.iter().min()?;
    if min <= &BigInt::zero() {
        return None;
    }
    Some(fd(&(max * &bi(1000)), min))
}

fn reserve_gap(ann: &BigInt, gamma: &BigInt, d: &BigInt, xp: &[BigInt]) -> Option<(BigInt, BigInt)> {
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

fn search_consistent_ann(
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
            Some((best_rel, best_total, _)) => &rel < best_rel || (&rel == best_rel && &total < best_total),
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
            || left_rel.as_ref().is_some_and(|left| right_rel.as_ref().is_none_or(|right| left <= right));
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
            || left_total.as_ref().is_some_and(|left| right_total.as_ref().is_none_or(|right| left <= right));
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

fn crypto3_consistent_candidate(
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
            Some((best_rel, best_total, _, _)) => &rel < best_rel || (&rel == best_rel && &total < best_total),
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
            &balances, &scales, &ann, &gamma, &d, i, j, precs.clone(), &mid, &out_fee, &fee_gamma,
        );
        if worst_bps(predict.predict().as_ref(), &hold).is_none_or(|score| score > 10) {
            return Ok(None);
        }
    }
    let scored = if rows.is_empty() { train } else { rows.to_vec() };
    let predict = crypto3_predict(
        &balances, &scales, &ann, &gamma, &d, i, j, precs, &mid, &out_fee, &fee_gamma,
    );
    match worst_bps(predict.predict().as_ref(), &scored) {
        Some(score) if score <= 10 => Ok(Some((score, predict))),
        _ => Ok(None),
    }
}

fn stable_scale_predict(
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

fn stable_scale_at(
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
    (worst_bps(formula.predict().as_ref(), rows), formula)
}

fn scale_window(
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
        let (score, _) = stable_scale_at(xp, amp, i, j, fee, rates, which, &ppm, target, denom, fee_on_input);
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
                xp, amp, i, j, fee, rates, which, &first, target, denom, fee_on_input,
            );
            let (right_score, _) = stable_scale_at(
                xp, amp, i, j, fee, rates, which, &second, target, denom, fee_on_input,
            );
            if left_score.is_none() && right_score.is_none() {
                break;
            }
            let left_better = match (left_score, right_score) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(left), Some(right)) => {
                    if prefer_left { left <= right } else { left < right }
                }
            };
            if left_better {
                right = second;
                if let Some(left_score) = left_score {
                    if best.as_ref().map(|item| left_score < item.0).unwrap_or(true) {
                        best = Some((left_score, which, first));
                    }
                }
            } else {
                left = first;
                if let Some(right_score) = right_score {
                    if best.as_ref().map(|item| right_score < item.0).unwrap_or(true) {
                        best = Some((right_score, which, second));
                    }
                }
            }
        }
    }
    best
}

fn finish_scale(
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
        let (hold_score, _) = stable_scale_at(xp, amp, i, j, fee, rates, which, &ppm, hold, denom, fee_on_input);
        if hold_score.is_none_or(|score| score > 10) {
            return None;
        }
    }
    let (score, predict) = stable_scale_at(xp, amp, i, j, fee, rates, which, &ppm, rows, denom, fee_on_input);
    let score = score?;
    if score > 10 { None } else { Some((score, predict)) }
}

const NEAR_ANCHORS: [i64; 9] = [10000, 9800, 10200, 9600, 10400, 9400, 10600, 9000, 11000];
const WIDE_ANCHORS: [i64; 8] = [8000, 12000, 7000, 13000, 6000, 15000, 5000, 20000];

fn scale_candidate(
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
        xp, amp, i, j, fee, rates, target, &NEAR_ANCHORS, 9000, 11000, 18, 5, denom, fee_on_input, false,
    );
    if let Some(accepted) = finish_scale(narrow, xp, amp, i, j, fee, rates, &hold, rows, denom, fee_on_input) {
        return Ok(Some(accepted));
    }
    let wide = scale_window(
        xp, amp, i, j, fee, rates, target, &WIDE_ANCHORS, 5000, 20000, 22, 8, denom, fee_on_input, true,
    );
    Ok(finish_scale(wide, xp, amp, i, j, fee, rates, &hold, rows, denom, fee_on_input))
}

fn curve_book(state: &Value) -> Result<Option<(Vec<BigInt>, BigInt, BigInt, Vec<BigInt>)>, String> {
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
        xp.push(if rate.is_zero() { balance.clone() } else { fd(&(balance * rate), &wad()) });
    }
    Ok(Some((xp, amp, fee, rates)))
}

fn stable_scale_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some((xp, amp, fee, rates)) = curve_book(state)? else {
        return Ok(None);
    };
    scale_candidate(&xp, &amp, i, j, &fee, &rates, direction, rows, &fee_denom(), false)
}

fn rate_index(len: usize, index: i64) -> Option<usize> {
    usize::try_from(index).ok().filter(|index| *index < len)
}

fn stable_flat_quote(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    dx: &BigInt,
) -> Option<BigInt> {
    let rate_i = rates.get(i)?;
    let rate_j = rates.get(j)?;
    let paid = if rate_i.is_zero() { dx.clone() } else { fd(&(dx * rate_i), &wad()) };
    let dy = stable_swap_out(
        xp,
        amp.clone(),
        bi(i as i64),
        bi(j as i64),
        paid,
        fee.clone(),
        fee_denom(),
        false,
        BigInt::zero(),
    )?;
    if rate_j.is_zero() {
        Some(dy)
    } else {
        Some(fd(&(&dy * &wad()), rate_j))
    }
}

fn stable_output_candidate(
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
        let score = worst_bps(predict.as_ref(), target)?;
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
    if !hold.is_empty() && worst_bps(formula.predict().as_ref(), &hold).is_none_or(|score| score > 10) {
        return Ok(None);
    }
    match worst_bps(formula.predict().as_ref(), rows) {
        Some(score) if score <= 10 => Ok(Some((score, formula))),
        _ => Ok(None),
    }
}

fn refine_precs(
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
                let Some(score) = worst_bps(predict.as_ref(), target) else {
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
                rec(choices, balances, rates, amp, i, j, fee, target, prefix, best);
                prefix.pop();
            }
        }
        rec(choices.as_slice(), balances, rates, amp, i, j, fee, target, prefix, best);
    };
    walk(&mut Vec::new(), &mut best);
    best
}

struct RelHit {
    rel: BigRational,
    formula: Formula,
}

fn rel_better(best: &Option<RelHit>, rel: &BigRational) -> bool {
    best.as_ref().map(|item| rel < &item.rel).unwrap_or(true)
}

fn coord_search(
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
                    || left_score.as_ref().is_some_and(|left| right_score.as_ref().is_none_or(|right| left <= right));
                if left_better {
                    right = second;
                    params[idx] = first;
                } else {
                    left = first;
                    params[idx] = second;
                }
                if best.as_ref().is_some_and(|hit| hit.rel == BigRational::zero()) {
                    return best;
                }
            }
        }
    }
    best
}

const CAP_SHARPNESS: [i64; 4] = [8000, 12000, 16000, 24000];
const JOINT_EXPS: [i32; 5] = [-2, -1, 0, 1, 2];

fn he40(value: BigDecimal) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    dec_ctx().round_decimal(value)
}

fn he_at(value: BigDecimal, prec: u64) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    Context::new(NonZeroU64::new(prec).expect("precision"), RoundingMode::HalfEven).round_decimal(value)
}

fn he_div_at(numer: &BigDecimal, denom: &BigDecimal, prec: u64) -> BigDecimal {
    if denom.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    let guard = prec.saturating_add(8).max(16);
    let ctx = Context::new(NonZeroU64::new(guard).expect("precision"), RoundingMode::HalfEven);
    he_at(numer * denom.inverse_with_context(&ctx), prec)
}

fn rat_dec(value: &BigRational) -> BigDecimal {
    let numer = BigDecimal::from(value.numer().clone());
    let denom = BigDecimal::from(value.denom().clone());
    he_div_at(&numer, &denom, 40)
}

fn trunc_dec(value: &BigDecimal) -> BigInt {
    let (digits, scale) = value.as_bigint_and_exponent();
    let exp = if scale <= 0 { -scale } else { scale };
    let Ok(exp) = u32::try_from(exp) else {
        return BigInt::zero();
    };
    // Past this, the integer part is not a token amount.
    if exp > 4_096 {
        return BigInt::zero();
    }
    if scale <= 0 {
        &digits * pow10(exp)
    } else {
        digits / pow10(exp)
    }
}

fn ln2_he(prec: u64) -> BigDecimal {
    let text = "693147180559945309417232121458176568075500134360255254120680009493393621969694715605863326996418687542001481020570685733685520235758130557032670751635";
    let keep = (prec as usize + 8).min(text.len());
    let digits = text[..keep].parse::<BigInt>().expect("ln2");
    he_at(BigDecimal::new(digits, keep as i64), prec)
}

/// Divide rounding half-even. `denom` is positive.
fn round_div(numer: &BigInt, denom: &BigInt) -> BigInt {
    if !denom.is_positive() {
        return BigInt::zero();
    }
    let q = numer / denom;
    let r = numer % denom;
    let twice = if r.is_negative() { -&r * 2 } else { &r * 2 };
    if &twice < denom {
        q
    } else if &twice > denom || !(&q % 2u32).is_zero() {
        if numer.is_negative() { q - 1 } else { q + 1 }
    } else {
        q
    }
}

fn fixed_of(value: &BigDecimal, scale: u32) -> BigInt {
    let (digits, exp) = value.as_bigint_and_exponent();
    let shift = scale as i64 - exp;
    if shift >= 0 {
        digits * pow10(shift as u32)
    } else {
        round_div(&digits, &pow10((-shift) as u32))
    }
}

/// Natural log at precision 40, half-even. Logistic quotes use this path.
/// The series runs in fixed point so a wave search stays on the same integers
/// without building a decimal context on every term.
fn ln_he(value: &BigDecimal) -> BigDecimal {
    if value.sign() != Sign::Plus {
        return BigDecimal::from(0);
    }
    const SCALE: u32 = 96;
    let one = pow10(SCALE);
    let mut reduced = fixed_of(value, SCALE);
    if !reduced.is_positive() {
        return BigDecimal::from(0);
    }
    let two = &one * 2;
    let half = &pow10(SCALE - 1) * 5;
    let ln2 = ln2_scaled();
    let mut extra = BigInt::zero();
    for _ in 0..20_000 {
        if reduced <= two {
            break;
        }
        reduced = round_div(&reduced, &BigInt::from(2));
        extra += ln2;
    }
    for _ in 0..20_000 {
        if reduced >= half {
            break;
        }
        reduced *= 2;
        extra -= ln2;
    }
    let z = round_div(&((&reduced - &one) * &one), &(&reduced + &one));
    let z2 = round_div(&(&z * &z), &one);
    let mut term = z.clone();
    let mut sum = z;
    let mut n = BigInt::from(1);
    for _ in 0..300 {
        term = round_div(&(&term * &z2), &one);
        n += 2;
        let piece = round_div(&term, &n);
        if piece.is_zero() {
            break;
        }
        sum += piece;
    }
    he40(BigDecimal::new(&sum * 2 + extra, SCALE as i64))
}

fn exp_fixed(value: &BigDecimal) -> BigDecimal {
    const SCALE: u32 = 96;
    let one = pow10(SCALE);
    let mut x = fixed_of(value, SCALE);
    let ln2 = ln2_scaled();
    let mut k: i32 = 0;
    for _ in 0..400 {
        if x <= *ln2 {
            break;
        }
        x -= ln2;
        k += 1;
    }
    for _ in 0..400 {
        if x >= -ln2 {
            break;
        }
        x += ln2;
        k -= 1;
    }
    let mut term = one.clone();
    let mut sum = one.clone();
    for n in 1..240 {
        term = round_div(&(&term * &x), &one);
        term = round_div(&term, &BigInt::from(n));
        if term.is_zero() {
            break;
        }
        sum += &term;
    }
    if k > 0 {
        sum <<= k as u32;
    } else if k < 0 {
        sum = round_div(&sum, &(BigInt::one() << ((-k) as usize)));
    }
    he40(BigDecimal::new(sum, SCALE as i64))
}

fn ln2_scaled() -> &'static BigInt {
    static LN2: std::sync::OnceLock<BigInt> = std::sync::OnceLock::new();
    LN2.get_or_init(|| fixed_of(&ln2_he(96), 96))
}

fn ln1exp(value: &BigDecimal) -> BigDecimal {
    let forty = BigDecimal::from(40);
    if value > &forty {
        return value.clone();
    }
    if value < &BigDecimal::from(-40) {
        return BigDecimal::from(0);
    }
    ln_he(&he40(BigDecimal::from(1) + exp_fixed(value)))
}

fn amount_vec(value: Option<&Value>) -> Result<Vec<BigInt>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(amount(Some(item))?.unwrap_or_else(BigInt::zero));
            }
            Ok(out)
        }
        Some(_) => Err("expected a list".to_string()),
    }
}

fn nonzero_amount(value: Option<&Value>, fallback: BigInt) -> Result<BigInt, String> {
    match amount(value)? {
        Some(parsed) if !parsed.is_zero() => Ok(parsed),
        _ => Ok(fallback),
    }
}

fn worst_rel(predict: &Predict, rows: &[(BigInt, BigInt)]) -> Option<BigRational> {
    let mut worst = BigRational::zero();
    let mut previous = bi(-1);
    for (amount, label) in rows {
        if !label.is_positive() {
            return None;
        }
        let pred = predict(amount.clone())?;
        if pred.is_negative() || pred < previous {
            return None;
        }
        previous = pred.clone();
        let gap = (&pred - label).abs();
        if gap <= dust() {
            continue;
        }
        let rel = BigRational::new(gap, label.clone());
        if rel > worst {
            worst = rel;
        }
    }
    Some(worst)
}

fn arrived(rows: &[(BigInt, BigInt)], tenths: i64) -> bool {
    if rows.len() < 4 {
        return false;
    }
    let mut ordered = rows.to_vec();
    ordered.sort();
    let span = &ordered[ordered.len() - 1].0 - &ordered[0].0;
    let end = &ordered[ordered.len() - 1].1;
    if span <= BigInt::zero() || end <= &BigInt::zero() {
        return false;
    }
    let mid = &ordered[0].0 + fd(&span, &bi(2));
    let mut y_mid: Option<&BigInt> = None;
    for (amount, out) in &ordered {
        if amount <= &mid {
            y_mid = Some(out);
        }
    }
    let Some(y_mid) = y_mid else {
        return false;
    };
    y_mid * bi(10) >= end * bi(tenths)
}

fn flattens(rows: &[(BigInt, BigInt)]) -> bool {
    arrived(rows, 7)
}

fn softens(rows: &[(BigInt, BigInt)]) -> bool {
    arrived(rows, 6)
}

fn gated(formula: &Formula, hold: &[(BigInt, BigInt)], rows: &[(BigInt, BigInt)]) -> Option<Scored> {
    let predict = formula.predict();
    if !hold.is_empty() && worst_bps(predict.as_ref(), hold).is_none_or(|score| score > 10) {
        return None;
    }
    let score = worst_bps(predict.as_ref(), rows)?;
    if score > 10 {
        None
    } else {
        Some((score, formula.clone()))
    }
}

fn accept_cap(formula: Formula, hold: &[(BigInt, BigInt)], rows: &[(BigInt, BigInt)]) -> Option<Scored> {
    let (score, formula) = gated(&formula, hold, rows)?;
    let predict = formula.predict();
    let mut previous = bi(-1);
    for (amount, _) in rows {
        let got = predict(amount.clone())?;
        if got < previous {
            return None;
        }
        previous = got;
    }
    Some((score, formula))
}

fn opening(target: &[(BigInt, BigInt)]) -> Option<(BigRational, BigRational, BigRational, BigInt)> {
    let mut slope0: Option<BigRational> = None;
    let mut cap_int: Option<BigInt> = None;
    for (amount, out) in target {
        if !amount.is_positive() {
            return None;
        }
        let slope = BigRational::new(out.clone(), amount.clone());
        if slope0.as_ref().map(|so_far| &slope > so_far).unwrap_or(true) {
            slope0 = Some(slope);
        }
        if cap_int.as_ref().map(|so_far| out > so_far).unwrap_or(true) {
            cap_int = Some(out.clone());
        }
    }
    let slope0 = slope0?;
    let cap_int = cap_int?;
    if slope0 <= BigRational::zero() || cap_int <= BigInt::zero() {
        return None;
    }
    let cap0 = BigRational::from_integer(cap_int.clone());
    let knee0 = &cap0 / &slope0;
    Some((slope0, cap0, knee0, cap_int))
}

fn fillet_quote(slope: BigRational, cap: BigRational, xa: BigRational, xb: BigRational) -> Predict {
    Arc::new(move |x: BigInt| {
        let fx = BigRational::from_integer(x);
        let y = if xa <= BigRational::zero() || fx <= xa || xb <= xa {
            let mut y = &slope * &fx;
            if xb <= xa && y > cap {
                y = cap.clone();
            }
            y
        } else if fx >= xb {
            cap.clone()
        } else {
            let u = (&fx - &xa) / (&xb - &xa);
            let one = BigRational::one();
            let two = BigRational::from_integer(bi(2));
            let three = BigRational::from_integer(bi(3));
            let one_u = &one - &u;
            let h00 = (&one + &two * &u) * &one_u * &one_u;
            let h10 = &u * &one_u * &one_u;
            let h01 = &u * &u * (&three - &two * &u);
            h00 * &slope * &xa + h10 * (&xb - &xa) * &slope + h01 * &cap
        };
        if y <= BigRational::zero() {
            Some(BigInt::zero())
        } else {
            Some(trunc_rat(&y))
        }
    })
}

fn logistic_quote(slope: &BigRational, rate: &BigRational, knee: &BigRational) -> Predict {
    let s = rat_dec(slope);
    let k = rat_dec(rate);
    let x0 = rat_dec(knee);
    if k.sign() != Sign::Plus || s.sign() != Sign::Plus || x0.sign() != Sign::Plus {
        return Arc::new(|_| Some(BigInt::zero()));
    }
    // The opening height and the slope/rate ratio do not depend on the input.
    let numer = ln1exp(&he40(&k * &x0));
    let slope_over_rate = he_div_at(&s, &k, 40);
    Arc::new(move |x: BigInt| {
        let x = BigDecimal::from(x);
        let denom = ln1exp(&he40(&(-&k) * he40(&x - &x0)));
        let y = he40(&slope_over_rate * he40(&numer - &denom));
        if y.sign() != Sign::Plus {
            Some(BigInt::zero())
        } else {
            Some(trunc_dec(&y))
        }
    })
}

fn logistic_at(slope0: &BigRational, knee0: &BigRational, s_ppm: i64, sharp: i64, x_ppm: i64) -> Option<Formula> {
    if s_ppm <= 0 || sharp <= 0 || x_ppm <= 0 || knee0 <= &BigRational::zero() {
        return None;
    }
    let ten_k = rat_i(10_000);
    let slope = slope0 * BigRational::from_integer(bi(s_ppm)) / &ten_k;
    let rate = BigRational::new(bi(sharp), bi(10_000)) * rat_i(10) / knee0;
    let knee = knee0 * BigRational::from_integer(bi(x_ppm)) / &ten_k;
    Some(Formula::Logistic { slope, rate, knee })
}

fn scaled_formula(base: Formula, ppm: i64) -> Formula {
    Formula::Scaled { base: Box::new(base), ppm }
}

fn wave_formula(base: Formula, cap: &BigInt, x_last: &BigInt, a_ppm: i64, b_ppm: i64) -> Formula {
    Formula::Wave {
        base: Box::new(base),
        cap: cap.clone(),
        x_last: x_last.clone(),
        a_ppm,
        b_ppm,
    }
}

fn scale_out(base: Predict, ppm: i64) -> Predict {
    let ppm = bi(ppm);
    Arc::new(move |amount: BigInt| {
        let value = base(amount).unwrap_or_else(BigInt::zero);
        Some(fd(&(&value * &ppm), &bi(10_000)))
    })
}

fn i64s(ppm: &[BigInt]) -> Option<Vec<i64>> {
    ppm.iter().map(|item| item.to_i64()).collect()
}

fn keep_rel(best: &mut Option<(BigRational, Formula)>, rel: BigRational, formula: Formula) {
    if best.as_ref().map(|(so_far, _)| &rel < so_far).unwrap_or(true) {
        *best = Some((rel, formula));
    }
}

fn thirds_i(left: i64, right: i64) -> (i64, i64) {
    let third = (right - left).div_euclid(3);
    (left + third, right - third)
}

fn fillet_cap_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    if !flattens(target) {
        return Ok(None);
    }
    let Some((slope0, cap0, _, _)) = opening(target) else {
        return Ok(None);
    };
    let starts: [[i64; 4]; 8] = [
        [10000, 10000, 10000, 10000],
        [10000, 10000, 8000, 12000],
        [10000, 10000, 6000, 14000],
        [10000, 10000, 4000, 18000],
        [9500, 10000, 7500, 13000],
        [10500, 10000, 8500, 11500],
        [10000, 10100, 7000, 15000],
        [9800, 10000, 9000, 11000],
    ];
    let bounds = [(5000, 16000), (9000, 14000), (500, 10000), (10000, 30000)];
    let mut best: Option<(BigRational, Formula)> = None;
    let score_of = |ppm: [i64; 4]| -> Option<(BigRational, Formula)> {
        let [s_ppm, c_ppm, a_ppm, b_ppm] = ppm;
        if s_ppm.min(c_ppm).min(a_ppm) <= 0 || a_ppm > b_ppm {
            return None;
        }
        let ten_k = rat_i(10_000);
        let slope = &slope0 * BigRational::from_integer(bi(s_ppm)) / &ten_k;
        let cap = &cap0 * BigRational::from_integer(bi(c_ppm)) / &ten_k;
        if slope <= BigRational::zero() {
            return None;
        }
        let hit = &cap / &slope;
        let formula = Formula::Fillet {
            slope,
            cap: cap.clone(),
            xa: &hit * BigRational::from_integer(bi(a_ppm)) / &ten_k,
            xb: &hit * BigRational::from_integer(bi(b_ppm)) / &ten_k,
        };
        let rel = worst_rel(&formula.predict(), target)?;
        Some((rel, formula))
    };
    for start in starts {
        let mut params = start;
        for _round in 0..3 {
            for (idx, (lo0, hi0)) in bounds.iter().enumerate() {
                let mut left = *lo0;
                let mut right = *hi0;
                for _step in 0..12 {
                    if right - left < 4 {
                        break;
                    }
                    let (first, second) = thirds_i(left, right);
                    let mut left_ppm = params;
                    let mut right_ppm = params;
                    left_ppm[idx] = first;
                    right_ppm[idx] = second;
                    let left_got = score_of(left_ppm);
                    let right_got = score_of(right_ppm);
                    let left_score = left_got.as_ref().map(|item| item.0.clone());
                    let right_score = right_got.as_ref().map(|item| item.0.clone());
                    if let Some((rel, predict)) = left_got {
                        keep_rel(&mut best, rel, predict);
                    }
                    if let Some((rel, predict)) = right_got {
                        keep_rel(&mut best, rel, predict);
                    }
                    if left_score.is_none() && right_score.is_none() {
                        break;
                    }
                    let left_better = right_score.is_none()
                        || left_score.as_ref().is_some_and(|left| right_score.as_ref().is_none_or(|right| left <= right));
                    if left_better {
                        right = second;
                        params[idx] = first;
                    } else {
                        left = first;
                        params[idx] = second;
                    }
                }
            }
        }
    }
    Ok(best.and_then(|(_, formula)| accept_cap(formula, &hold, rows)))
}

fn logistic_cap_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    if !flattens(target) {
        return Ok(None);
    }
    let Some((slope0, _, knee0, _)) = opening(target) else {
        return Ok(None);
    };
    let mut best: Option<(BigRational, Formula)> = None;
    for sharp in CAP_SHARPNESS {
        let Some(base) = logistic_at(&slope0, &knee0, 10000, sharp, 10000) else {
            continue;
        };
        let mut ppm = 9700;
        while ppm <= 10300 {
            let formula = scaled_formula(base.clone(), ppm);
            if let Some(rel) = worst_rel(&formula.predict(), target) {
                keep_rel(&mut best, rel, formula);
            }
            ppm += 10;
        }
    }
    Ok(best.and_then(|(_, formula)| accept_cap(formula, &hold, rows)))
}

fn logistic_refine_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    if !flattens(target) {
        return Ok(None);
    }
    let Some((slope0, _, knee0, _)) = opening(target) else {
        return Ok(None);
    };
    let mut best: Option<(BigRational, Formula)> = None;
    let consider = |best: &mut Option<(BigRational, Formula)>, s_ppm: i64, sharp: i64, x_ppm: i64| -> Option<BigRational> {
        let formula = logistic_at(&slope0, &knee0, s_ppm, sharp, x_ppm)?;
        let rel = worst_rel(&formula.predict(), target)?;
        let saved = rel.clone();
        keep_rel(best, rel, formula);
        Some(saved)
    };
    for sharp in [2000, 5000, 10000, 20000, 40000, 80000] {
        consider(&mut best, 10000, sharp, 10000);
        let mut params = [10000, sharp, 10000];
        let windows = [(3000, 30000), (500, 200000), (2000, 40000)];
        for _round in 0..2 {
            for (idx, (lo0, hi0)) in windows.iter().enumerate() {
                let mut left = *lo0;
                let mut right = *hi0;
                for _step in 0..10 {
                    if right - left < 4 {
                        break;
                    }
                    let (first, second) = thirds_i(left, right);
                    let mut left_try = params;
                    let mut right_try = params;
                    left_try[idx] = first;
                    right_try[idx] = second;
                    let left_score = consider(&mut best, left_try[0], left_try[1], left_try[2]);
                    let right_score = consider(&mut best, right_try[0], right_try[1], right_try[2]);
                    if left_score.is_none() && right_score.is_none() {
                        break;
                    }
                    let left_better = right_score.is_none()
                        || left_score.as_ref().is_some_and(|left| right_score.as_ref().is_none_or(|right| left <= right));
                    if left_better {
                        right = second;
                        params[idx] = first;
                    } else {
                        left = first;
                        params[idx] = second;
                    }
                    if best.as_ref().is_some_and(|(rel, _)| rel.is_zero()) {
                        break;
                    }
                }
            }
        }
    }
    Ok(best.and_then(|(_, formula)| accept_cap(formula, &hold, rows)))
}

fn wave_quote(base: Predict, cap: &BigInt, x_last: &BigInt, a_ppm: i64, b_ppm: i64) -> Predict {
    let cap = cap.clone();
    let x_last = x_last.clone();
    let a_ppm = bi(a_ppm);
    let b_ppm = bi(b_ppm);
    Arc::new(move |amount: BigInt| {
        let value = base(amount.clone()).unwrap_or_else(BigInt::zero);
        if amount <= BigInt::zero() || amount >= x_last || (a_ppm.is_zero() && b_ppm.is_zero()) {
            return Some(value);
        }
        let span = &amount * (&x_last - &amount);
        let mut corr = fd(&(&cap * &a_ppm * &span), &(&x_last * &x_last * bi(10_000)));
        corr += fd(
            &(&cap * &b_ppm * &span * (bi(2) * &amount - &x_last)),
            &(&x_last * &x_last * &x_last * bi(10_000)),
        );
        Some(value + corr)
    })
}

#[derive(Clone, Copy)]
struct WaveWindow {
    starts: &'static [[i64; 3]],
    bounds: [(i64, i64); 3],
    steps: usize,
    stop_on_zero: bool,
    wave_steps: usize,
    wave_rounds: usize,
    stop_wave: bool,
}

/// One sharpness. The four sharpness values do not read each other's best
/// score, so they run together. A later value replaces the best only when its
/// relative error is strictly smaller, which is the sequential order.
fn search_sharp(
    sharp: i64,
    slope0: &BigRational,
    knee0: &BigRational,
    cap: &BigInt,
    x_last: &BigInt,
    target: &[(BigInt, BigInt)],
    window: WaveWindow,
) -> Option<RelHit> {
    let mut logistic: Option<RelHit> = None;
    for start in window.starts {
        let start_bi = [bi(start[0]), bi(start[1]), bi(start[2])];
        let bounds = [
            (bi(window.bounds[0].0), bi(window.bounds[0].1)),
            (bi(window.bounds[1].0), bi(window.bounds[1].1)),
            (bi(window.bounds[2].0), bi(window.bounds[2].1)),
        ];
        let got = coord_search(
            |ppm| {
                let values = i64s(ppm)?;
                if values.len() != 3 {
                    return None;
                }
                let base = logistic_at(slope0, knee0, values[0], sharp, values[1])?;
                if values[2] <= 0 {
                    return None;
                }
                let formula = scaled_formula(base, values[2]);
                let rel = worst_rel(&formula.predict(), target)?;
                Some(RelHit { rel, formula })
            },
            &start_bi,
            &bounds,
            window.steps,
            1,
        );
        if let Some(hit) = got {
            if rel_better(&logistic, &hit.rel) {
                logistic = Some(hit);
            }
        }
        if window.stop_on_zero && logistic.as_ref().is_some_and(|hit| hit.rel.is_zero()) {
            break;
        }
    }
    let base = logistic?.formula;
    let mut best: Option<RelHit> = None;
    for start in [[0, 0], [-40, -50]] {
        let start_bi = [bi(start[0]), bi(start[1])];
        let bounds = [(bi(-200), bi(200)), (bi(-200), bi(200))];
        let got = coord_search(
            |ppm| {
                let values = i64s(ppm)?;
                if values.len() != 2 {
                    return None;
                }
                let formula = wave_formula(base.clone(), cap, x_last, values[0], values[1]);
                let rel = worst_rel(&formula.predict(), target)?;
                Some(RelHit { rel, formula })
            },
            &start_bi,
            &bounds,
            window.wave_steps,
            window.wave_rounds,
        );
        if let Some(hit) = got {
            if rel_better(&best, &hit.rel) {
                best = Some(hit);
            }
        }
        if window.stop_wave && best.as_ref().is_some_and(|hit| hit.rel.is_zero()) {
            break;
        }
    }
    best
}

fn logistic_wave_search(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
    window: WaveWindow,
) -> Result<Option<Formula>, String> {
    let (train, _) = split_holdout(direction)?;
    let target = train_rows(&train, rows).to_vec();
    if !softens(&target) {
        return Ok(None);
    }
    let Some((slope0, _, knee0, cap)) = opening(&target) else {
        return Ok(None);
    };
    let mut ordered = target.clone();
    ordered.sort();
    let x_last = ordered.last().map(|row| row.0.clone()).unwrap_or_else(BigInt::zero);
    if x_last <= BigInt::zero() || cap <= BigInt::zero() {
        return Ok(None);
    }
    let hits = std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for sharp in CAP_SHARPNESS {
            let slope0 = slope0.clone();
            let knee0 = knee0.clone();
            let cap = cap.clone();
            let x_last = x_last.clone();
            let target = target.clone();
            joins.push(scope.spawn(move || {
                search_sharp(sharp, &slope0, &knee0, &cap, &x_last, &target, window)
            }));
        }
        joins
            .into_iter()
            .map(|join| join.join().expect("sharp search"))
            .collect::<Vec<_>>()
    });
    let mut best: Option<RelHit> = None;
    for hit in hits.into_iter().flatten() {
        if rel_better(&best, &hit.rel) {
            best = Some(hit);
        }
    }
    Ok(best.map(|hit| hit.formula))
}

fn near_wave(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Formula>, String> {
    logistic_wave_search(
        direction,
        rows,
        WaveWindow {
            starts: &[[10000, 10000, 10000], [9900, 10000, 9990]],
            bounds: [(9000, 11000), (8000, 14000), (9800, 10200)],
            steps: 10,
            stop_on_zero: false,
            wave_steps: 14,
            wave_rounds: 2,
            stop_wave: false,
        },
    )
}

fn logistic_wave_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let Some(predict) = near_wave(direction, rows)? else {
        return Ok(None);
    };
    let (_, hold) = split_holdout(direction)?;
    Ok(accept_cap(predict, &hold, rows))
}

fn logistic_wide_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let Some(predict) = logistic_wave_search(
        direction,
        rows,
        WaveWindow {
            starts: &[[10000, 10000, 10000], [10000, 4000, 10000], [10000, 20000, 10000]],
            bounds: [(7000, 14000), (2000, 100000), (9700, 10300)],
            steps: 12,
            stop_on_zero: true,
            wave_steps: 12,
            wave_rounds: 1,
            stop_wave: true,
        },
    )?
    else {
        return Ok(None);
    };
    let (_, hold) = split_holdout(direction)?;
    Ok(accept_cap(predict, &hold, rows))
}

fn stable_ppms_predict(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    ppms: &[BigInt],
) -> Formula {
    let scaled: Vec<BigInt> = xp.iter().zip(ppms).map(|(value, ppm)| fd(&(value * ppm), &bi(10_000))).collect();
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

fn joint_amps(amp0: &BigInt) -> Vec<BigInt> {
    let seeds = [amp0.clone(), fd(amp0, &bi(10)), amp0 * bi(10), bi(100), bi(1000), bi(10_000), bi(100_000)];
    let mut amps = Vec::new();
    for amp in seeds {
        if amp.is_positive() && !amps.contains(&amp) {
            amps.push(amp);
        }
    }
    amps
}

fn joint_ppm(exp: i32) -> BigInt {
    pow10((4 + exp) as u32)
}

fn score_ppms(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    ppms: &[BigInt],
    target: &[(BigInt, BigInt)],
) -> Option<i64> {
    worst_bps(stable_ppms_predict(xp, amp, i, j, fee, rates, ppms).predict().as_ref(), target)
}

struct JointFound {
    score: i64,
    amp: BigInt,
    ppms: Vec<BigInt>,
}

fn refine_joint(
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
                    || left_score.is_some_and(|left_score| right_score.is_none_or(|right_score| left_score <= right_score));
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
                best = JointFound { score: local_score, amp: amp.clone(), ppms: chosen.clone() };
            }
            if stop_at_bar && best.score <= 10 {
                return best;
            }
        }
    }
    best
}

fn remember(best: &mut Option<JointFound>, score: i64, amp: &BigInt, ppms: Vec<BigInt>) {
    if best.as_ref().map(|item| score < item.score).unwrap_or(true) {
        *best = Some(JointFound { score, amp: amp.clone(), ppms });
    }
}

fn joint_search(
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
                        let Some(score) = score_ppms(xp, &amp, i, j, fee, rates, &trial, target) else {
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

fn joint_best(
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

fn joint_state(state: &Value) -> Result<Option<(Vec<BigInt>, BigInt, BigInt, Vec<BigInt>)>, String> {
    let Some(coins) = state.get("coins").and_then(Value::as_array) else {
        return Ok(None);
    };
    if coins.len() < 3 {
        return Ok(None);
    }
    curve_book(state)
}

fn finish_joint(
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
    gated(&stable_ppms_predict(xp, &found.amp, i, j, fee, rates, &found.ppms), hold, rows)
}

fn stable_joint_candidate(
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
    Ok(finish_joint(joint_search(&xp, &amp, i, j, &fee, &rates, target), &xp, i, j, &fee, &rates, &hold, rows))
}

fn stable_joint_best_candidate(
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
    Ok(finish_joint(joint_best(&xp, &amp, i, j, &fee, &rates, target), &xp, i, j, &fee, &rates, &hold, rows))
}

fn stable_refine_candidate(
    state: &Value,
    i: usize,
    j: usize,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let coins = state.get("coins").and_then(Value::as_array).ok_or_else(|| "coins".to_string())?;
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
    let Some((_stored, precs, base_xp)) = refine_precs(&balances, &rates, &amp0, i, j, &fee0, &target) else {
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
            let (amp, ppm_i, ppm_j, out_ppm, fee) =
                (ppm[0].clone(), ppm[1].clone(), ppm[2].clone(), ppm[3].clone(), ppm[4].clone());
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
            Some(RelHit { rel: worst_rel(&formula.predict(), &target)?, formula })
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

fn base_residual_candidate(
    base: &Formula,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    if train.len() < 2 {
        return Ok(None);
    }
    let formula = Formula::Added {
        base: Box::new(base.clone()),
        knots: train,
        alpha: BigRational::one(),
    };
    Ok(gated(&formula, &hold, rows))
}

fn flat_residual_candidate(
    base: &Formula,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    if train.len() < 2 {
        return Ok(None);
    }
    let formula = Formula::Added {
        base: Box::new(base.clone()),
        knots: train,
        alpha: BigRational::zero(),
    };
    Ok(gated(&formula, &hold, rows))
}

fn scaled_quote(knots: &[(BigInt, BigInt)], ppm: &BigInt) -> Option<Formula> {
    residual_quote(knots)?;
    Some(Formula::ScaledResidual { knots: knots.to_vec(), ppm: ppm.clone() })
}

fn scaled_residual_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    if train.len() < 3 {
        return Ok(None);
    }
    let mut best: Option<(i64, BigInt)> = None;
    let take = |best: &mut Option<(i64, BigInt)>, ppm: &BigInt| -> Option<i64> {
        let mut worst = 0i64;
        for index in 0..train.len() {
            let knots: Vec<(BigInt, BigInt)> =
                train.iter().enumerate().filter(|(n, _)| *n != index).map(|(_, row)| row.clone()).collect();
            let predict = scaled_quote(&knots, ppm)?;
            let score = worst_bps(predict.predict().as_ref(), &train[index..index + 1])?;
            worst = worst.max(score);
        }
        let tie_closer = best.as_ref().is_some_and(|(score, chosen)| {
            *score == worst && (ppm - bi(10_000)).abs() < (chosen - bi(10_000)).abs()
        });
        if best.as_ref().map(|(score, _)| worst < *score).unwrap_or(true) || tie_closer {
            *best = Some((worst, ppm.clone()));
        }
        Some(worst)
    };
    for ppm in [10_000, 9990, 10010, 9980, 10020, 9970, 10030, 9950, 10050, 9940, 10060, 9920, 10080, 9900, 10100] {
        take(&mut best, &bi(ppm));
        if best.as_ref().is_some_and(|(score, _)| *score == 0) {
            break;
        }
    }
    if best.is_none() {
        return Ok(None);
    }
    let mut left = bi(9900);
    let mut right = bi(10100);
    for _step in 0..12 {
        if best.as_ref().is_some_and(|(score, _)| *score <= 10) || &right - &left < bi(4) {
            break;
        }
        let (first, second) = ternary_points(&left, &right);
        let left_score = take(&mut best, &first);
        let right_score = take(&mut best, &second);
        if left_score.is_none() && right_score.is_none() {
            break;
        }
        if right_score.is_none() || left_score.is_some_and(|left_score| right_score.is_none_or(|right_score| left_score <= right_score))
        {
            right = second;
        } else {
            left = first;
        }
    }
    let Some((_, ppm)) = best else {
        return Ok(None);
    };
    let Some(predict) = scaled_quote(&train, &ppm) else {
        return Ok(None);
    };
    Ok(gated(&predict, &hold, rows))
}

fn loo_worst(base: &Formula, train: &[(BigInt, BigInt)], alpha: &BigRational) -> Option<i64> {
    let mut worst = 0i64;
    for index in 0..train.len() {
        let knots: Vec<(BigInt, BigInt)> =
            train.iter().enumerate().filter(|(n, _)| *n != index).map(|(_, row)| row.clone()).collect();
        let predict = added_residual(base.predict(), &knots, alpha)?;
        let score = worst_bps(predict.as_ref(), &train[index..index + 1])?;
        worst = worst.max(score);
    }
    Some(worst)
}

fn alphas() -> Vec<BigRational> {
    vec![
        BigRational::zero(),
        BigRational::new(bi(1), bi(8)),
        BigRational::new(bi(1), bi(4)),
        BigRational::new(bi(1), bi(2)),
        BigRational::new(bi(3), bi(4)),
        BigRational::one(),
        BigRational::new(bi(5), bi(4)),
        BigRational::new(bi(3), bi(2)),
        rat_i(2),
        rat_i(4),
    ]
}

fn loo_alpha_candidate(base: &Formula, direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    if train.len() < 3 {
        return Ok(None);
    }
    let mut best: Option<(i64, i64, BigRational)> = None;
    for alpha in alphas() {
        let Some(score) = loo_worst(base, &train, &alpha) else {
            continue;
        };
        let prefer = if alpha == BigRational::one() { 0 } else { 1 };
        if best.as_ref().map(|(so_far, pref, _)| (score, prefer) < (*so_far, *pref)).unwrap_or(true) {
            best = Some((score, prefer, alpha));
        }
    }
    let Some((_, _, alpha)) = best else {
        return Ok(None);
    };
    let formula = Formula::Added {
        base: Box::new(base.clone()),
        knots: train,
        alpha,
    };
    Ok(gated(&formula, &hold, rows))
}

fn wave_loo_from(
    base: Option<Formula>,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some(base) = base else {
        return Ok(None);
    };
    let Some((score, predict)) = loo_alpha_candidate(&base, direction, rows)? else {
        return Ok(None);
    };
    let mut ordered = rows.to_vec();
    ordered.sort();
    let mut previous = bi(-1);
    for (amount, _) in &ordered {
        let Some(value) = predict.predict()(amount.clone()) else {
            return Ok(None);
        };
        if value < previous {
            return Ok(None);
        }
        previous = value;
    }
    Ok(Some((score, predict)))
}

fn wave_loo_candidate(direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    wave_loo_from(near_wave(direction, rows)?, direction, rows)
}

fn through_sample_candidates(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Vec<(&'static str, Scored)>, String> {
    let (train, hold) = split_holdout(direction)?;
    let made = [
        ("chord", Formula::Chord { knots: train.clone() }),
        ("cubic", Formula::Cubic { knots: train.clone() }),
        ("residual", Formula::HyperResidual { knots: train.clone() }),
    ];
    let mut found = Vec::new();
    for (name, formula) in made {
        let Some(scored) = gated(&formula, &hold, rows) else {
            continue;
        };
        found.push((name, scored));
    }
    Ok(found)
}

fn ladder_candidate(state: &Value, direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
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
        let (Some(tick_at), Some(net)) = (whole(word.get("tick"))?, amount(word.get("liquidity_net"))?) else {
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

fn product_candidate(state: &Value, direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
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
    let (reserve_in, reserve_out) = if tin.eq_ignore_ascii_case(token0) && tout.eq_ignore_ascii_case(token1) {
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

fn weighted_candidate(state: &Value, i: usize, j: usize, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
    let balances = amount_vec(state.get("balances"))?;
    let weights = amount_vec(state.get("weights"))?;
    let fee = amount_or_zero(state.get("fee"))?;
    if weights.len() != balances.len() || fee > wad() || i >= balances.len() || j >= balances.len() {
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

fn balancer_stable_candidate(
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
        let xp: Vec<BigInt> = balances.iter().zip(precs).map(|(balance, prec)| balance * prec).collect();
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

fn balancer_scale_candidate(
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
    if amp_word <= BigInt::zero() || fee > wad() || balances.iter().any(|balance| balance <= &BigInt::zero()) {
        return Ok(None);
    }
    let amp = fd(&amp_word, &precision);
    let rates = vec![BigInt::zero(); balances.len()];
    scale_candidate(&balances, &amp, i, j, &fee, &rates, direction, rows, &wad(), true)
}

fn bin_candidate(state: &Value, direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Scored>, String> {
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
    let (selling0, scale_key) = if tin.eq_ignore_ascii_case(token0) && tout.eq_ignore_ascii_case(token1) {
        (true, "scale1")
    } else if tin.eq_ignore_ascii_case(token1) && tout.eq_ignore_ascii_case(token0) {
        (false, "scale0")
    } else {
        return Ok(None);
    };
    let scale_out = amount(state.get(scale_key))?.unwrap_or_else(BigInt::zero);
    let bin_index = whole(state.get("bin_index"))?;
    let mut scaled_out = BigInt::zero();
    let mut length = BigInt::zero();
    if let (Some(bins), Some(bin_index)) = (state.get("bins").and_then(Value::as_array), bin_index) {
        for word in bins {
            let Some(word) = word.as_object() else {
                continue;
            };
            if whole(word.get("index"))?.as_ref() != Some(&bin_index) {
                continue;
            }
            length = whole(word.get("length_e6"))?.unwrap_or_else(BigInt::zero);
            let key = if selling0 { "token1_scaled" } else { "token0_scaled" };
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

pub(crate) fn choose(state: &Value, direction: &Value, rows: &[(BigInt, BigInt)]) -> Result<Option<Chosen>, String> {
    let shape = state.get("shape").and_then(Value::as_str);
    let mut found: Vec<Found> = Vec::new();
    let mut fit_three = None;
    let mut fit_stable = None;
    let mut fit_balancer = None;
    let mut ladder_got = None;
    let mut stable_got = None;
    let mut flat_got = None;
    let mut bal_got = None;
    match shape {
        Some("constant_product") => consider(&mut found, "product", &product_candidate(state, direction, rows)?),
        Some("concentrated") => {
            ladder_got = ladder_candidate(state, direction, rows)?;
            consider(&mut found, "ladder", &ladder_got);
        }
        Some("curve") => {
            if let Some(coins) = state.get("coins").and_then(Value::as_array) {
                if let Some(tokens) = string_tokens(coins) {
                    let i = index_of(&tokens, direction.get("token_in"));
                    let j = index_of(&tokens, direction.get("token_out"));
                    let gamma = amount_or_zero(state.get("gamma"))?;
                    let crypto = truthy(state.get("crypto")) || !gamma.is_zero();
                    if let (Some(i), Some(j)) = (i, j) {
                        if i != j && !crypto {
                            stable_got = stable_candidate(state, i, j, rows)?;
                            consider(&mut found, "stable", &stable_got);
                            if stable_got.as_ref().is_none_or(|item| item.0 > 10) {
                                fit_stable = Some((i, j));
                            }
                        } else if i != j && crypto && coins.len() == 2 {
                            let gamma_fit = crypto2_candidate(state, i, j, rows)?;
                            consider(&mut found, "two-coin", &gamma_fit);
                            if gamma_fit.as_ref().is_none_or(|item| item.0 > 10) {
                                flat_got = flat_pair_candidate(state, i, j, rows)?;
                                consider(&mut found, "flat-pair", &flat_got);
                            }
                        } else if i != j && crypto && coins.len() == 3 {
                            let stored = crypto3_candidate(state, i, j, rows)?;
                            consider(&mut found, "three-coin", &stored);
                            if stored.as_ref().is_none_or(|item| item.0 > 10) {
                                fit_three = Some((i, j));
                            }
                        }
                    }
                }
            }
        }
        Some("balancer") => {
            if let Some(tokens) = state.get("tokens").and_then(Value::as_array) {
                if tokens.iter().all(Value::is_string) {
                    let typed: Vec<String> =
                        tokens.iter().filter_map(|token| token.as_str().map(str::to_string)).collect();
                    let i = index_of(&typed, direction.get("token_in"));
                    let j = index_of(&typed, direction.get("token_out"));
                    if let (Some(i), Some(j)) = (i, j) {
                        if i != j && truthy(state.get("stable")) {
                            bal_got = balancer_stable_candidate(state, i, j, rows)?;
                            consider(&mut found, "balancer-stable", &bal_got);
                            if bal_got.as_ref().is_none_or(|item| item.0 > 10) {
                                fit_balancer = Some((i, j));
                            }
                        } else if i != j {
                            consider(&mut found, "weighted", &weighted_candidate(state, i, j, rows)?);
                        }
                    }
                }
            }
        }
        Some("metric") => consider(&mut found, "bins", &bin_candidate(state, direction, rows)?),
        _ => {}
    }
    consider(&mut found, "hyperbola", &hyperbola_candidate(rows));
    for (name, scored) in through_sample_candidates(direction, rows)? {
        consider(&mut found, name, &Some(scored));
    }
    if !found.iter().any(|item| item.score <= 10) {
        for got in [&ladder_got, &stable_got, &flat_got, &bal_got] {
            let Some((_, formula)) = got else {
                continue;
            };
            consider(&mut found, "residual", &base_residual_candidate(formula, direction, rows)?);
            consider(&mut found, "residual", &flat_residual_candidate(formula, direction, rows)?);
        }
        consider(&mut found, "residual", &scaled_residual_candidate(direction, rows)?);
        if let Some((_, formula)) = hyperbola_candidate(rows) {
            consider(&mut found, "residual", &loo_alpha_candidate(&formula, direction, rows)?);
        }
        if let Some((i, j)) = fit_three {
            consider(&mut found, "three-coin", &crypto3_fit_candidate(state, i, j, direction, rows)?);
            consider(&mut found, "three-coin", &crypto3_consistent_candidate(state, i, j, direction, rows)?);
        }
        if let Some((i, j)) = fit_stable {
            consider(&mut found, "stable", &stable_mixed_candidate(state, i, j, direction, rows)?);
            consider(&mut found, "stable", &stable_scale_candidate(state, i, j, direction, rows)?);
            consider(&mut found, "stable", &stable_output_candidate(state, i, j, direction, rows)?);
            if stable_got.as_ref().is_some_and(|item| item.0 <= 400) {
                consider(&mut found, "stable", &stable_refine_candidate(state, i, j, direction, rows)?);
                if state.get("coins").and_then(Value::as_array).is_some_and(|coins| coins.len() >= 3) {
                    consider(&mut found, "stable", &stable_joint_candidate(state, i, j, direction, rows)?);
                    consider(&mut found, "stable", &stable_joint_best_candidate(state, i, j, direction, rows)?);
                }
            }
        }
        if let Some((i, j)) = fit_balancer {
            consider(&mut found, "balancer-stable", &balancer_scale_candidate(state, i, j, direction, rows)?);
        }
        consider(&mut found, "cap", &fillet_cap_candidate(direction, rows)?);
        consider(&mut found, "cap", &logistic_cap_candidate(direction, rows)?);
        consider(&mut found, "cap", &logistic_refine_candidate(direction, rows)?);
        let near = near_wave(direction, rows)?;
        let (_, hold) = split_holdout(direction)?;
        let near_scored = near
            .as_ref()
            .and_then(|formula| accept_cap(formula.clone(), &hold, rows));
        consider(&mut found, "cap", &near_scored);
        consider(&mut found, "cap", &wave_loo_from(near, direction, rows)?);
        consider(&mut found, "cap", &logistic_wide_candidate(direction, rows)?);
    }
    Ok(found.into_iter().min_by(|left, right| {
        (left.score, left.rank, left.name).cmp(&(right.score, right.rank, right.name))
    }).map(|item| Chosen { score: item.score, name: item.name, formula: item.formula }))
}

/// Run the selector on one direction. The score is the all-label score.
pub fn select_fit(state: &Value, direction: &Value) -> Result<Option<(i64, &'static str)>, String> {
    let rows = labels_of(direction)?;
    Ok(choose(state, direction, &rows)?.map(|item| (item.score, item.name)))
}

/// One logistic quote. `amounts` share the closed form built for the search.
pub fn logistic_outs(
    slope: &BigRational,
    rate: &BigRational,
    knee: &BigRational,
    amounts: &[BigInt],
) -> Vec<Option<BigInt>> {
    let predict = logistic_quote(slope, rate, knee);
    amounts.iter().map(|amount| predict(amount.clone())).collect()
}

/// Decode a formula or a fit payload and quote `amount`. Panics are bugs.
pub fn poke_formula(bytes: &[u8], amount: &BigInt) {
    if let Ok(formula) = Formula::decode(bytes) {
        let _ = eval_formula(&formula, amount);
        let mut encoded = Vec::new();
        formula.encode(&mut encoded);
        if let Ok(again) = Formula::decode(&encoded) {
            assert_eq_formula(&formula, &again);
            let _ = eval_formula(&again, amount);
        } else {
            panic!("encoded formula did not decode");
        }
    }
    if let Ok(fit) = crate::fit_formula::decode_fit(bytes) {
        for direction in &fit.directions {
            let _ = eval_formula(&direction.formula, amount);
        }
        let encoded = crate::fit_formula::encode_fit(fit.reason, &fit.directions);
        let again = crate::fit_formula::decode_fit(&encoded).expect("encoded fit");
        if again.directions.len() != fit.directions.len() {
            panic!("fit direction count changed");
        }
        for (left, right) in fit.directions.iter().zip(&again.directions) {
            assert_eq_formula(&left.formula, &right.formula);
        }
    }
}

fn assert_eq_formula(left: &Formula, right: &Formula) {
    if left != right {
        panic!("formula round-trip changed");
    }
}

/// Exercise `choose` on a bounded pool built from `bytes`.
pub fn poke_choose(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let direction = shaped_direction(bytes);
    let Ok(rows) = labels_of(&direction) else {
        return;
    };
    let _ = choose(&shaped_state(bytes), &direction, &rows);
    if bytes.len() <= 1_536 {
        if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
            poke_choose_value(&value);
        }
    }
}

fn poke_choose_value(value: &Value) {
    let Some(direction) = value.as_object() else {
        return;
    };
    if direction.len() > 16 {
        return;
    }
    let mut direction = Value::Object(direction.clone());
    if let Some(samples) = direction.get_mut("samples").and_then(Value::as_array_mut) {
        samples.truncate(4);
        for sample in samples.iter_mut() {
            clip_sample(sample);
        }
    }
    let Ok(rows) = labels_of(&direction) else {
        return;
    };
    if rows.len() > 4 {
        return;
    }
    let state = json!({
        "shape": value.get("shape").and_then(Value::as_str).unwrap_or("constant_product"),
        "reserve0": clip_text(value.get("reserve0")),
        "reserve1": clip_text(value.get("reserve1")),
        "fee_pips_used": value.get("fee_pips_used").and_then(Value::as_i64).unwrap_or(0),
        "token0": "a",
        "token1": "b",
        "coins": [],
    });
    let _ = choose(&state, &direction, &rows);
}

fn clip_sample(sample: &mut Value) {
    let Some(sample) = sample.as_object_mut() else {
        return;
    };
    for key in ["amount_in", "chain_out"] {
        if let Some(text) = sample.get(key).and_then(Value::as_str) {
            let clipped: String = text.chars().take(40).collect();
            sample.insert(key.to_string(), Value::String(clipped));
        }
    }
}

fn clip_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or("0")
        .chars()
        .take(40)
        .collect()
}

fn shaped_state(bytes: &[u8]) -> Value {
    let kind = bytes[0] % 5;
    let reserve = word_at(bytes, 1);
    match kind {
        0 => json!({
            "shape": "constant_product",
            "reserve0": reserve.to_string(),
            "reserve1": word_at(bytes, 9).to_string(),
            "fee_pips_used": u16::from_le_bytes([bytes.first().copied().unwrap_or(0), bytes.get(1).copied().unwrap_or(0)]) % 10_000,
            "token0": "a",
            "token1": "b",
        }),
        1 => json!({
            "shape": "curve",
            "amp": word_at(bytes, 1).max(BigInt::one()).to_string(),
            "fee": "1000000",
            "gamma": "0",
            "crypto": false,
            "coins": [
                {"token": "a", "balance": reserve.max(BigInt::one()).to_string(), "rate": "0", "price_scale": "0"},
                {"token": "b", "balance": word_at(bytes, 9).max(BigInt::one()).to_string(), "rate": "0", "price_scale": "0"},
            ],
        }),
        2 => json!({
            "shape": "balancer",
            "amp": "100",
            "amp_precision": "1",
            "fee": "0",
            "tokens": ["a", "b"],
            "balances": [reserve.max(BigInt::one()).to_string(), word_at(bytes, 9).max(BigInt::one()).to_string()],
            "weights": ["1", "1"],
            "stable": bytes[0].is_multiple_of(2),
        }),
        3 => json!({
            "shape": "concentrated",
            "token0": "a",
            "token1": "b",
            "fee_pips": 3000,
            "sqrt_price_x96": word_at(bytes, 1).max(BigInt::one()).to_string(),
            "liquidity": word_at(bytes, 9).max(BigInt::one()).to_string(),
            "tick": 0,
            "ticks": [],
        }),
        _ => json!({
            "shape": "metric",
            "token0": "a",
            "token1": "b",
            "reference_x64": word_at(bytes, 1).max(BigInt::one()).to_string(),
            "scale0": "1",
            "scale1": "1",
            "bin_index": 0,
            "distance_e6": 0,
            "position": "0",
            "bins": [],
        }),
    }
}

fn shaped_direction(bytes: &[u8]) -> Value {
    let mut samples = Vec::new();
    let mut at = 17;
    while at + 8 <= bytes.len() && samples.len() < 4 {
        let amount = word_at(bytes, at);
        let out = word_at(bytes, at + 8);
        if amount.is_positive() && out.is_positive() {
            let fraction = if samples.len() == 2 { "0.25" } else { "0.1" };
            samples.push(json!({
                "amount_in": amount.to_string(),
                "chain_out": out.to_string(),
                "fraction": fraction,
            }));
        }
        at += 16;
    }
    if samples.is_empty() {
        samples.push(json!({"amount_in": "1000", "chain_out": "900", "fraction": "0.1"}));
        samples.push(json!({"amount_in": "4000", "chain_out": "3000", "fraction": "0.5"}));
    }
    json!({
        "token_in": "a",
        "token_out": "b",
        "samples": samples,
    })
}

fn word_at(bytes: &[u8], at: usize) -> BigInt {
    let mut buf = [0u8; 8];
    for (slot, byte) in buf.iter_mut().enumerate() {
        *byte = bytes.get(at + slot).copied().unwrap_or(0);
    }
    BigInt::from(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_bi(text: &str) -> BigInt {
        text.parse().expect("integer")
    }

    fn direction(samples: &[(&str, &str, &str)], token_in: &str, token_out: &str) -> Value {
        json!({
            "token_in": token_in,
            "token_out": token_out,
            "samples": samples.iter().map(|(x, y, fraction)| json!({
                "amount_in": x,
                "chain_out": y,
                "fraction": fraction,
            })).collect::<Vec<_>>()
        })
    }

    fn rows_of(samples: &[(&str, &str, &str)]) -> Vec<(BigInt, BigInt)> {
        samples.iter().map(|(x, y, _)| (parse_bi(x), parse_bi(y))).collect()
    }

    #[test]
    fn a_short_book_and_a_zero_divisor_do_not_panic() {
        let missing = Formula::StablePpms {
            xp: vec![bi(1), bi(1)],
            amp: bi(100),
            i: -1,
            j: 5,
            fee: BigInt::zero(),
            rates: vec![BigInt::zero()],
            ppms: Vec::new(),
        };
        assert!(eval_formula(&missing, &bi(10)).is_none());
        let zero_ppm = Formula::StableScale {
            xp: vec![bi(1_000), bi(1_000)],
            amp: bi(100),
            i: 0,
            j: 1,
            fee: BigInt::zero(),
            rates: vec![BigInt::zero(), BigInt::zero()],
            which: 0,
            ppm: BigInt::zero(),
            denom: bi(1_000_000),
            fee_on_input: true,
        };
        let _ = eval_formula(&zero_ppm, &bi(100));
        poke_formula(&[0xff, 3, 0, 0], &bi(1));
        // Tag 2 then a list length of 0xf4f4f4f4. The length is not allocated.
        poke_formula(&[2, 244, 244, 244, 244, 244], &bi(1));
        poke_formula(&[22u8; 64], &bi(1));
        assert!(Formula::decode(&[2, 244, 244, 244, 244, 244]).is_err());
        poke_choose(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        poke_choose(br#"{"shape":"constant_product","samples":[{"amount_in":"10","chain_out":"9","fraction":"0.1"}]}"#);
    }

    #[test]
    fn logistic_formula_round_trips_off_the_grid() {
        let formula = Formula::Logistic {
            slope: rat_i(2),
            rate: BigRational::new(bi(1), bi(100)),
            knee: rat_i(100),
        };
        let stored = crate::fit_formula::StoredFit {
            token_in: alloy_primitives::Address::ZERO,
            token_out: alloy_primitives::Address::ZERO,
            score: 0,
            name: "cap".to_string(),
            formula: formula.clone(),
        };
        let bytes = crate::fit_formula::encode_fit(0, &[stored]);
        let decoded = crate::fit_formula::decode_fit(&bytes).unwrap();
        assert!(decoded.directions[0].formula == formula);
        assert_eq!(eval_formula(&decoded.directions[0].formula, &bi(50)).unwrap(), bi(67));
        assert_eq!(eval_formula(&decoded.directions[0].formula, &bi(200)).unwrap(), bi(199));
    }

    #[test]
    fn logistic_matches_decimal_context() {
        let predict = logistic_quote(&rat_i(2), &BigRational::new(bi(1), bi(100)), &rat_i(100));
        assert_eq!(predict(bi(50)).unwrap(), bi(67));
        assert_eq!(predict(bi(200)).unwrap(), bi(199));
        let got = he40(ln1exp(&"1.5".parse().unwrap()));
        let expect = he40("1.701413277982752409499482809057091105254".parse().unwrap());
        assert_eq!(got, expect);
        let ln2 = he40(ln_he(&BigDecimal::from(2)));
        let expect_ln2 = he40("0.6931471805599453094172321214581765680755".parse().unwrap());
        assert_eq!(ln2, expect_ln2);
    }

    #[test]
    fn selector_structural() {
        let dust_score = worst_bps(&|_| Some(bi(2_000)), &[(bi(1), bi(1_000))]);
        assert_eq!(dust_score, Some(0));
        let over = worst_bps(&|_| Some(bi(2_001)), &[(bi(1), bi(1_000))]);
        assert_eq!(over, Some(10_010));

        let y_small = product_out(&bi(1000), &bi(1000), &bi(100), &BigInt::zero(), &bi(1_000_000)).unwrap();
        let y_large = product_out(&bi(1000), &bi(1000), &bi(400), &BigInt::zero(), &bi(1_000_000)).unwrap();
        let product = json!({
            "shape": "constant_product",
            "reserve0": "1000",
            "reserve1": "1000",
            "fee_pips_used": 0,
            "token0": "a",
            "token1": "b",
        });
        let small = y_small.to_string();
        let large = y_large.to_string();
        let tied_samples = [("100", small.as_str(), "0.1"), ("400", large.as_str(), "1")];
        let tied = choose(&product, &direction(&tied_samples, "a", "b"), &rows_of(&tied_samples)).unwrap();
        let tied = tied.expect("product tie");
        assert_eq!((tied.score, tied.name), (0, "product"));

        let bow = hyperbola_out(&bi(100_000), &bi(500_000), &bi(400_000), &bi(800_000), &bi(200_000)).unwrap();
        let bow_text = bow.to_string();
        let wrong = json!({
            "shape": "constant_product",
            "reserve0": "100000",
            "reserve1": "100000",
            "fee_pips_used": 0,
            "token0": "a",
            "token1": "b",
        });
        let closer_samples = [
            ("100000", "500000", "0.1"),
            ("200000", bow_text.as_str(), "0.5"),
            ("400000", "800000", "1"),
        ];
        let closer = choose(&wrong, &direction(&closer_samples, "a", "b"), &rows_of(&closer_samples))
            .unwrap()
            .expect("hyperbola");
        assert_eq!(closer.name, "hyperbola");
        assert!(closer.score <= 10);

        let line = [
            ("100000", "1000000", "0.1"),
            ("200000", "1100000", "0.25"),
            ("400000", "1300000", "0.5"),
            ("800000", "1700000", "0.8"),
            ("1000000", "1900000", "1"),
        ];
        let blank = json!({"shape": "curve", "coins": [], "crypto": false});
        let kept = choose(&blank, &direction(&line, "a", "b"), &rows_of(&line)).unwrap().expect("line");
        assert!(kept.score <= 10);
        assert!(kept.name == "chord" || kept.name == "cubic");

        let spiked = [
            ("100", "100", "0.1"),
            ("200", "100000", "0.25"),
            ("400", "500", "0.5"),
            ("900", "900", "1"),
        ];
        assert!(choose(&blank, &direction(&spiked, "a", "b"), &rows_of(&spiked)).unwrap().is_none());

        let train: Vec<(BigInt, BigInt)> = line
            .iter()
            .filter(|(_, _, fraction)| !HOLD_FRACTIONS.contains(fraction))
            .map(|(x, y, _)| (parse_bi(x), parse_bi(y)))
            .collect();
        let hold: Vec<(BigInt, BigInt)> = line
            .iter()
            .filter(|(_, _, fraction)| HOLD_FRACTIONS.contains(fraction))
            .map(|(x, y, _)| (parse_bi(x), parse_bi(y)))
            .collect();
        let refit = chord_quote(&train).expect("chord");
        assert!(worst_bps(refit.as_ref(), &hold).unwrap() <= 10);
    }

    fn pack(samples: &[(&str, &str, &str)], token_in: &str, token_out: &str) -> (Value, Vec<(BigInt, BigInt)>) {
        (direction(samples, token_in, token_out), rows_of(samples))
    }

    fn spike(samples: &[(&str, &str, &str)], token_in: &str, token_out: &str) -> (Value, Vec<(BigInt, BigInt)>) {
        let owned: Vec<(String, String, String)> = samples
            .iter()
            .map(|(x, y, fraction)| {
                let y = parse_bi(y);
                let y = if *fraction == "0.25" || *fraction == "0.8" { y * 2 } else { y };
                ((*x).to_string(), y.to_string(), (*fraction).to_string())
            })
            .collect();
        let refs: Vec<(&str, &str, &str)> =
            owned.iter().map(|(x, y, fraction)| (x.as_str(), y.as_str(), fraction.as_str())).collect();
        pack(&refs, token_in, token_out)
    }

    fn curve_coins(balances: &[&str], amp: &str, fee: &str) -> Value {
        let names = ["a", "b", "c", "d"];
        json!({
            "shape": "curve",
            "crypto": false,
            "amp": amp,
            "fee": fee,
            "coins": balances.iter().enumerate().map(|(n, balance)| json!({
                "token": names[n],
                "balance": balance,
                "rate": "0",
            })).collect::<Vec<_>>()
        })
    }

    fn sized_quotes(maker: impl Fn(&BigInt) -> Option<BigInt>, stored: &BigInt) -> Vec<(String, String, String)> {
        let cuts = [("0.1", fd(stored, &bi(50))), ("0.25", fd(stored, &bi(10))), ("0.5", fd(stored, &bi(3))), ("0.8", fd(stored, &bi(2))), ("1", fd(&(stored * bi(4)), &bi(5)))];
        cuts.into_iter()
            .map(|(fraction, dx)| {
                let quoted = maker(&dx).expect("quote");
                assert!(quoted.is_positive());
                (dx.to_string(), quoted.to_string(), fraction.to_string())
            })
            .collect()
    }

    fn refs_of(owned: &[(String, String, String)]) -> Vec<(&str, &str, &str)> {
        owned.iter().map(|(x, y, fraction)| (x.as_str(), y.as_str(), fraction.as_str())).collect()
    }

    #[test]
    fn selector_scale_windows() {
        let stored = pow10(22);
        let amp = bi(100);
        let ppm_true = bi(15_000);
        let true_xp = vec![fd(&(&stored * &ppm_true), &bi(10_000)), stored.clone()];
        let wide_owned = sized_quotes(
            |dx| {
                let paid = fd(&(dx * &ppm_true), &bi(10_000));
                stable_swap_out(&true_xp, amp.clone(), bi(0), bi(1), paid, BigInt::zero(), fee_denom(), false, BigInt::zero())
            },
            &stored,
        );
        let wide_refs = refs_of(&wide_owned);
        let (wide_dir, wide_rows) = pack(&wide_refs, "a", "b");
        let wide_state = curve_coins(&[&stored.to_string(), &stored.to_string()], "100", "0");
        let wide_got = stable_scale_candidate(&wide_state, 0, 1, &wide_dir, &wide_rows).unwrap().expect("wide scale");
        assert!(wide_got.0 <= 10);
        let chosen = choose(&wide_state, &wide_dir, &wide_rows).unwrap().expect("wide choose");
        assert!(chosen.score <= wide_got.0);
        if chosen.score == wide_got.0 {
            assert_eq!(chosen.name, "stable");
        }

        let bal_owned = sized_quotes(
            |dx| {
                let paid = fd(&(dx * &ppm_true), &bi(10_000));
                stable_swap_out(&true_xp, amp.clone(), bi(0), bi(1), paid, BigInt::zero(), wad(), true, BigInt::zero())
            },
            &stored,
        );
        let bal_refs = refs_of(&bal_owned);
        let (bal_dir, bal_rows) = pack(&bal_refs, "a", "b");
        let bal_state = json!({
            "shape": "balancer",
            "stable": true,
            "amp": "1000",
            "amp_precision": "10",
            "fee": "0",
            "balances": [stored.to_string(), stored.to_string()],
            "weights": [],
            "tokens": ["a", "b"],
        });
        let bal_got = balancer_scale_candidate(&bal_state, 0, 1, &bal_dir, &bal_rows).unwrap().expect("balancer scale");
        assert!(bal_got.0 <= 10);

        let out_xp = vec![stored.clone(), stored.clone()];
        let out_owned = sized_quotes(
            |dx| {
                let quoted = stable_swap_out(&out_xp, bi(100), bi(0), bi(1), dx.clone(), BigInt::zero(), fee_denom(), false, BigInt::zero())?;
                Some(fd(&(&quoted * bi(9960)), &bi(10_000)))
            },
            &stored,
        );
        let out_refs = refs_of(&out_owned);
        let (out_dir, out_rows) = pack(&out_refs, "a", "b");
        let raw: Predict = Arc::new({
            let out_xp = out_xp.clone();
            move |dx| stable_swap_out(&out_xp, bi(100), bi(0), bi(1), dx, BigInt::zero(), fee_denom(), false, BigInt::zero())
        });
        let bare = worst_bps(raw.as_ref(), &out_rows).expect("bare");
        assert!(bare > 10);
        let out_state = curve_coins(&[&stored.to_string(), &stored.to_string()], "100", "0");
        let out_got = stable_output_candidate(&out_state, 0, 1, &out_dir, &out_rows).unwrap().expect("output scale");
        assert!(out_got.0 <= 10);
    }

    #[test]
    fn selector_residuals() {
        let identity = Formula::Identity;
        let level = [
            ("100000", "105000", "0.1"),
            ("250000", "255000", "0.25"),
            ("500000", "505000", "0.5"),
            ("800000", "805000", "0.8"),
            ("1000000", "1005000", "1"),
        ];
        let (level_dir, level_rows) = pack(&level, "a", "b");
        let lifted = base_residual_candidate(&identity, &level_dir, &level_rows).unwrap().expect("lifted");
        assert!(lifted.0 <= 10);

        let spiked_level = [
            ("100000", "100000", "0.1"),
            ("250000", "350000", "0.25"),
            ("500000", "500000", "0.5"),
            ("800000", "800000", "0.8"),
            ("1000000", "1000000", "1"),
        ];
        let (spiked_dir, spiked_rows) = pack(&spiked_level, "a", "b");
        assert!(base_residual_candidate(&identity, &spiked_dir, &spiked_rows).unwrap().is_none());
        let thin = [("100000", "105000", "0.1"), ("250000", "255000", "0.25"), ("800000", "805000", "0.8")];
        let (thin_dir, thin_rows) = pack(&thin, "a", "b");
        assert!(base_residual_candidate(&identity, &thin_dir, &thin_rows).unwrap().is_none());

        let scaled_book = [
            ("115031368", "40565", "0.0001"),
            ("1150313686", "405369", "0.001"),
            ("11503136860", "4028173", "0.01"),
            ("57515684301", "19559573", "0.05"),
            ("115031368603", "37341258", "0.1"),
            ("287578421508", "82120432", "0.25"),
            ("575156843017", "136742416", "0.5"),
            ("920250948828", "182192054", "0.8"),
            ("1092798001734", "199658026", "0.95"),
            ("1150313686035", "204896279", "1"),
        ];
        let (scaled_dir, scaled_rows) = pack(&scaled_book, "a", "b");
        let (scaled_train, _) = split_holdout(&scaled_dir).unwrap();
        let bare_residual = residual_quote(&scaled_train).expect("residual");
        assert!(worst_bps(bare_residual.as_ref(), &scaled_rows).unwrap() > 10);
        let scaled_got = scaled_residual_candidate(&scaled_dir, &scaled_rows).unwrap().expect("scaled");
        assert!(scaled_got.0 <= 10);
        assert!(scaled_residual_candidate(&spiked_dir, &spiked_rows).unwrap().is_none());
        assert!(scaled_residual_candidate(&thin_dir, &thin_rows).unwrap().is_none());

        let flat_book = [
            ("14548", "2395240484", "2461420875", "0.0001"),
            ("145488", "10856634268", "11070063847", "0.001"),
            ("1454880", "66069602810", "67749161460", "0.01"),
            ("7274403", "211903794191", "215625141636", "0.05"),
            ("14548807", "297997313258", "301688177096", "0.1"),
            ("36372019", "395260737247", "397871903127", "0.25"),
            ("72744039", "443764316235", "445406024721", "0.5"),
            ("116390462", "465195596129", "466323721194", "0.8"),
            ("138213674", "471188786545", "472160474742", "0.95"),
            ("145488078", "472811276491", "473741935483", "1"),
        ];
        let mut flat_samples = Vec::new();
        let mut pairs = Vec::new();
        for (x, y, pred, fraction) in flat_book {
            pairs.push((parse_bi(x), parse_bi(pred)));
            flat_samples.push((x, y, fraction));
        }
        let ladder = Formula::Lookup { pairs };
        let (flat_dir, flat_rows) = pack(&flat_samples, "a", "b");
        let (flat_train, _) = split_holdout(&flat_dir).unwrap();
        let clamped = added_residual(ladder.predict(), &flat_train, &BigRational::one()).expect("clamped");
        assert!(worst_bps(clamped.as_ref(), &flat_rows).unwrap() > 10);
        let flat_got = flat_residual_candidate(&ladder, &flat_dir, &flat_rows).unwrap().expect("flat");
        assert!(flat_got.0 <= 10);
        assert!(flat_residual_candidate(&identity, &spiked_dir, &spiked_rows).unwrap().is_none());
    }

    #[test]
    fn selector_loo_alpha() {
        let alpha_samples = [
            ("93", "406", "0.0001"),
            ("938", "4087", "0.001"),
            ("9385", "40744", "0.01"),
            ("46926", "198222", "0.05"),
            ("93852", "376915", "0.1"),
            ("234630", "813440", "0.25"),
            ("469261", "1332487", "0.5"),
            ("750818", "1757190", "0.8"),
            ("891596", "1918921", "0.95"),
            ("938523", "1967274", "1"),
        ];
        let (alpha_dir, alpha_rows) = pack(&alpha_samples, "a", "b");
        let (alpha_train, _) = split_holdout(&alpha_dir).unwrap();
        let alpha_base = hyperbola_candidate(&alpha_rows).expect("hyperbola");
        let clamped = added_residual(alpha_base.1.predict(), &alpha_train, &BigRational::one()).expect("clamped alpha");
        assert!(worst_bps(clamped.as_ref(), &alpha_rows).unwrap() > 10);
        let alpha_got = loo_alpha_candidate(&alpha_base.1, &alpha_dir, &alpha_rows).unwrap().expect("loo");
        assert!(alpha_got.0 <= 10);
        let alpha_state = json!({"shape": "curve", "coins": []});
        let choice = choose(&alpha_state, &alpha_dir, &alpha_rows).unwrap().expect("alpha choose");
        assert!(choice.score <= 10);
        assert!(choice.score <= alpha_got.0);
        let (spiked_dir, spiked_rows) = spike(&alpha_samples, "a", "b");
        let spiked_base = hyperbola_candidate(&spiked_rows).expect("spiked hyperbola");
        assert!(loo_alpha_candidate(&spiked_base.1, &spiked_dir, &spiked_rows).unwrap().is_none());
    }

    fn assert_named(choice: &Chosen, name: &str, limit: i64) {
        assert!(choice.score <= limit, "{}", choice.score);
        assert_eq!(choice.name, name);
    }

    #[test]
    fn selector_joints() {
        let state = curve_coins(&["2546456916", "2316863024", "3983216"], "2000", "4000000");
        let joint_samples = [
            ("617833", "611134", "0.0001"),
            ("6178334", "6111275", "0.001"),
            ("61783341", "61105796", "0.01"),
            ("308916709", "305368078", "0.05"),
            ("617833418", "610278299", "0.1"),
            ("1544583547", "1518777006", "0.25"),
            ("3089167094", "2299616136", "0.5"),
            ("4942667351", "2312289490", "0.8"),
            ("5869417479", "2313538623", "0.95"),
            ("6178334189", "2313808815", "1"),
        ];
        let (joint_dir, joint_rows) = pack(&joint_samples, "a", "b");
        let stored = stable_candidate(&state, 0, 1, &joint_rows).unwrap().expect("stored");
        assert!(stored.0 > 10);
        let one = stable_scale_candidate(&state, 0, 1, &joint_dir, &joint_rows).unwrap();
        assert!(one.as_ref().is_none_or(|item| item.0 > 10));
        let joint_got = stable_joint_candidate(&state, 0, 1, &joint_dir, &joint_rows).unwrap().expect("joint");
        assert!(joint_got.0 <= 10);
        let choice = choose(&state, &joint_dir, &joint_rows).unwrap().expect("joint choose");
        assert!(choice.score <= joint_got.0);
        assert_named(&choice, "stable", 10);
        let (spiked_dir, spiked_rows) = spike(&joint_samples, "a", "b");
        assert!(stable_joint_candidate(&state, 0, 1, &spiked_dir, &spiked_rows).unwrap().is_none());

        let back_samples = [
            ("615342", "621588", "0.0001"),
            ("6153427", "6215815", "0.001"),
            ("61534271", "62151089", "0.01"),
            ("307671355", "310598874", "0.05"),
            ("615342711", "620784689", "0.1"),
            ("1538356779", "1546878243", "0.25"),
            ("3076713558", "2523187763", "0.5"),
            ("4922741693", "2541394325", "0.8"),
            ("5845755761", "2542841442", "0.95"),
            ("6153427116", "2543146698", "1"),
        ];
        let (back_dir, back_rows) = pack(&back_samples, "b", "a");
        let back_stored = stable_candidate(&state, 1, 0, &back_rows).unwrap().expect("back stored");
        assert!(back_stored.0 > 10);
        let back_one = stable_scale_candidate(&state, 1, 0, &back_dir, &back_rows).unwrap();
        assert!(back_one.as_ref().is_none_or(|item| item.0 > 10));
        assert!(stable_joint_candidate(&state, 1, 0, &back_dir, &back_rows).unwrap().is_none());
        let back_got = stable_joint_best_candidate(&state, 1, 0, &back_dir, &back_rows).unwrap().expect("joint best");
        assert!(back_got.0 <= 10);
        let back_choice = choose(&state, &back_dir, &back_rows).unwrap().expect("back choose");
        assert!(back_choice.score <= back_got.0);
        assert_named(&back_choice, "stable", 10);
        let (spiked_back_dir, spiked_back_rows) = spike(&back_samples, "b", "a");
        assert!(stable_joint_best_candidate(&state, 1, 0, &spiked_back_dir, &spiked_back_rows).unwrap().is_none());
    }

    fn blank_curve() -> Value {
        json!({"shape": "curve", "coins": []})
    }

    fn expect_cap(
        samples: &[(&str, &str, &str)],
        run: fn(&Value, &[(BigInt, BigInt)]) -> Result<Option<Scored>, String>,
        limit: i64,
    ) {
        let (dir, rows) = pack(samples, "a", "b");
        let got = run(&dir, &rows).unwrap().expect("candidate");
        assert!(got.0 <= limit, "{}", got.0);
        let choice = choose(&blank_curve(), &dir, &rows).unwrap().expect("choose");
        assert!(choice.score <= got.0, "{} > {}", choice.score, got.0);
        assert_named(&choice, "cap", 10);
        let (spiked_dir, spiked_rows) = spike(samples, "a", "b");
        assert!(run(&spiked_dir, &spiked_rows).unwrap().is_none());
    }

    #[test]
    fn selector_fillet_and_logistic_caps() {
        let fillet = [
            ("5375423", "5485986", "0.0001"),
            ("53754234", "54856464", "0.001"),
            ("537542342", "548227631", "0.01"),
            ("2687711711", "2729262949", "0.05"),
            ("5375423422", "4345001472", "0.1"),
            ("13438558556", "4377017501", "0.25"),
            ("26877117112", "4378770599", "0.5"),
            ("43003387380", "4379090767", "0.8"),
            ("51066522513", "4379149123", "0.95"),
            ("53754234225", "4379162930", "1"),
        ];
        expect_cap(&fillet, fillet_cap_candidate, 10);

        let grid = [
            ("3022185", "21069261", "0.0001"),
            ("30221852", "210691862", "0.001"),
            ("302218524", "2106841153", "0.01"),
            ("1511092620", "10532244219", "0.05"),
            ("3022185241", "21058084913", "0.1"),
            ("7555463103", "52525740806", "0.25"),
            ("15110926206", "79237047434", "0.5"),
            ("24177481929", "79474008911", "0.8"),
            ("28710759791", "79498583385", "0.95"),
            ("30221852412", "79504017100", "1"),
        ];
        let (grid_dir, grid_rows) = pack(&grid, "a", "b");
        assert!(logistic_refine_candidate(&grid_dir, &grid_rows).unwrap().is_none());
        expect_cap(&grid, logistic_cap_candidate, 10);

        let refined = [
            ("411826208851860009610", "440581", "0.0001"),
            ("4118262088518600096103", "4405808", "0.001"),
            ("41182620885186000961034", "44057742", "0.01"),
            ("205913104425930004805172", "220280812", "0.05"),
            ("411826208851860009610344", "440539860", "0.1"),
            ("1029565522129650024025860", "1101129923", "0.25"),
            ("2059131044259300048051720", "2200163593", "0.5"),
            ("3294609670814880076882752", "3013664017", "0.8"),
            ("3912348984092670091298268", "3018012608", "0.95"),
            ("4118262088518600096103440", "3018543490", "1"),
        ];
        let (refine_dir, refine_rows) = pack(&refined, "a", "b");
        assert!(logistic_cap_candidate(&refine_dir, &refine_rows).unwrap().is_none());
        expect_cap(&refined, logistic_refine_candidate, 10);
    }

    #[test]
    fn selector_waves() {
        let series = [
            [
                ("4667", "1224633", "0.0001"),
                ("46672", "12246297", "0.001"),
                ("466720", "122458856", "0.01"),
                ("2333602", "612196439", "0.05"),
                ("4667204", "1224112113", "0.1"),
                ("11668011", "3057097607", "0.25"),
                ("23336022", "6059820178", "0.5"),
                ("37337636", "7083955298", "0.8"),
                ("44338443", "7096893380", "0.95"),
                ("46672045", "7099144840", "1"),
            ],
            [
                ("4667", "1088171", "0.0001"),
                ("46672", "10881697", "0.001"),
                ("466720", "108814837", "0.01"),
                ("2333602", "544025227", "0.05"),
                ("4667204", "1087920138", "0.1"),
                ("11668011", "2718601257", "0.25"),
                ("23336022", "5429021430", "0.5"),
                ("37337636", "8392769757", "0.8"),
                ("44338443", "8604957057", "0.95"),
                ("46672045", "8616365875", "1"),
            ],
        ];
        for samples in series {
            let (dir, rows) = pack(&samples, "a", "b");
            assert!(fillet_cap_candidate(&dir, &rows).unwrap().is_none());
            assert!(logistic_cap_candidate(&dir, &rows).unwrap().is_none());
            assert!(logistic_refine_candidate(&dir, &rows).unwrap().is_none());
            expect_cap(&samples, logistic_wave_candidate, 10);
        }
        let wide = [
            [
                ("3022185241214", "18", "0.0001"),
                ("30221852412148", "187", "0.001"),
                ("302218524121487", "1870", "0.01"),
                ("1511092620607439", "9350", "0.05"),
                ("3022185241214879", "18698", "0.1"),
                ("7555463103037197", "46690", "0.25"),
                ("15110926206074395", "82198", "0.5"),
                ("24177481929719033", "82766", "0.8"),
                ("28710759791541351", "82799", "0.95"),
                ("30221852412148791", "82806", "1"),
            ],
            [
                ("4667204582094", "1245", "0.0001"),
                ("46672045820941", "12454", "0.001"),
                ("466720458209411", "124545", "0.01"),
                ("2333602291047059", "622629", "0.05"),
                ("4667204582094118", "1244976", "0.1"),
                ("11668011455235296", "3109251", "0.25"),
                ("23336022910470592", "6165343", "0.5"),
                ("37337636656752947", "7239830", "0.8"),
                ("44338443529894125", "7253306", "0.95"),
                ("46672045820941184", "7255638", "1"),
            ],
        ];
        for samples in wide {
            expect_cap(&samples, logistic_wide_candidate, 10);
        }
        let wave_loo = [
            ("1255412165039985654", "434150601494383576", "0.0001"),
            ("12554121650399856548", "4341393668540135027", "0.001"),
            ("125541216503998565482", "43402511033146923132", "0.01"),
            ("627706082519992827411", "216735387493943511373", "0.05"),
            ("1255412165039985654822", "432645993685433256752", "0.1"),
            ("3138530412599964137055", "1071407515238329484054", "0.25"),
            ("6277060825199928274110", "1969321888656347844695", "0.5"),
            ("10043297320319885238576", "2183065786340123384542", "0.8"),
            ("11926415567879863720809", "2198779625485002206952", "0.95"),
            ("12554121650399856548221", "2201970551579885219215", "1"),
        ];
        expect_cap(&wave_loo, wave_loo_candidate, 4);
    }

    #[test]
    fn selector_fee_refine() {
        let cases: [(usize, usize, &str, &str, &[(&str, &str, &str)]); 2] = [
            (
                1,
                0,
                "147974849398",
                "1008378630008940969171",
                &[
                    ("925273707933325931", "10068003", "0.0001"),
                    ("9252737079333259310", "99980743", "0.001"),
                    ("92527370793332593108", "935384506", "0.01"),
                    ("462636853966662965544", "3668188883", "0.05"),
                    ("925273707933325931089", "5864773975", "0.1"),
                    ("2313184269833314827722", "9649007359", "0.25"),
                    ("4626368539666629655445", "13426138976", "0.5"),
                    ("7402189663466607448712", "16970020761", "0.8"),
                    ("8790100225366596345345", "18602700198", "0.95"),
                    ("9252737079333259310890", "19135522827", "1"),
                ],
            ),
            (
                0,
                1,
                "9285165",
                "14825833172516094238",
                &[
                    ("928516", "110402526507932229", "0.0001"),
                    ("9285165", "664982939163483950", "0.001"),
                    ("92851650", "1986343000224994734", "0.01"),
                    ("464258250", "5705768274552803321", "0.05"),
                    ("928516500", "10244720388441022256", "0.1"),
                    ("2321291250", "14812074783128551103", "0.25"),
                    ("4642582500", "14818781153909248236", "0.5"),
                    ("7428132000", "14819525040808283310", "0.8"),
                    ("8820906750", "14819644291707933287", "0.95"),
                    ("9285165000", "14819671654549768915", "1"),
                ],
            ),
        ];
        for (coin_in, coin_out, left, right, samples) in cases {
            let names = ["a", "b"];
            let state = curve_coins(&[left, right], "200", "4000000");
            let (dir, rows) = pack(samples, names[coin_in], names[coin_out]);
            let stored = stable_candidate(&state, coin_in, coin_out, &rows).unwrap().expect("stored");
            assert!(stored.0 > 10 && stored.0 <= 400, "{}", stored.0);
            let got = stable_refine_candidate(&state, coin_in, coin_out, &dir, &rows).unwrap().expect("refine");
            assert!(got.0 <= 10, "{}", got.0);
            let choice = choose(&state, &dir, &rows).unwrap().expect("refine choose");
            assert!(choice.score <= got.0);
            assert_named(&choice, "stable", 10);
            let (spiked_dir, spiked_rows) = spike(samples, names[coin_in], names[coin_out]);
            assert!(stable_refine_candidate(&state, coin_in, coin_out, &spiked_dir, &spiked_rows).unwrap().is_none());
        }
    }

    #[test]
    fn selector_consistent_and_mixed() {
        let first = json!({
            "shape": "curve",
            "crypto": true,
            "gamma": "126562100000000",
            "amp": "0",
            "d": "12815659946462930322364730",
            "mid_fee": "207000",
            "out_fee": "69314000",
            "fee_gamma": "4920000000000",
            "coins": [
                {"token": "a", "balance": "4285099268118", "price_scale": "75737331428183937831170", "rate": "0"},
                {"token": "b", "balance": "5613714639", "price_scale": "2235286570743132532000", "rate": "0"},
                {"token": "c", "balance": "1914246883653147602242", "price_scale": "0", "rate": "0"},
            ],
        });
        let first_samples = [
            ("191424688365314760", "425648398", "0.0001"),
            ("1914246883653147602", "4256127463", "0.001"),
            ("19142468836531476022", "42429340539", "0.01"),
            ("95712344182657380112", "205026346100", "0.05"),
            ("191424688365314760224", "390878874549", "0.1"),
            ("478561720913286900560", "857784758655", "0.25"),
            ("957123441826573801121", "1427014660772", "0.5"),
            ("1531397506922518081793", "1900545379336", "0.8"),
            ("1818534539470490222129", "2082496851463", "0.95"),
            ("1914246883653147602242", "2137063828523", "1"),
        ];
        let (dir, rows) = pack(&first_samples, "c", "a");
        let got = crypto3_consistent_candidate(&first, 2, 0, &dir, &rows).unwrap().expect("consistent");
        assert!(got.0 <= 2, "{}", got.0);
        let choice = choose(&first, &dir, &rows).unwrap().expect("consistent choose");
        assert!(choice.score <= got.0);
        assert_named(&choice, "three-coin", 2);
        let (spiked_dir, spiked_rows) = spike(&first_samples, "c", "a");
        assert!(crypto3_consistent_candidate(&first, 2, 0, &spiked_dir, &spiked_rows).unwrap().is_none());

        let second = json!({
            "shape": "curve",
            "crypto": true,
            "gamma": "69999999999999",
            "amp": "3645",
            "d": "289349234090331792140547",
            "mid_fee": "4000000",
            "out_fee": "40000000",
            "fee_gamma": "10000000000000000",
            "coins": [
                {"token": "a", "balance": "91751712394", "price_scale": "78767292027298630679712", "rate": "0"},
                {"token": "b", "balance": "125226144", "price_scale": "2324095769479075828124", "rate": "0"},
                {"token": "c", "balance": "42599448406198332507", "price_scale": "0", "rate": "0"},
            ],
        });
        let second_samples = [
            ("12522", "4247424573662904", "0.0001"),
            ("125226", "42457482330770066", "0.001"),
            ("1252261", "422682754539822034", "0.01"),
            ("6261307", "2059617153289795134", "0.05"),
            ("12522614", "3946195433388600664", "0.1"),
            ("31306536", "8664432804945539603", "0.25"),
            ("62613072", "14386343500120694926", "0.5"),
            ("100180915", "19128341414405320511", "0.8"),
            ("118964836", "20946575943919077096", "0.95"),
            ("125226144", "21491455947841976087", "1"),
        ];
        let (dir, rows) = pack(&second_samples, "b", "c");
        let got = crypto3_consistent_candidate(&second, 1, 2, &dir, &rows).unwrap().expect("consistent 2");
        assert!(got.0 <= 2, "{}", got.0);
        let choice = choose(&second, &dir, &rows).unwrap().expect("consistent choose 2");
        assert!(choice.score <= got.0);
        assert_named(&choice, "three-coin", 2);
        let (spiked_dir, spiked_rows) = spike(&second_samples, "b", "c");
        assert!(crypto3_consistent_candidate(&second, 1, 2, &spiked_dir, &spiked_rows).unwrap().is_none());

        let mixed_a = curve_coins(
            &["6616494140643702778", "226374747", "721", "1550859921615324669071"],
            "100",
            "4000000",
        );
        let mixed_a_samples = [
            ("155085992161532466", "2021942806401156", "0.0001"),
            ("1550859921615324669", "20176503191581999", "0.001"),
            ("15508599216153246690", "197562081339725637", "0.01"),
            ("77542996080766233453", "903343924658862851", "0.05"),
            ("155085992161532466907", "1629583266247721677", "0.1"),
            ("387714980403831167267", "3126129796109520355", "0.25"),
            ("775429960807662334535", "4448793926771016987", "0.5"),
            ("1240687937292259735256", "5233033205196985311", "0.8"),
            ("1473316925534558435617", "5473178570286745061", "0.95"),
            ("1550859921615324669071", "5539336923065416677", "1"),
        ];
        let (dir, rows) = pack(&mixed_a_samples, "d", "a");
        let got = stable_mixed_candidate(&mixed_a, 3, 0, &dir, &rows).unwrap().expect("mixed");
        assert_eq!(got.0, 0);
        let choice = choose(&mixed_a, &dir, &rows).unwrap().expect("mixed choose");
        assert!(choice.score <= got.0);
        assert_named(&choice, "stable", 0);
        let (spiked_dir, spiked_rows) = spike(&mixed_a_samples, "d", "a");
        assert!(stable_mixed_candidate(&mixed_a, 3, 0, &spiked_dir, &spiked_rows).unwrap().is_none());

        let mixed_b = curve_coins(
            &["333055825", "332496894", "184585815059", "334947017487739244032"],
            "500",
            "3000000",
        );
        let mixed_b_samples = [
            ("306540978878428895", "70409589", "0.0001"),
            ("3065409788784288955", "699751212", "0.001"),
            ("30654097887842889553", "6593472787", "0.01"),
            ("153270489439214447766", "26387745588", "0.05"),
            ("306540978878428895533", "42566087216", "0.1"),
            ("766352447196072238832", "68612592162", "0.25"),
            ("1532704894392144477665", "87977291934", "0.5"),
            ("2452327831027431164264", "99626097806", "0.8"),
            ("2912139299345074507563", "103529306006", "0.95"),
            ("3065409788784288955330", "104658453324", "1"),
        ];
        let (dir, rows) = pack(&mixed_b_samples, "d", "c");
        let got = stable_mixed_candidate(&mixed_b, 3, 2, &dir, &rows).unwrap().expect("mixed b");
        assert_eq!(got.0, 0);
        let choice = choose(&mixed_b, &dir, &rows).unwrap().expect("mixed choose b");
        assert!(choice.score <= got.0);
        assert_named(&choice, "stable", 0);
        let (spiked_dir, spiked_rows) = spike(&mixed_b_samples, "d", "c");
        assert!(stable_mixed_candidate(&mixed_b, 3, 2, &spiked_dir, &spiked_rows).unwrap().is_none());
    }
}
