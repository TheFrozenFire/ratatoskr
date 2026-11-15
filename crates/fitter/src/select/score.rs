//! Rank a candidate by its worst basis-point error.

use crate::formula::Formula;
use crate::predict::{worst_bps, Predict};
use crate::units::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};

pub struct Chosen {
    pub score: i64,
    pub name: &'static str,
    pub formula: Formula,
}

pub(crate) struct Found {
    pub(crate) score: i64,
    pub(crate) rank: i64,
    pub(crate) name: &'static str,
    pub(crate) formula: Formula,
}

pub(crate) fn rank_of(name: &str) -> i64 {
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

pub(crate) fn consider(found: &mut Vec<Found>, name: &'static str, got: &Option<Scored>) {
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

pub(crate) fn score_predict(formula: Formula, rows: &[(BigInt, BigInt)]) -> Option<Scored> {
    let score = worst_bps(formula.predict().as_ref(), rows)?;
    Some((score, formula))
}

pub(crate) fn worse(best: &Option<Scored>, score: i64) -> bool {
    best.as_ref().map(|item| score < item.0).unwrap_or(true)
}

pub(crate) fn take_score(best: &mut Option<Scored>, got: Option<Scored>) {
    let Some(got) = got else {
        return;
    };
    if worse(best, got.0) {
        *best = Some(got);
    }
}

pub(crate) fn best_over(
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
        if choices
            .iter()
            .all(|choice| choice.iter().any(|item| item == &prec))
        {
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

pub(crate) fn for_product(choices: &[Vec<BigInt>], mut visit: impl FnMut(&[BigInt])) {
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

pub(crate) fn worst_rel(predict: &Predict, rows: &[(BigInt, BigInt)]) -> Option<BigRational> {
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

pub(crate) fn arrived(rows: &[(BigInt, BigInt)], tenths: i64) -> bool {
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

pub(crate) fn flattens(rows: &[(BigInt, BigInt)]) -> bool {
    arrived(rows, 7)
}

pub(crate) fn softens(rows: &[(BigInt, BigInt)]) -> bool {
    arrived(rows, 6)
}

pub(crate) fn gated(
    formula: &Formula,
    hold: &[(BigInt, BigInt)],
    rows: &[(BigInt, BigInt)],
) -> Option<Scored> {
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

pub(crate) fn accept_cap(
    formula: Formula,
    hold: &[(BigInt, BigInt)],
    rows: &[(BigInt, BigInt)],
) -> Option<Scored> {
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

pub(crate) fn keep_rel(
    best: &mut Option<(BigRational, Formula)>,
    rel: BigRational,
    formula: Formula,
) {
    if best
        .as_ref()
        .map(|(so_far, _)| &rel < so_far)
        .unwrap_or(true)
    {
        *best = Some((rel, formula));
    }
}

pub(crate) fn thirds_i(left: i64, right: i64) -> (i64, i64) {
    let third = (right - left).div_euclid(3);
    (left + third, right - third)
}
