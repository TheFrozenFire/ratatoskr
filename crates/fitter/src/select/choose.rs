//! Search order for one direction. The order matches `cas/select_fit.py`.

use crate::select::cap::*;
use crate::select::curve::*;
use crate::select::residual::*;
use crate::select::sample::*;
use crate::select::score::*;
use crate::select::shape::*;
use crate::select::spline::*;
use num_bigint::BigInt;
use num_traits::Zero;
use serde_json::Value;

pub fn choose(
    state: &Value,
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Option<Chosen>, String> {
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
        Some("constant_product") => consider(
            &mut found,
            "product",
            &product_candidate(state, direction, rows)?,
        ),
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
                    let typed: Vec<String> = tokens
                        .iter()
                        .filter_map(|token| token.as_str().map(str::to_string))
                        .collect();
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
                            consider(
                                &mut found,
                                "weighted",
                                &weighted_candidate(state, i, j, rows)?,
                            );
                        }
                    }
                }
            }
        }
        Some("metric") => consider(&mut found, "bins", &bin_candidate(state, direction, rows)?),
        _ => {}
    }
    if found.iter().any(|item| item.score == 0) {
        return Ok(chosen(found));
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
            consider(
                &mut found,
                "residual",
                &base_residual_candidate(formula, direction, rows)?,
            );
            consider(
                &mut found,
                "residual",
                &flat_residual_candidate(formula, direction, rows)?,
            );
        }
        consider(
            &mut found,
            "residual",
            &scaled_residual_candidate(direction, rows)?,
        );
        if let Some((_, formula)) = hyperbola_candidate(rows) {
            consider(
                &mut found,
                "residual",
                &loo_alpha_candidate(&formula, direction, rows)?,
            );
        }
        if let Some((i, j)) = fit_three {
            consider(
                &mut found,
                "three-coin",
                &crypto3_fit_candidate(state, i, j, direction, rows)?,
            );
            consider(
                &mut found,
                "three-coin",
                &crypto3_consistent_candidate(state, i, j, direction, rows)?,
            );
        }
        if let Some((i, j)) = fit_stable {
            consider(
                &mut found,
                "stable",
                &stable_mixed_candidate(state, i, j, direction, rows)?,
            );
            consider(
                &mut found,
                "stable",
                &stable_scale_candidate(state, i, j, direction, rows)?,
            );
            consider(
                &mut found,
                "stable",
                &stable_output_candidate(state, i, j, direction, rows)?,
            );
            if stable_got.as_ref().is_some_and(|item| item.0 <= 400) {
                consider(
                    &mut found,
                    "stable",
                    &stable_refine_candidate(state, i, j, direction, rows)?,
                );
                if state
                    .get("coins")
                    .and_then(Value::as_array)
                    .is_some_and(|coins| coins.len() >= 3)
                {
                    consider(
                        &mut found,
                        "stable",
                        &stable_joint_candidate(state, i, j, direction, rows)?,
                    );
                    consider(
                        &mut found,
                        "stable",
                        &stable_joint_best_candidate(state, i, j, direction, rows)?,
                    );
                }
            }
        }
        if let Some((i, j)) = fit_balancer {
            consider(
                &mut found,
                "balancer-stable",
                &balancer_scale_candidate(state, i, j, direction, rows)?,
            );
        }
        consider(&mut found, "cap", &fillet_cap_candidate(direction, rows)?);
        consider(&mut found, "cap", &logistic_cap_candidate(direction, rows)?);
        consider(
            &mut found,
            "cap",
            &logistic_refine_candidate(direction, rows)?,
        );
        let near = near_wave(direction, rows)?;
        let (_, hold) = split_holdout(direction)?;
        let near_scored = near
            .as_ref()
            .and_then(|formula| accept_cap(formula.clone(), &hold, rows));
        consider(&mut found, "cap", &near_scored);
        consider(&mut found, "cap", &wave_loo_from(near, direction, rows)?);
        consider(
            &mut found,
            "cap",
            &logistic_wide_candidate(direction, rows)?,
        );
    }
    Ok(chosen(found))
}

fn chosen(found: Vec<Found>) -> Option<Chosen> {
    found
        .into_iter()
        .min_by(|left, right| {
            (left.score, left.rank, left.name).cmp(&(right.score, right.rank, right.name))
        })
        .map(|item| Chosen {
            score: item.score,
            name: item.name,
            formula: item.formula,
        })
}
