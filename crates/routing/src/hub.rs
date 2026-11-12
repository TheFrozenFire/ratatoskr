//! Hub labels.
//!
//! A label is the directed quote from a token to one hub. The opposite
//! direction is a different label. The same hub on both labels is
//! [`hop_num`] over [`hop_den`]. Two hubs insert the quote between them,
//! [`two_hub_num`] over [`two_hub_den`]. The direct quote stays in the
//! candidate set. Each candidate enters as its numerator times the other
//! denominator, and [`label_choice`] is the maximum of those keys.

use num_bigint::BigInt;

use crate::basket::{product_den, product_num};

/// One constant-product leg: input reserve, output reserve, and multiplier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub input: BigInt,
    pub output: BigInt,
    pub multiplier: BigInt,
}

/// Two-leg numerator. The second leg takes the first leg's quote as its input.
pub fn hop_num(first: &Leg, second: &Leg, dx: &BigInt) -> BigInt {
    &second.output * &second.multiplier * product_num(&first.output, &first.multiplier, dx)
}

/// Two-leg denominator.
pub fn hop_den(first: &Leg, second: &Leg, dx: &BigInt) -> BigInt {
    &second.input * product_den(&first.input, &first.multiplier, dx)
        + &second.multiplier * product_num(&first.output, &first.multiplier, dx)
}

/// Three-leg numerator. The outer leg takes the two-leg quote as its input.
pub fn two_hub_num(first: &Leg, second: &Leg, outer: &Leg, dx: &BigInt) -> BigInt {
    &outer.output * &outer.multiplier * hop_num(first, second, dx)
}

/// Three-leg denominator.
pub fn two_hub_den(first: &Leg, second: &Leg, outer: &Leg, dx: &BigInt) -> BigInt {
    &outer.input * hop_den(first, second, dx) + &outer.multiplier * hop_num(first, second, dx)
}

/// Numerator times the other candidate's denominator.
pub fn cross_key(numer: &BigInt, other_den: &BigInt) -> BigInt {
    numer * other_den
}

/// Maximum of the three cleared cross keys. The maximum is one of the keys.
pub fn label_choice(direct: &BigInt, shared: &BigInt, across: &BigInt) -> BigInt {
    std::cmp::max(direct, std::cmp::max(shared, across)).clone()
}

