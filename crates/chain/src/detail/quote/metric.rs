use super::super::types::*;
use super::mul_div::*;
use alloy_primitives::{Address, U256};

pub(super) fn quote_metric(
    pool: &MetricPool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    if pool.flags & FLAG_NO_REFERENCE != 0 || pool.reference_x64.is_zero() {
        return Err("reference price".into());
    }
    if pool.token0.is_zero() || pool.token1.is_zero() {
        return Err("token".into());
    }
    let bin = pool.bins.iter().find(|bin| bin.index == pool.bin_index);
    let price = pool.price_at(bin.map(|bin| bin.length_e6).unwrap_or(0))?;
    let selling_token0 = if token_in == pool.token0 && token_out == pool.token1 {
        true
    } else if token_in == pool.token1 && token_out == pool.token0 {
        false
    } else {
        return Err("token".into());
    };
    let (scale_out, scaled_out, scale_in) = if selling_token0 {
        (
            pool.scale1,
            bin.map(|bin| bin.token1_scaled).unwrap_or(0),
            pool.scale0,
        )
    } else {
        (
            pool.scale0,
            bin.map(|bin| bin.token0_scaled).unwrap_or(0),
            pool.scale1,
        )
    };
    if scale_in.is_zero() || scale_out.is_zero() {
        return Err("scale".into());
    }
    // An empty bin has no inventory. A zero scaled balance pays nothing.
    if scaled_out == 0 {
        return Ok(Quote {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            exhausted: true,
        });
    }
    // `price` is whole token1 per whole token0. `scale` is `10^(D - decimals)`.
    let unscaled = if selling_token0 {
        mul_div(amount_in, price, U256::from(1u8) << 64)?
    } else if price.is_zero() {
        return Err("price".into());
    } else {
        mul_div(amount_in, U256::from(1u8) << 64, price)?
    };
    let amount_out = mul_div(unscaled, scale_in, scale_out)?;
    let raw_available = Some(U256::from(scaled_out) / scale_out);
    if let Some(available) = raw_available {
        if amount_out > available {
            return Ok(Quote {
                amount_in,
                amount_out: available,
                fee_paid: U256::ZERO,
                exhausted: true,
            });
        }
    }
    Ok(Quote {
        amount_in,
        amount_out,
        fee_paid: U256::ZERO,
        exhausted: false,
    })
}

impl MetricPool {
    pub fn marginal_price_x64(&self) -> Result<U256, String> {
        let length = self
            .bins
            .iter()
            .find(|bin| bin.index == self.bin_index)
            .map(|bin| bin.length_e6)
            .unwrap_or(0);
        self.price_at(length)
    }

    fn price_at(&self, length_e6: u16) -> Result<U256, String> {
        if self.reference_x64.is_zero() {
            return Err("reference price".into());
        }
        let lower = distance_price(self.reference_x64, self.distance_e6)?;
        let upper = distance_price(
            self.reference_x64,
            self.distance_e6.saturating_add(i32::from(length_e6)),
        )?;
        if upper < lower {
            return Err("bin price".into());
        }
        let span = upper - lower;
        let max_pos = (U256::from(1u8) << 104) - U256::from(1u8);
        let pos = U256::from(self.position);
        if span.is_zero() || pos.is_zero() {
            return Ok(lower);
        }
        Ok(lower + mul_div(span, pos, max_pos)?)
    }
}

pub(super) fn distance_price(reference: U256, distance_e6: i32) -> Result<U256, String> {
    let million = U256::from(1_000_000u64);
    if distance_e6 >= 0 {
        mul_div(reference, million + U256::from(distance_e6 as u32), million)
    } else {
        let distance = distance_e6.unsigned_abs();
        if U256::from(distance) >= million {
            return Err("distance".into());
        }
        mul_div(reference, million - U256::from(distance), million)
    }
}
