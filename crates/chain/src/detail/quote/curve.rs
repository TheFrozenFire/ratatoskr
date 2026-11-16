use super::super::types::*;
use super::mul_div::*;
use alloy_primitives::{Address, U256};
use fitter::{crypto2_out, crypto3_out};
use num_bigint::{BigInt, Sign};

pub(super) fn quote_curve(
    pool: &CurvePool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
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
    if input == output {
        return Err("token".into());
    }
    if pool.flags & FLAG_CRYPTO != 0 || !pool.gamma.is_zero() {
        return quote_crypto(pool, input, output, amount_in);
    }
    let fee = pool.fee.ok_or_else(|| "fee unread".to_string())?;
    let balances = pool
        .coins
        .iter()
        .map(xp_balance)
        .collect::<Result<Vec<_>, _>>()?;
    let dx = xp_amount(&pool.coins[input], amount_in)?;
    stable_out(
        &balances,
        pool.amp,
        input,
        output,
        dx,
        fee,
        U256::from(10_000_000_000u64),
    )
    .and_then(|quote| {
        let amount_out = raw_amount(&pool.coins[output], quote.amount_out)?;
        Ok(Quote {
            amount_in,
            amount_out,
            fee_paid: quote.fee_paid,
            exhausted: quote.exhausted,
        })
    })
}

pub(super) fn quote_crypto(
    pool: &CurvePool,
    input: usize,
    output: usize,
    amount_in: U256,
) -> Result<Quote, String> {
    let n = pool.coins.len();
    if n != 2 && n != 3 {
        return Err("coins".into());
    }
    let amp_limit = U256::from(1_000_000_000_000u64);
    if pool.amp.is_zero() || pool.amp > amp_limit {
        return Err("amp".into());
    }
    if pool.coins.iter().any(|coin| coin.precision.is_zero()) {
        return Err("precision".into());
    }
    let balances = pool
        .coins
        .iter()
        .map(|coin| bigint_from_u256(coin.balance))
        .collect::<Vec<_>>();
    let precs = pool
        .coins
        .iter()
        .map(|coin| bigint_from_u256(coin.precision))
        .collect::<Vec<_>>();
    let dx = bigint_from_u256(amount_in);
    let amp = bigint_from_u256(pool.amp);
    let gamma = bigint_from_u256(pool.gamma);
    let d = bigint_from_u256(pool.d);
    let mid = bigint_from_u256(pool.mid_fee);
    let out_fee = bigint_from_u256(pool.out_fee);
    let fee_gamma = bigint_from_u256(pool.fee_gamma);
    let amount = if n == 3 {
        let scales = vec![
            bigint_from_u256(pool.coins[0].price_scale),
            bigint_from_u256(pool.coins[1].price_scale),
        ];
        crypto3_out(
            &balances,
            &precs,
            &scales,
            &amp,
            &gamma,
            &d,
            input as i64,
            output as i64,
            &dx,
            &mid,
            &out_fee,
            &fee_gamma,
        )
    } else {
        crypto2_out(
            &balances,
            &precs,
            &bigint_from_u256(pool.coins[0].price_scale),
            &amp,
            &gamma,
            &d,
            input as i64,
            output as i64,
            &dx,
            &mid,
            &out_fee,
            &fee_gamma,
        )
    };
    let amount_out = amount.ok_or_else(|| "quote".to_string())?;
    Ok(Quote {
        amount_in,
        amount_out: u256_from_bigint(&amount_out)?,
        fee_paid: U256::ZERO,
        exhausted: false,
    })
}

fn bigint_from_u256(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}

