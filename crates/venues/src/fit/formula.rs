//! Address and amount text for the selector's sample JSON.

use alloy_primitives::{Address, U256};
use serde_json::Value;

pub(super) fn opt_u256(value: Option<U256>) -> Value {
    match value {
        Some(value) => Value::String(value.to_string()),
        None => Value::Null,
    }
}

pub(super) fn addr(value: Address) -> String {
    format!("{value}")
}
