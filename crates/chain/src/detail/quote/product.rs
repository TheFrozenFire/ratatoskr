use super::super::types::*;
use super::mul_div::mul_div;
use alloy_primitives::{Address, U256};
use store_model::PANCAKE_V2;

pub(super) fn quote_product(
    pool: &ConstantProduct,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let (reserve_in, reserve_out) = if token_in == pool.token0 && token_out == pool.token1 {
        (pool.reserve0, pool.reserve1)
    } else if token_in == pool.token1 && token_out == pool.token0 {
        (pool.reserve1, pool.reserve0)
    } else {
        return Err("token".into());
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err("reserves".into());
    }
    let fee = product_fee_pips(pool.kind, pool.fee_pips);
    constant_product_out(
        reserve_in,
        reserve_out,
        amount_in,
        U256::from(fee),
        U256::from(1_000_000),
    )
}

pub fn product_fee_pips(kind: u8, raw: Option<u32>) -> u32 {
    match raw {
        Some(fee) if fee <= 100 => fee.saturating_mul(100),
        Some(fee) if fee <= 1_000_000 => fee,
        _ => {
            if kind == PANCAKE_V2 {
                2500
            } else {
                3000
            }
        }
    }
}

pub(super) fn constant_product_out(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee: U256,
    fee_denom: U256,
) -> Result<Quote, String> {
    if fee > fee_denom {
        return Err("fee".into());
    }
    let after = mul_div(amount_in, fee_denom - fee, fee_denom)?;
    let out = mul_div(reserve_out, after, reserve_in + after)?;
    Ok(Quote {
        amount_in,
        amount_out: out,
        fee_paid: amount_in - after,
        exhausted: false,
    })
}
