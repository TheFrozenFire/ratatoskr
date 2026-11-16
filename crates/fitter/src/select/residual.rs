//! Residual and leave-one-out candidates on top of a base formula.

use crate::formula::Formula;
use crate::predict::{added_residual, residual_quote, worst_bps};
use crate::select::sample::*;
use crate::select::score::*;
use crate::units::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use serde_json::Value;

pub(crate) fn base_residual_candidate(
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

pub(crate) fn flat_residual_candidate(
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

pub(crate) fn scaled_quote(knots: &[(BigInt, BigInt)], ppm: &BigInt) -> Option<Formula> {
    residual_quote(knots)?;
    Some(Formula::ScaledResidual {
        knots: knots.to_vec(),
        ppm: ppm.clone(),
    })
}

pub(crate) fn scaled_residual_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    if train.len() < 3 {
        return Ok(None);
    }
    let mut best: Option<(i64, BigInt)> = None;
    let take = |best: &mut Option<(i64, BigInt)>, ppm: &BigInt| -> Option<i64> {
        let mut worst = 0i64;
        for index in 0..train.len() {
            let knots: Vec<(BigInt, BigInt)> = train
                .iter()
                .enumerate()
                .filter(|(n, _)| *n != index)
                .map(|(_, row)| row.clone())
                .collect();
            let predict = scaled_quote(&knots, ppm)?.predict();
            let score = worst_bps(&|dx| predict(dx.clone()), &train[index..index + 1])?;
            worst = worst.max(score);
        }
        let tie_closer = best.as_ref().is_some_and(|(score, chosen)| {
            *score == worst && (ppm - bi(10_000)).abs() < (chosen - bi(10_000)).abs()
        });
        if best
            .as_ref()
            .map(|(score, _)| worst < *score)
            .unwrap_or(true)
            || tie_closer
        {
            *best = Some((worst, ppm.clone()));
        }
        Some(worst)
    };
    for ppm in [
        10_000, 9990, 10010, 9980, 10020, 9970, 10030, 9950, 10050, 9940, 10060, 9920, 10080, 9900,
        10100,
    ] {
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
        if right_score.is_none()
            || left_score.is_some_and(|left_score| {
                right_score.is_none_or(|right_score| left_score <= right_score)
            })
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

pub(crate) fn loo_worst(
    base: &Formula,
    train: &[(BigInt, BigInt)],
    alpha: &BigRational,
) -> Option<i64> {
    let mut worst = 0i64;
    for index in 0..train.len() {
        let knots: Vec<(BigInt, BigInt)> = train
            .iter()
            .enumerate()
            .filter(|(n, _)| *n != index)
            .map(|(_, row)| row.clone())
            .collect();
        let predict = added_residual(base.predict(), &knots, alpha)?;
        let score = worst_bps(&|dx| predict(dx.clone()), &train[index..index + 1])?;
        worst = worst.max(score);
    }
    Some(worst)
}

pub(crate) fn alphas() -> Vec<BigRational> {
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

pub(crate) fn loo_alpha_candidate(
    base: &Formula,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
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
        if best
            .as_ref()
            .map(|(so_far, pref, _)| (score, prefer) < (*so_far, *pref))
            .unwrap_or(true)
        {
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

pub(crate) fn wave_loo_from(
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
