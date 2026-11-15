//! Labeled samples on one direction.

use crate::units::*;
use num_bigint::BigInt;
use num_traits::Zero;
use serde_json::Value;

pub(crate) fn truthy(value: Option<&Value>) -> bool {
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

pub(crate) fn amount_or_zero(value: Option<&Value>) -> Result<BigInt, String> {
    Ok(amount(value)?.unwrap_or_else(BigInt::zero))
}

/// `primary or fallback`, where zero and null fall through.
pub(crate) fn amount_or(primary: Option<&Value>, fallback: BigInt) -> Result<BigInt, String> {
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

pub(crate) fn index_of(tokens: &[String], token: Option<&Value>) -> Option<usize> {
    let token = token?.as_str()?;
    let key = token.to_lowercase();
    tokens.iter().position(|item| item.to_lowercase() == key)
}

pub(crate) fn string_tokens(coins: &[Value]) -> Option<Vec<String>> {
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

pub(crate) fn require_object<'a>(
    value: &'a Value,
    message: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    value.as_object().ok_or_else(|| message.to_string())
}

pub(crate) fn coin_field(coin: &Value, key: &str) -> Result<BigInt, String> {
    let coin = require_object(coin, "coin")?;
    amount_or_zero(coin.get(key))
}

pub(crate) fn split_holdout(
    direction: &Value,
) -> Result<(Vec<(BigInt, BigInt)>, Vec<(BigInt, BigInt)>), String> {
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

pub(crate) fn train_rows<'a>(
    train: &'a [(BigInt, BigInt)],
    rows: &'a [(BigInt, BigInt)],
) -> &'a [(BigInt, BigInt)] {
    if train.len() >= 2 {
        train
    } else {
        rows
    }
}

pub(crate) fn amount_vec(value: Option<&Value>) -> Result<Vec<BigInt>, String> {
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

pub(crate) fn nonzero_amount(value: Option<&Value>, fallback: BigInt) -> Result<BigInt, String> {
    match amount(value)? {
        Some(parsed) if !parsed.is_zero() => Ok(parsed),
        _ => Ok(fallback),
    }
}
