//! Latency of two roads that carry a demand of 2.
//!
//! The linear road's latency equals its flow, so its social cost is the
//! square of that flow. The other road's latency is the constant 2. Flow `f`
//! on the linear road costs `(2 - f) * 2 + f * f`. That cost is 3 only at
//! flow 1, where the linear marginal equals the constant latency. A cross
//! link of latency 0 raises the cost of the even split from 3 to 4.

use num_bigint::BigInt;

/// Demand carried by the two roads.
pub const DEMAND: i64 = 2;

/// Latency of the constant road.
pub const CONSTANT_LATENCY: i64 = 2;

pub fn linear_latency(flow: &BigInt) -> BigInt {
    flow.clone()
}

pub fn linear_social(flow: &BigInt) -> BigInt {
    flow * flow
}

pub fn linear_marginal_social(flow: &BigInt) -> BigInt {
    BigInt::from(2) * flow
}

/// `(DEMAND - flow) * CONSTANT_LATENCY + flow * flow`.
pub fn pigou_cost(flow: &BigInt) -> BigInt {
    (BigInt::from(DEMAND) - flow) * CONSTANT_LATENCY + linear_social(flow)
}

/// Social cost added by a step `step` on the linear road.
pub fn linear_social_gap(flow: &BigInt, step: &BigInt) -> BigInt {
    linear_marginal_social(flow) * step + step * step
}

/// Latency of the path that uses the linear road and then the constant road.
pub fn split_path(flow: &BigInt) -> BigInt {
    linear_latency(flow) + CONSTANT_LATENCY
}

/// Latency of the path that sends both units across a zero-latency cross link.
pub fn cross_path() -> BigInt {
    linear_latency(&BigInt::from(DEMAND)) + linear_latency(&BigInt::from(DEMAND))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(value: i64) -> BigInt {
        BigInt::from(value)
    }

    #[test]
    fn the_minimum_equalizes_marginals_and_the_corners_cost_more() {
        for flow in -6..=8 {
            for step in -4..=4 {
                let grown = &z(flow) + z(step);
                assert_eq!(
                    linear_social(&grown) - linear_social(&z(flow)),
                    linear_social_gap(&z(flow), &z(step))
                );
            }
            let cost = pigou_cost(&z(flow));
            assert_eq!(cost, z(4) - z(2) * z(flow) + z(flow) * z(flow));
            assert_eq!(&cost - z(3), (&z(flow) - z(1)) * (&z(flow) - z(1)));
            assert!(cost >= z(3));
            assert_eq!(cost == z(3), flow == 1);
        }
        assert_eq!(pigou_cost(&z(0)), z(4));
        assert_eq!(pigou_cost(&z(1)), z(3));
        assert_eq!(pigou_cost(&z(2)), z(4));
        assert_eq!(pigou_cost(&z(2)) * z(3), pigou_cost(&z(1)) * z(4));
        assert_eq!(linear_latency(&z(DEMAND)), z(CONSTANT_LATENCY));
        assert!(linear_latency(&z(0)) < z(CONSTANT_LATENCY));
        assert_eq!(linear_marginal_social(&z(1)), z(CONSTANT_LATENCY));
        assert_ne!(linear_marginal_social(&z(DEMAND)), z(CONSTANT_LATENCY));
        let mut splits = 0;
        for linear in 1..DEMAND {
            let constant = DEMAND - linear;
            if constant > 0 {
                splits += 1;
                assert_ne!(linear_latency(&z(linear)), z(CONSTANT_LATENCY));
            }
        }
        assert!(splits > 0);
    }

    #[test]
    fn a_zero_latency_cross_link_raises_the_even_split() {
        assert_eq!(split_path(&z(1)), z(3));
        assert_eq!(z(CONSTANT_LATENCY) + linear_latency(&z(1)), z(3));
        assert_eq!(cross_path(), z(4));
        assert_eq!(linear_latency(&z(DEMAND)) + z(CONSTANT_LATENCY), z(4));
        assert!(split_path(&z(1)) < cross_path());
    }
}
