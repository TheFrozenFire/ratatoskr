//! One pool snapshot turned into an execution-lens call.

use super::abi::{
    encode_quote_bal2, encode_quote_bal3, encode_quote_curve, encode_quote_metric, encode_quote_v2,
    encode_quote_v3, encode_quote_v4,
};
use super::QUOTE_LENS;
use crate::catalog::{
    AERO, BALANCER_V2, BALANCER_V3, PANCAKE_V3, SUSHI_V3, UNI_V3, UNI_V4, V3_FORK,
};
use alloy_primitives::{Address, U256};
use chain::detail::{product_fee_pips, PoolDetail};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Family {
    V2,
    V3,
    V4,
    Curve,
    Bal2,
    Bal3,
    Aero,
    Metric,
}

pub(super) struct Prepared {
    pub(super) family: Family,
    pub(super) target: Address,
    pub(super) method: &'static str,
    pub(super) data: Vec<u8>,
    pub(super) alt: Option<Vec<u8>>,
    pub(super) alt_method: Option<&'static str>,
    pub(super) token_in: Address,
    pub(super) fund_eth: bool,
}

pub(super) enum Prep {
    Ready(Prepared),
    Skip(String),
}

pub(super) fn prepare(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    amount: U256,
    block: u64,
) -> Prep {
    if block == 0 {
        return Prep::Skip("snapshot block is 0".into());
    }
    let built = match detail {
        PoolDetail::ConstantProduct(pool) => prepare_product(pool, token_in, token_out, amount),
        PoolDetail::Concentrated(pool) => prepare_concentrated(pool, token_in, token_out, amount),
        PoolDetail::Balancer(pool) => prepare_balancer(pool, token_in, token_out, amount),
        PoolDetail::Curve(pool) => prepare_curve(pool, token_in, token_out, amount),
        PoolDetail::Metric(pool) => prepare_metric(pool, token_in, token_out, amount),
        PoolDetail::Failed(_) => Err("pool read failed".into()),
    };
    match built {
        Ok(prepared) => Prep::Ready(prepared),
        Err(reason) => Prep::Skip(reason),
    }
}

pub(super) fn prepare_product(
    pool: &chain::detail::ConstantProduct,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    if pool.pair == Address::ZERO {
        return Err("pair".into());
    }
    if token_in == Address::ZERO || token_out == Address::ZERO {
        return Err("token".into());
    }
    let fee = product_fee_pips(pool.kind, pool.fee_pips);
    Ok(execution(
        Family::V2,
        encode_quote_v2(pool.pair, token_in, token_out, amount, fee),
        token_in,
    ))
}

pub(super) fn prepare_concentrated(
    pool: &chain::detail::Concentrated,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    if exceeds_positive_i256(amount) {
        return Err("amount exceeds int256".into());
    }
    let zero_for_one = zero_for_one(pool.token0, pool.token1, token_in, token_out)?;
    match pool.kind {
        UNI_V4 => {
            let fee = pool.fee_pips.ok_or_else(|| "fee unread".to_string())?;
            if !fits_i24(pool.tick_spacing) {
                return Err("tick spacing".into());
            }
            Ok(execution(
                Family::V4,
                encode_quote_v4(
                    pool.token0,
                    pool.token1,
                    fee,
                    pool.tick_spacing,
                    pool.hooks,
                    zero_for_one,
                    amount,
                ),
                token_in,
            ))
        }
        UNI_V3 | PANCAKE_V3 | SUSHI_V3 | V3_FORK | AERO => {
            if pool.home == Address::ZERO {
                return Err("pool".into());
            }
            if token_in == Address::ZERO || token_out == Address::ZERO {
                return Err("token".into());
            }
            let family = if pool.kind == AERO {
                Family::Aero
            } else {
                Family::V3
            };
            Ok(execution(
                family,
                encode_quote_v3(pool.home, token_in, token_out, zero_for_one, amount),
                token_in,
            ))
        }
        _ => Err("pool".into()),
    }
}

pub(super) fn prepare_metric(
    pool: &chain::detail::MetricPool,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    if pool.pool == Address::ZERO {
        return Err("pool".into());
    }
    let zero_for_one = zero_for_one(pool.token0, pool.token1, token_in, token_out)?;
    if amount > U256::from(i128::MAX as u128) {
        return Err("amount exceeds int128".into());
    }
    // zeroForOne uses no price limit. oneForZero uses `type(uint128).max`.
    let limit = if zero_for_one {
        U256::ZERO
    } else {
        U256::from(u128::MAX)
    };
    Ok(execution(
        Family::Metric,
        encode_quote_metric(pool.pool, token_in, token_out, zero_for_one, amount, limit),
        token_in,
    ))
}

pub(super) fn prepare_balancer(
    pool: &chain::detail::BalancerPool,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    if token_in == Address::ZERO || token_out == Address::ZERO {
        return Err("token".into());
    }
    if pool.vault == Address::ZERO {
        return Err("vault".into());
    }
    match pool.kind {
        BALANCER_V2 => Ok(execution(
            Family::Bal2,
            encode_quote_bal2(pool.vault, pool.id, token_in, token_out, amount),
            token_in,
        )),
        BALANCER_V3 => {
            if pool.pool == Address::ZERO {
                return Err("pool".into());
            }
            Ok(execution(
                Family::Bal3,
                encode_quote_bal3(pool.vault, pool.pool, token_in, token_out, amount),
                token_in,
            ))
        }
        _ => Err("vault".into()),
    }
}

pub(super) fn prepare_curve(
    pool: &chain::detail::CurvePool,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<Prepared, String> {
    let input = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_in)
        .ok_or("token")?;
    let output = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_out)
        .ok_or("token")?;
    if pool.pool == Address::ZERO {
        return Err("pool".into());
    }
    if token_in == Address::ZERO || token_out == Address::ZERO {
        return Err("native curve".into());
    }
    let preferred_uint = pool.coins_indexed_by_uint;
    let mut prepared = execution(
        Family::Curve,
        encode_quote_curve(
            pool.pool,
            token_in,
            token_out,
            input as u64,
            output as u64,
            amount,
            preferred_uint,
        ),
        token_in,
    );
    prepared.alt = Some(encode_quote_curve(
        pool.pool,
        token_in,
        token_out,
        input as u64,
        output as u64,
        amount,
        !preferred_uint,
    ));
    prepared.alt_method = Some("swap");
    Ok(prepared)
}

pub(super) fn execution(family: Family, data: Vec<u8>, token_in: Address) -> Prepared {
    Prepared {
        family,
        target: QUOTE_LENS,
        method: "swap",
        data,
        alt: None,
        alt_method: None,
        token_in,
        fund_eth: token_in == Address::ZERO,
    }
}

pub(super) fn zero_for_one(
    token0: Address,
    token1: Address,
    token_in: Address,
    token_out: Address,
) -> Result<bool, String> {
    if token_in == token0 && token_out == token1 {
        Ok(true)
    } else if token_in == token1 && token_out == token0 {
        Ok(false)
    } else {
        Err("token".into())
    }
}

pub(super) fn exceeds_positive_i256(amount: U256) -> bool {
    amount.bit(255)
}

pub(super) fn fits_i24(value: i32) -> bool {
    (-8_388_608..8_388_608).contains(&value)
}
