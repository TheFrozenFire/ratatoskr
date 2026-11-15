//! Local quote of one ingested pool snapshot.

mod balancer;
mod concentrated;
mod curve;
mod metric;
mod mul_div;
mod product;

use super::types::{PoolDetail, Quote, FLAG_FAILED};
use alloy_primitives::{Address, U256};
use balancer::quote_balancer;
use concentrated::quote_concentrated;
use curve::quote_curve;
use metric::quote_metric;
use product::quote_product;

impl PoolDetail {
    pub fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<Quote, String> {
        if amount_in.is_zero() {
            return Ok(Quote {
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                fee_paid: U256::ZERO,
                exhausted: false,
            });
        }
        if self.flags() & FLAG_FAILED != 0 {
            return Err("pool read failed".into());
        }
        match self {
            Self::ConstantProduct(pool) => quote_product(pool, token_in, token_out, amount_in),
            Self::Concentrated(pool) => quote_concentrated(pool, token_in, token_out, amount_in),
            Self::Balancer(pool) => quote_balancer(pool, token_in, token_out, amount_in),
            Self::Curve(pool) => quote_curve(pool, token_in, token_out, amount_in),
            Self::Metric(pool) => quote_metric(pool, token_in, token_out, amount_in),
            Self::Failed(_) => Err("pool read failed".into()),
        }
    }
}

pub use balancer::v3_raw_balance;
pub use concentrated::concentrated_ladder;
pub use product::product_fee_pips;

#[cfg(test)]
pub(crate) use concentrated::{quote_ladder, sqrt_at_tick};
#[cfg(test)]
pub(crate) use mul_div::e18;