/// The direct key is the maximum when its cross product is at least the other.
pub fn larger_cross_wins(dnum: &BigInt, dden: &BigInt, vnum: &BigInt, vden: &BigInt) -> bool {
    vnum * dden <= dnum * vden
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(value: i64) -> BigInt {
        BigInt::from(value)
    }

    fn leg(input: i64, output: i64, multiplier: i64) -> Leg {
        Leg {
            input: z(input),
            output: z(output),
            multiplier: z(multiplier),
        }
    }

    #[test]
    fn two_hub_zero_input_quotes_zero() {
        let leg = leg(1, 1, 1);
        assert_eq!(two_hub_num(&leg, &leg, &leg, &z(0)), z(0));
    }

    #[test]
    fn two_hub_denominator_stays_positive() {
        let leg = leg(1, 0, 0);
        assert_eq!(two_hub_den(&leg, &leg, &leg, &z(0)), z(1));
    }

    #[test]
    fn two_hub_increase_factors() {
        let one = leg(1, 1, 1);
        let gap = two_hub_num(&one, &one, &one, &z(3)) * two_hub_den(&one, &one, &one, &z(1))
            - two_hub_num(&one, &one, &one, &z(1)) * two_hub_den(&one, &one, &one, &z(3));
        assert_eq!(gap, z(2));
        let flat_outer = leg(1, 1, 0);
        let flat = two_hub_num(&one, &one, &flat_outer, &z(3))
            * two_hub_den(&one, &one, &flat_outer, &z(1))
            - two_hub_num(&one, &one, &flat_outer, &z(1))
                * two_hub_den(&one, &one, &flat_outer, &z(3));
        assert_eq!(flat, z(0));
    }

    #[test]
    fn label_choice_is_one_key_and_ignores_order() {
        let direct = z(3);
        let shared = z(9);
        let across = z(4);
        let choice = label_choice(&direct, &shared, &across);
        assert_eq!(choice, shared);
        assert_eq!(choice, label_choice(&across, &direct, &shared));
        assert!(direct <= choice && shared <= choice && across <= choice);
        assert!(label_choice(&direct, &shared, &z(1)) <= choice);
    }

    #[test]
    fn direction_quotes_differ() {
        let forward = cross_key(
            &product_num(&z(100), &z(1), &z(10)),
            &product_den(&z(100), &z(1), &z(10)),
        );
        let back = cross_key(
            &product_num(&z(10), &z(1), &z(10)),
            &product_den(&z(10), &z(1), &z(10)),
        );
        assert_eq!(forward, z(110_000));
        assert_eq!(back, z(2_000));
        assert_ne!(forward, back);
        assert!(larger_cross_wins(
            &product_num(&z(100), &z(1), &z(10)),
            &product_den(&z(100), &z(1), &z(10)),
            &product_num(&z(10), &z(1), &z(10)),
            &product_den(&z(10), &z(1), &z(10)),
        ));
    }

    #[test]
    fn hub_winner_depends_on_size() {
        let small_second = cross_key(
            &product_num(&z(20), &z(1), &z(10)),
            &product_den(&z(1000), &z(1), &z(10)),
        );
        let small_first = cross_key(
            &product_num(&z(100), &z(1), &z(10)),
            &product_den(&z(10), &z(1), &z(10)),
        );
        assert_eq!(small_second, z(202_000));
        assert_eq!(small_first, z(20_000));
        assert!(small_second > small_first);
        let large_first = cross_key(
            &product_num(&z(100), &z(1), &z(1000)),
            &product_den(&z(10), &z(1), &z(1000)),
        );
        let large_second = cross_key(
            &product_num(&z(20), &z(1), &z(1000)),
            &product_den(&z(1000), &z(1), &z(1000)),
        );
        assert_eq!(large_first, z(101_000_000));
        assert_eq!(large_second, z(40_000_000));
        assert!(large_first > large_second);
    }

    #[test]
    fn hub_order_differs() {
        let steep_num = two_hub_num(&leg(8, 8, 1), &leg(0, 16, 1), &leg(0, 8, 1), &z(8));
        let steep_den = two_hub_den(&leg(8, 8, 1), &leg(4, 16, 1), &leg(8, 0, 1), &z(8));
        let shallow_num = two_hub_num(&leg(8, 8, 1), &leg(0, 4, 1), &leg(0, 8, 1), &z(8));
        let shallow_den = two_hub_den(&leg(8, 8, 1), &leg(16, 4, 1), &leg(8, 0, 1), &z(8));
        assert_eq!(steep_num, z(8192));
        assert_eq!(steep_den, z(2048));
        assert_eq!(shallow_num, z(2048));
        assert_eq!(shallow_den, z(2816));
        assert_ne!(
            cross_key(&steep_num, &shallow_den),
            cross_key(&shallow_num, &steep_den)
        );
    }

    #[test]
    fn hub_index_misses_an_outside_bridge() {
        let via_num = hop_num(&leg(0, 20, 1), &leg(0, 20, 1), &z(10));
        let via_den = hop_den(&leg(50, 20, 1), &leg(50, 0, 1), &z(10));
        let direct_num = product_num(&z(10), &z(1), &z(10));
        let direct_den = product_den(&z(100), &z(1), &z(10));
        let bridge_num = product_num(&z(50), &z(1), &z(10));
        let bridge_den = product_den(&z(10), &z(1), &z(10));
        assert_eq!(via_num, z(4000));
        assert_eq!(via_den, z(3200));
        assert_eq!(direct_num, z(100));
        assert_eq!(direct_den, z(110));
        assert_eq!(bridge_num, z(500));
        assert_eq!(bridge_den, z(20));
        assert!(cross_key(&via_num, &direct_den) > cross_key(&direct_num, &via_den));
        assert!(cross_key(&bridge_num, &via_den) > cross_key(&via_num, &bridge_den));
        assert!(cross_key(&bridge_num, &direct_den) > cross_key(&direct_num, &bridge_den));
    }
}
