//! JSON for a stored formula.
//!
//! The field names are the extract schema. The formula bytes are unchanged.

use alloy_primitives::{Address, U256};
use fitter::{Formula, StoredFit};
use num_bigint::BigInt;
use num_rational::BigRational;
use serde_json::{json, Value};

pub(super) fn fit_json(fit: &StoredFit) -> Value {
    json!({
        "token_in": addr(fit.token_in),
        "token_out": addr(fit.token_out),
        "score": fit.score,
        "name": fit.name,
        "formula": formula_json(&fit.formula),
    })
}

pub(super) fn bi_json(value: &BigInt) -> String {
    value.to_string()
}

pub(super) fn bis_json(values: &[BigInt]) -> Vec<String> {
    values.iter().map(bi_json).collect()
}

pub(super) fn pairs_json(values: &[(BigInt, BigInt)]) -> Vec<Value> {
    values
        .iter()
        .map(|(left, right)| json!([bi_json(left), bi_json(right)]))
        .collect()
}

pub(super) fn rat_json(value: &BigRational) -> Value {
    json!({
        "n": value.numer().to_string(),
        "d": value.denom().to_string(),
    })
}

pub(super) fn formula_json(formula: &Formula) -> Value {
    match formula {
        Formula::Hyperbola { x1, y1, x2, y2 } => json!({
            "kind": "hyperbola",
            "x1": bi_json(x1), "y1": bi_json(y1), "x2": bi_json(x2), "y2": bi_json(y2),
        }),
        Formula::Stable {
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
        } => json!({
            "kind": "stable",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j,
            "fee": bi_json(fee), "denom": bi_json(denom), "offpeg": bi_json(offpeg),
            "rate_i": bi_json(rate_i), "rate_j": bi_json(rate_j),
            "prec_i": bi_json(prec_i), "prec_j": bi_json(prec_j),
        }),
        Formula::Crypto2 {
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
        } => json!({
            "kind": "crypto2",
            "balances": bis_json(balances), "precs": bis_json(precs), "scale": bi_json(scale),
            "ann": bi_json(ann), "gamma": bi_json(gamma), "d": bi_json(d), "i": i, "j": j,
            "mid": bi_json(mid), "out_fee": bi_json(out_fee), "fee_gamma": bi_json(fee_gamma),
        }),
        Formula::FlatPair {
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
        } => json!({
            "kind": "flat_pair",
            "balances": bis_json(balances), "precs": bis_json(precs), "scale": bi_json(scale),
            "amp": bi_json(amp), "d": bi_json(d), "i": i, "j": j,
            "mid": bi_json(mid), "out_fee": bi_json(out_fee), "fee_gamma": bi_json(fee_gamma),
            "flat_fee": flat_fee,
        }),
        Formula::Crypto3 {
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
        } => json!({
            "kind": "crypto3",
            "balances": bis_json(balances), "precs": bis_json(precs), "scales": bis_json(scales),
            "ann": bi_json(ann), "gamma": bi_json(gamma), "d": bi_json(d), "i": i, "j": j,
            "mid": bi_json(mid), "out_fee": bi_json(out_fee), "fee_gamma": bi_json(fee_gamma),
        }),
        Formula::StableScale {
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
        } => json!({
            "kind": "stable_scale",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j, "fee": bi_json(fee),
            "rates": bis_json(rates), "which": which, "ppm": bi_json(ppm),
            "denom": bi_json(denom), "fee_on_input": fee_on_input,
        }),
        Formula::StableOutput {
            xp,
            amp,
            i,
            j,
            fee,
            rates,
            ppm,
        } => json!({
            "kind": "stable_output",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j,
            "fee": bi_json(fee), "rates": bis_json(rates), "ppm": bi_json(ppm),
        }),
        Formula::StablePpms {
            xp,
            amp,
            i,
            j,
            fee,
            rates,
            ppms,
        } => json!({
            "kind": "stable_ppms",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j,
            "fee": bi_json(fee), "rates": bis_json(rates), "ppms": bis_json(ppms),
        }),
        Formula::StableRefine {
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
        } => json!({
            "kind": "stable_refine",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j,
            "fee": bi_json(fee), "offpeg": bi_json(offpeg),
            "rate_i": bi_json(rate_i), "prec_i": bi_json(prec_i),
            "ppm_i": bi_json(ppm_i), "ppm_j": bi_json(ppm_j),
            "rate_j": bi_json(rate_j), "prec_j": bi_json(prec_j), "out_ppm": bi_json(out_ppm),
        }),
        Formula::Product {
            reserve_in,
            reserve_out,
            fee,
            denom,
        } => json!({
            "kind": "product",
            "reserve_in": bi_json(reserve_in), "reserve_out": bi_json(reserve_out),
            "fee": bi_json(fee), "denom": bi_json(denom),
        }),
        Formula::Weighted {
            balance_in,
            balance_out,
            weight_in,
            weight_out,
            fee,
        } => json!({
            "kind": "weighted",
            "balance_in": bi_json(balance_in), "balance_out": bi_json(balance_out),
            "weight_in": bi_json(weight_in), "weight_out": bi_json(weight_out), "fee": bi_json(fee),
        }),
        Formula::BalancerStable {
            xp,
            amp,
            i,
            j,
            fee,
            prec_i,
            prec_j,
        } => json!({
            "kind": "balancer_stable",
            "xp": bis_json(xp), "amp": bi_json(amp), "i": i, "j": j,
            "fee": bi_json(fee), "prec_i": bi_json(prec_i), "prec_j": bi_json(prec_j),
        }),
        Formula::Ladder {
            sqrt_price,
            liquidity,
            tick,
            fee,
            ticks,
            zero_for_one,
        } => json!({
            "kind": "ladder",
            "sqrt_price": bi_json(sqrt_price), "liquidity": bi_json(liquidity), "tick": bi_json(tick),
            "fee": bi_json(fee), "ticks": pairs_json(ticks), "zero_for_one": zero_for_one,
        }),
        Formula::Bins {
            reference,
            distance,
            length,
            position,
            selling0,
            scaled_out,
            scale_out,
        } => json!({
            "kind": "bins",
            "reference": bi_json(reference), "distance": bi_json(distance), "length": bi_json(length),
            "position": bi_json(position), "selling0": selling0,
            "scaled_out": bi_json(scaled_out), "scale_out": bi_json(scale_out),
        }),
        Formula::Chord { knots } => json!({"kind": "chord", "knots": pairs_json(knots)}),
        Formula::Cubic { knots } => json!({"kind": "cubic", "knots": pairs_json(knots)}),
        Formula::HyperResidual { knots } => {
            json!({"kind": "hyper_residual", "knots": pairs_json(knots)})
        }
        Formula::Added { base, knots, alpha } => json!({
            "kind": "added",
            "base": formula_json(base),
            "knots": pairs_json(knots),
            "alpha": rat_json(alpha),
        }),
        Formula::ScaledResidual { knots, ppm } => json!({
            "kind": "scaled_residual",
            "knots": pairs_json(knots),
            "ppm": bi_json(ppm),
        }),
        Formula::Fillet { slope, cap, xa, xb } => json!({
            "kind": "fillet",
            "slope": rat_json(slope), "cap": rat_json(cap), "xa": rat_json(xa), "xb": rat_json(xb),
        }),
        Formula::Logistic { slope, rate, knee } => json!({
            "kind": "logistic",
            "slope": rat_json(slope), "rate": rat_json(rate), "knee": rat_json(knee),
        }),
        Formula::Scaled { base, ppm } => json!({
            "kind": "scaled",
            "base": formula_json(base),
            "ppm": ppm,
        }),
        Formula::Wave {
            base,
            cap,
            x_last,
            a_ppm,
            b_ppm,
        } => json!({
            "kind": "wave",
            "base": formula_json(base),
            "cap": bi_json(cap), "x_last": bi_json(x_last), "a_ppm": a_ppm, "b_ppm": b_ppm,
        }),
        Formula::Identity => json!({"kind": "identity"}),
        Formula::Lookup { pairs } => json!({"kind": "lookup", "pairs": pairs_json(pairs)}),
    }
}

pub(super) fn opt_u256(value: Option<U256>) -> Value {
    match value {
        Some(value) => Value::String(value.to_string()),
        None => Value::Null,
    }
}

pub(super) fn addr(value: Address) -> String {
    format!("{value}")
}
