//! Fillet, logistic, and wave candidates for a book that flattens.

use crate::formula::Formula;
use crate::select::curve::{coord_search, i64s, rel_better, RelHit};
use crate::select::residual::wave_loo_from;
use crate::select::sample::*;
use crate::select::score::*;
use crate::units::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, Zero};
use serde_json::Value;

pub(crate) const CAP_SHARPNESS: [i64; 4] = [8000, 12000, 16000, 24000];

pub(crate) fn opening(
    target: &[(BigInt, BigInt)],
) -> Option<(BigRational, BigRational, BigRational, BigInt)> {
    let mut slope0: Option<BigRational> = None;
    let mut cap_int: Option<BigInt> = None;
    for (amount, out) in target {
        if !amount.is_positive() {
            return None;
        }
        let slope = BigRational::new(out.clone(), amount.clone());
        if slope0
            .as_ref()
            .map(|so_far| &slope > so_far)
            .unwrap_or(true)
        {
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

pub(crate) fn logistic_at(
    slope0: &BigRational,
    knee0: &BigRational,
    s_ppm: i64,
    sharp: i64,
    x_ppm: i64,
) -> Option<Formula> {
    if s_ppm <= 0 || sharp <= 0 || x_ppm <= 0 || knee0 <= &BigRational::zero() {
        return None;
    }
    let ten_k = rat_i(10_000);
    let slope = slope0 * BigRational::from_integer(bi(s_ppm)) / &ten_k;
    let rate = BigRational::new(bi(sharp), bi(10_000)) * rat_i(10) / knee0;
    let knee = knee0 * BigRational::from_integer(bi(x_ppm)) / &ten_k;
    Some(Formula::Logistic { slope, rate, knee })
}

pub(crate) fn scaled_formula(base: Formula, ppm: i64) -> Formula {
    Formula::Scaled {
        base: Box::new(base),
        ppm,
    }
}

pub(crate) fn wave_formula(
    base: Formula,
    cap: &BigInt,
    x_last: &BigInt,
    a_ppm: i64,
    b_ppm: i64,
) -> Formula {
    Formula::Wave {
        base: Box::new(base),
        cap: cap.clone(),
        x_last: x_last.clone(),
        a_ppm,
        b_ppm,
    }
}

pub(crate) fn fillet_cap_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
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
                        || left_score.as_ref().is_some_and(|left| {
                            right_score.as_ref().is_none_or(|right| left <= right)
                        });
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

pub(crate) fn logistic_cap_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
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

pub(crate) fn logistic_refine_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let (train, hold) = split_holdout(direction)?;
    let target = train_rows(&train, rows);
    if !flattens(target) {
        return Ok(None);
    }
    let Some((slope0, _, knee0, _)) = opening(target) else {
        return Ok(None);
    };
    let mut best: Option<(BigRational, Formula)> = None;
    let consider = |best: &mut Option<(BigRational, Formula)>,
                    s_ppm: i64,
                    sharp: i64,
                    x_ppm: i64|
     -> Option<BigRational> {
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
                        || left_score.as_ref().is_some_and(|left| {
                            right_score.as_ref().is_none_or(|right| left <= right)
                        });
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

#[derive(Clone, Copy)]
pub(crate) struct WaveWindow {
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
pub(crate) fn search_sharp(
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

pub(crate) fn logistic_wave_search(
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
    let x_last = ordered
        .last()
        .map(|row| row.0.clone())
        .unwrap_or_else(BigInt::zero);
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

pub(crate) fn near_wave(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Formula>, String> {
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

pub(crate) fn logistic_wave_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some(predict) = near_wave(direction, rows)? else {
        return Ok(None);
    };
    let (_, hold) = split_holdout(direction)?;
    Ok(accept_cap(predict, &hold, rows))
}

pub(crate) fn logistic_wide_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    let Some(predict) = logistic_wave_search(
        direction,
        rows,
        WaveWindow {
            starts: &[
                [10000, 10000, 10000],
                [10000, 4000, 10000],
                [10000, 20000, 10000],
            ],
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

pub(crate) fn wave_loo_candidate(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Scored>, String> {
    wave_loo_from(near_wave(direction, rows)?, direction, rows)
}
