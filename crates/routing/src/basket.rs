//! Basket count.
//!
//! Holding `h` supports count `q` at weight `r` when `q * r <= h`. For a
//! positive weight the last count is the floor quotient `h / r`. Two outputs
//! support the minimum of their last counts. A negative holding supports no
//! nonnegative count.

use num_bigint::BigInt;

use crate::floor_div;

/// `count * weight <= holding`.
pub fn supported(holding: &BigInt, weight: &BigInt, count: &BigInt) -> bool {
    count * weight <= *holding
}

/// Floor quotient `holding / weight`. The weight is positive.
pub fn last_count(holding: &BigInt, weight: &BigInt) -> Option<BigInt> {
    floor_div(holding, weight)
}

/// Minimum of the last counts. Each weight is positive, and there is a leg.
pub fn basket_count(legs: &[(BigInt, BigInt)]) -> Option<BigInt> {
    let mut count: Option<BigInt> = None;
    for (holding, weight) in legs {
        let leg = last_count(holding, weight)?;
        count = Some(match count {
            Some(current) if current < leg => current,
            _ => leg,
        });
    }
    count
}

/// The open count, net of its cost, is still larger than the closed count.
pub fn open_count_wins(open_count: &BigInt, closed_count: &BigInt, cost: &BigInt) -> bool {
    open_count - cost > *closed_count
}

/// Constant-product numerator `output * multiplier * tender`.
pub fn product_num(output: &BigInt, multiplier: &BigInt, tender: &BigInt) -> BigInt {
    output * multiplier * tender
}

/// Constant-product denominator `input + multiplier * tender`.
pub fn product_den(input: &BigInt, multiplier: &BigInt, tender: &BigInt) -> BigInt {
    input + multiplier * tender
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(value: i64) -> BigInt {
        BigInt::from(value)
    }

    #[test]
    fn the_last_count_is_the_floor_quotient() {
        for holding in -6..=8 {
            for weight in 1..=5 {
                let count = last_count(&z(holding), &z(weight)).expect("positive weight");
                assert!(supported(&z(holding), &z(weight), &count));
                assert!(!supported(&z(holding), &z(weight), &(&count + 1)));
                assert_eq!(count < z(0), holding < 0);
            }
        }
        assert_eq!(last_count(&z(-1), &z(2)), Some(z(-1)));
        assert!(last_count(&z(4), &z(0)).is_none());
    }

    #[test]
    fn a_negative_holding_supports_no_nonnegative_count() {
        for holding in -4..0 {
            for weight in 1..=4 {
                for count in 0..=6 {
                    assert!(!supported(&z(holding), &z(weight), &z(count)));
                }
            }
        }
        assert!(supported(&z(0), &z(3), &z(0)));
    }

    #[test]
    fn two_outputs_support_the_minimum_of_their_last_counts() {
        for left in -4..=6 {
            for right in -4..=6 {
                for left_weight in 1..=4 {
                    for right_weight in 1..=4 {
                        let legs = [(z(left), z(left_weight)), (z(right), z(right_weight))];
                        let count = basket_count(&legs).expect("weights");
                        assert!(supported(&z(left), &z(left_weight), &count));
                        assert!(supported(&z(right), &z(right_weight), &count));
                        let next = &count + 1;
                        assert!(
                            !supported(&z(left), &z(left_weight), &next)
                                || !supported(&z(right), &z(right_weight), &next)
                        );
                    }
                }
            }
        }
        assert!(basket_count(&[]).is_none());
    }

    #[test]
    fn raising_a_holding_does_not_lower_the_count() {
        for holding in -3..=5 {
            for raised in holding..=6 {
                for weight in 1..=4 {
                    let low = last_count(&z(holding), &z(weight)).expect("weight");
                    let high = last_count(&z(raised), &z(weight)).expect("weight");
                    assert!(low <= high);
                    let legs = [(z(holding), z(weight)), (z(1), z(1))];
                    let raised_legs = [(z(raised), z(weight)), (z(2), z(1))];
                    let before = basket_count(&legs).expect("legs");
                    let after = basket_count(&raised_legs).expect("legs");
                    assert!(before <= after);
                }
            }
        }
    }

    #[test]
    fn a_count_past_the_last_fails_and_a_slack_leg_still_has_room() {
        for holding in -2..=8 {
            for weight in 1..=4 {
                let last = last_count(&z(holding), &z(weight)).expect("weight");
                for count in -2..=4 {
                    if z(count) < last {
                        assert!(z(count) * z(weight) < z(holding));
                    }
                }
                for count in 0..=5 {
                    if z(count) > last {
                        assert!(!supported(&z(holding), &z(weight), &z(count)));
                    }
                }
            }
        }
    }

    #[test]
    fn an_even_split_of_two_identical_legs_supports_a_higher_count() {
        let multiplier = z(1);
        let even_num = product_num(&z(6), &multiplier, &z(3));
        let even_den = product_den(&z(6), &multiplier, &z(3));
        let skew_large_num = product_num(&z(6), &multiplier, &z(4));
        let skew_large_den = product_den(&z(6), &multiplier, &z(4));
        let skew_small_num = product_num(&z(6), &multiplier, &z(2));
        let skew_small_den = product_den(&z(6), &multiplier, &z(2));
        let even = basket_count(&[(even_num.clone(), even_den.clone()), (even_num, even_den)])
            .expect("even");
        let uneven = basket_count(&[
            (skew_large_num, skew_large_den),
            (skew_small_num, skew_small_den),
        ])
        .expect("uneven");
        assert_eq!(even, z(2));
        assert_eq!(uneven, z(1));
    }

    #[test]
    fn the_open_count_wins_only_when_it_stays_larger_after_its_cost() {
        for open in -5..=8 {
            for closed in -5..=8 {
                for cost in -4..=6 {
                    let net_smaller = z(open) < z(closed) + z(cost);
                    let compared = &z(open) - z(cost) < z(closed);
                    assert_eq!(net_smaller, compared);
                    assert_eq!(
                        open_count_wins(&z(open), &z(closed), &z(cost)),
                        z(open) - z(cost) > z(closed)
                    );
                }
            }
        }
        assert!(open_count_wins(&z(5), &z(3), &z(1)));
        assert!(!open_count_wins(&z(4), &z(3), &z(1)));
    }
}
