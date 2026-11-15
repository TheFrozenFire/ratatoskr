//! Fuzz and bench entry points.

use super::choose;
use crate::eval::{eval_formula, logistic_quote};
use crate::formula::Formula;
use crate::select::sample::labels_of;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed};
use serde_json::{json, Value};

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
    amounts
        .iter()
        .map(|amount| predict(amount.clone()))
        .collect()
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
    if let Ok(fit) = crate::formula::decode_fit(bytes) {
        for direction in &fit.directions {
            let _ = eval_formula(&direction.formula, amount);
        }
        let encoded = crate::formula::encode_fit(fit.reason, &fit.directions);
        let again = crate::formula::decode_fit(&encoded).expect("encoded fit");
        if again.directions.len() != fit.directions.len() {
            panic!("fit direction count changed");
        }
        for (left, right) in fit.directions.iter().zip(&again.directions) {
            assert_eq_formula(&left.formula, &right.formula);
        }
    }
}

pub(crate) fn assert_eq_formula(left: &Formula, right: &Formula) {
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

pub(crate) fn poke_choose_value(value: &Value) {
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

pub(crate) fn clip_sample(sample: &mut Value) {
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

pub(crate) fn clip_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or("0")
        .chars()
        .take(40)
        .collect()
}

pub(crate) fn shaped_state(bytes: &[u8]) -> Value {
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

pub(crate) fn shaped_direction(bytes: &[u8]) -> Value {
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

pub(crate) fn word_at(bytes: &[u8], at: usize) -> BigInt {
    let mut buf = [0u8; 8];
    for (slot, byte) in buf.iter_mut().enumerate() {
        *byte = bytes.get(at + slot).copied().unwrap_or(0);
    }
    BigInt::from(u64::from_le_bytes(buf))
}