fn u256_from_bigint(value: &BigInt) -> Result<U256, String> {
    if value.sign() == Sign::Minus {
        return Err("quote".into());
    }
    let bytes = value.to_bytes_be().1;
    if bytes.len() > 32 {
        return Err("quote".into());
    }
    let mut wide = [0u8; 32];
    wide[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(U256::from_be_bytes(wide))
}

pub(super) fn stable_out(
    balances: &[U256],
    amp: U256,
    input: usize,
    output: usize,
    amount_in: U256,
    fee: U256,
    fee_denom: U256,
) -> Result<Quote, String> {
    if amp.is_zero() || fee > fee_denom {
        return Err("amp".into());
    }
    let d = get_d(balances, amp)?;
    let x = balances[input]
        .checked_add(amount_in)
        .ok_or_else(|| "amount".to_string())?;
    let y = get_y(input, output, x, balances, amp, d)?;
    let mut dy = balances[output]
        .checked_sub(y)
        .ok_or_else(|| "y".to_string())?
        .checked_sub(U256::from(1u8))
        .ok_or_else(|| "y".to_string())?;
    let uncharged = dy;
    if !fee.is_zero() {
        dy = mul_div(dy, fee_denom - fee, fee_denom)?;
    }
    Ok(Quote {
        amount_in,
        amount_out: dy,
        fee_paid: uncharged - dy,
        exhausted: false,
    })
}

pub(super) fn get_d(xp: &[U256], amp: U256) -> Result<U256, String> {
    let n = U256::from(xp.len());
    let sum = xp
        .iter()
        .try_fold(U256::ZERO, |acc, value| acc.checked_add(*value))
        .ok_or("sum")?;
    if sum.is_zero() {
        return Ok(U256::ZERO);
    }
    let ann = amp.checked_mul(n).ok_or("amp")?;
    let mut d = sum;
    for _ in 0..255 {
        let mut d_p = d;
        for value in xp {
            if value.is_zero() {
                return Err("empty coin".into());
            }
            d_p = mul_div(d_p, d, value.checked_mul(n).ok_or("n")?)?;
        }
        let prev = d;
        let numerator = ann
            .checked_mul(sum)
            .ok_or("ann")?
            .checked_add(d_p.checked_mul(n).ok_or("dp")?)
            .ok_or("num")?;
        let denominator = (ann - U256::from(1u8))
            .checked_mul(d)
            .ok_or("den")?
            .checked_add((n + U256::from(1u8)).checked_mul(d_p).ok_or("den")?)
            .ok_or("den")?;
        d = mul_div(numerator, d, denominator)?;
        let diff = if d > prev { d - prev } else { prev - d };
        if diff <= U256::from(1u8) {
            return Ok(d);
        }
    }
    Err("invariant".into())
}

/// Positive root of `y² + (b − d) y − c = 0`, the fixed point of one Newton step.
/// The iteration still starts from this value and stops on the same `|Δ| ≤ 1` test.
fn y_seed(b: U256, d: U256, c: U256) -> U256 {
    let abs_b = if b >= d { b - d } else { d - b };
    let Some(square) = abs_b.checked_mul(abs_b) else {
        return d;
    };
    let Some(four_c) = c.checked_mul(U256::from(4u8)) else {
        return d;
    };
    let Some(disc) = square.checked_add(four_c) else {
        return d;
    };
    let root = disc.root(2);
    let Some(sum) = (if b >= d {
        root.checked_sub(abs_b)
    } else {
        root.checked_add(abs_b)
    }) else {
        return d;
    };
    let y = sum / U256::from(2u8);
    if y.is_zero() {
        d
    } else {
        y
    }
}

pub(super) fn get_y(
    i: usize,
    j: usize,
    x: U256,
    xp: &[U256],
    amp: U256,
    d: U256,
) -> Result<U256, String> {
    let n = U256::from(xp.len());
    let ann = amp.checked_mul(n).ok_or("amp")?;
    let mut c = d;
    let mut s = U256::ZERO;
    for (k, balance) in xp.iter().enumerate() {
        if k == j {
            continue;
        }
        let x_k = if k == i { x } else { *balance };
        s = s.checked_add(x_k).ok_or("s")?;
        c = mul_div(c, d, x_k.checked_mul(n).ok_or("xn")?)?;
    }
    c = mul_div(c, d, ann.checked_mul(n).ok_or("ann")?)?;
    let b = s.checked_add(d / ann).ok_or("b")?;
    let mut y = y_seed(b, d, c);
    for _ in 0..255 {
        let prev = y;
        let numerator = y.checked_mul(y).ok_or("y")?.checked_add(c).ok_or("y")?;
        let denominator = (y * U256::from(2u8))
            .checked_add(b)
            .ok_or("y")?
            .checked_sub(d)
            .ok_or("y")?;
        y = numerator / denominator;
        let diff = if y > prev { y - prev } else { prev - y };
        if diff <= U256::from(1u8) {
            return Ok(y);
        }
    }
    Err("y".into())
}

pub(super) fn xp_balance(coin: &CurveCoin) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(coin.balance)
    } else {
        mul_div(coin.balance, coin.rate, e18())
    }
}

pub(super) fn xp_amount(coin: &CurveCoin, amount: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount)
    } else {
        mul_div(amount, coin.rate, e18())
    }
}

pub(super) fn raw_amount(coin: &CurveCoin, amount_xp: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount_xp)
    } else {
        mul_div(amount_xp, e18(), coin.rate)
    }
}
