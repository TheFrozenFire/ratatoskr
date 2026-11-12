//! Pair conduit, basket count, the trading-set ledger, and hub labels.
//!
//! A slope segment is a positive input width and a nonnegative output on one
//! action. Input `t` on that segment pays `(output * t) / width`, which is
//! the output when `t` is the width. The merge spends the next input on the
//! steepest remaining segment.
//!
//! A basket count is the largest integer every fixed-weight output still
//! supports. A trading set quotes one direction from its original reserves:
//! same-direction tenders are one submission.

pub mod basket;
pub mod depletion;
pub mod hub;
pub mod latency;

use num_bigint::BigInt;
use num_traits::Zero;
use std::cmp::Ordering;

/// Floor quotient for a positive divisor. The remainder is nonnegative.
pub(crate) fn floor_div(numer: &BigInt, denom: &BigInt) -> Option<BigInt> {
    if denom <= &BigInt::from(0) {
        return None;
    }
    let mut quot = numer / denom;
    let rem = numer % denom;
    if numer < &BigInt::from(0) && !rem.is_zero() {
        quot -= 1;
    }
    Some(quot)
}

/// Least integer quotient that meets a nonnegative numerator.
pub(crate) fn ceil_div(numer: &BigInt, denom: &BigInt) -> Option<BigInt> {
    if denom <= &BigInt::from(0) || numer < &BigInt::from(0) {
        return None;
    }
    if numer.is_zero() {
        return Some(BigInt::from(0));
    }
    Some((numer + denom - 1) / denom)
}

/// Pool a slope segment spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Action {
    pub kind: u8,
    pub pool: [u8; 32],
}

/// One constant marginal on one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlopeSegment {
    pub width: BigInt,
    pub output: BigInt,
    pub action: Action,
}

/// Steeper first. Equal slopes of different actions stay in action order.
fn slope_order(left: &SlopeSegment, right: &SlopeSegment) -> Ordering {
    let left_cross = &left.output * &right.width;
    let right_cross = &right.output * &left.width;
    match left_cross.cmp(&right_cross) {
        Ordering::Greater => Ordering::Less,
        Ordering::Less => Ordering::Greater,
        Ordering::Equal => left.action.cmp(&right.action),
    }
}

fn same_slope(left: &SlopeSegment, right: &SlopeSegment) -> bool {
    &left.output * &right.width == &right.output * &left.width
}

/// Knots are cumulative `(input, output)`, including the origin when the
/// caller has one. A later secant that rises is lowered until its slope no
/// longer rises. A zero-output segment is omitted.
pub fn segments_from_knots(knots: &[(BigInt, BigInt)], action: Action) -> Vec<SlopeSegment> {
    let mut points = vec![(BigInt::from(0), BigInt::from(0))];
    for (input, output) in knots {
        let Some(last) = points.last() else {
            continue;
        };
        if input <= &last.0 {
            continue;
        }
        let output = if output < &last.1 {
            last.1.clone()
        } else {
            output.clone()
        };
        points.push((input.clone(), output));
    }
    let mut previous: Option<(BigInt, BigInt)> = None;
    let mut out = Vec::new();
    for index in 1..points.len() {
        let width = &points[index].0 - &points[index - 1].0;
        if width <= BigInt::from(0) {
            continue;
        }
        let mut output = &points[index].1 - &points[index - 1].1;
        if output < BigInt::from(0) {
            output = BigInt::from(0);
        }
        if let Some((previous_width, previous_output)) = &previous {
            if &output * previous_width > previous_output * &width {
                output = (previous_output * &width) / previous_width;
            }
        }
        points[index].1 = &points[index - 1].1 + &output;
        previous = Some((width.clone(), output.clone()));
        if output.is_zero() {
            continue;
        }
        out.push(SlopeSegment {
            width,
            output,
            action,
        });
    }
    out
}

/// Fill every member steepest first. Equal slopes of one action are joined.
/// Equal slopes of two actions stay separate.
pub fn merge(members: &[Vec<SlopeSegment>]) -> Vec<SlopeSegment> {
    let mut parts: Vec<SlopeSegment> = members
        .iter()
        .flat_map(|member| member.iter().cloned())
        .filter(|segment| segment.width > BigInt::from(0) && segment.output > BigInt::from(0))
        .collect();
    parts.sort_by(slope_order);
    let mut merged: Vec<SlopeSegment> = Vec::new();
    for segment in parts {
        if let Some(last) = merged.last_mut() {
            if last.action == segment.action && same_slope(last, &segment) {
                last.width += &segment.width;
                last.output += &segment.output;
                continue;
            }
        }
        merged.push(segment);
    }
    merged
}

/// Chord pay along the sorted segments. Input past the last output adds nothing.
pub fn quote(segments: &[SlopeSegment], input: &BigInt) -> BigInt {
    if input <= &BigInt::from(0) {
        return BigInt::from(0);
    }
    let mut left = input.clone();
    let mut paid = BigInt::from(0);
    for segment in segments {
        if left.is_zero() || segment.width.is_zero() {
            break;
        }
        let taken = if left < segment.width {
            left.clone()
        } else {
            segment.width.clone()
        };
        paid += (&segment.output * &taken) / &segment.width;
        left -= taken;
    }
    paid
}

/// Least input whose forward quote meets `target`. A target above the tail is
/// not a quote.
pub fn backward(segments: &[SlopeSegment], target: &BigInt) -> Option<BigInt> {
    if target <= &BigInt::from(0) {
        return Some(BigInt::from(0));
    }
    let mut need = target.clone();
    let mut spent = BigInt::from(0);
    for segment in segments {
        if segment.output.is_zero() || segment.width.is_zero() {
            continue;
        }
        if need <= segment.output {
            let numerator = &need * &segment.width;
            let taken = (&numerator + &segment.output - 1) / &segment.output;
            return Some(spent + taken);
        }
        need -= &segment.output;
        spent += &segment.width;
    }
    None
}

#[cfg(test)]
fn segment(width: i64, output: i64, kind: u8, pool_byte: u8) -> SlopeSegment {
    let mut pool = [0_u8; 32];
    pool[31] = pool_byte;
    SlopeSegment {
        width: BigInt::from(width),
        output: BigInt::from(output),
        action: Action { kind, pool },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steeper_segment_is_filled_first() {
        let members = [vec![segment(2, 10, 1, 1), segment(4, 4, 1, 2)]];
        let merged = merge(&members);
        assert_eq!(merged[0].output, BigInt::from(10));
        let input = BigInt::from(6);
        let best = quote(&merged, &input);
        for left in 0..=6 {
            let right = 6 - left;
            let pay = quote(&[segment(2, 10, 1, 1)], &BigInt::from(left))
                + quote(&[segment(4, 4, 1, 2)], &BigInt::from(right));
            assert!(pay <= best, "{left} pays {pay} above {best}");
        }
    }

    #[test]
    fn a_flat_curve_against_a_rising_line_does_not_kink() {
        let flat = vec![segment(1, 10, 1, 1)];
        let line = vec![segment(3, 12, 1, 2)];
        let merged = merge(&[flat, line]);
        let at = |input: i64| quote(&merged, &BigInt::from(input));
        assert_eq!(at(0), BigInt::from(0));
        assert_eq!(at(1), BigInt::from(10));
        assert_eq!(at(2), BigInt::from(14));
        assert_eq!(at(3), BigInt::from(18));
        assert_ne!(at(2), BigInt::from(10));
    }

    #[test]
    fn equal_slopes_of_two_actions_stay_separate_and_quote_the_same() {
        let first = merge(&[vec![segment(2, 4, 1, 1), segment(2, 4, 1, 2)]]);
        let swapped = merge(&[vec![segment(2, 4, 1, 2), segment(2, 4, 1, 1)]]);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].action.pool[31], 1);
        assert_eq!(swapped[0].action.pool[31], 1);
        assert_eq!(
            quote(&first, &BigInt::from(3)),
            quote(&swapped, &BigInt::from(3))
        );
    }

    #[test]
    fn equal_slopes_of_one_action_join() {
        let merged = merge(&[vec![segment(2, 4, 1, 1), segment(2, 4, 1, 1)]]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].width, BigInt::from(4));
        assert_eq!(merged[0].output, BigInt::from(8));
        assert_eq!(quote(&merged, &BigInt::from(4)), BigInt::from(8));
    }

    #[test]
    fn a_full_segment_pays_its_output_and_a_zero_output_is_omitted() {
        let knots = [
            (BigInt::from(0), BigInt::from(0)),
            (BigInt::from(4), BigInt::from(10)),
            (BigInt::from(6), BigInt::from(10)),
        ];
        let segments = segments_from_knots(&knots, segment(1, 1, 1, 1).action);
        assert_eq!(segments.len(), 1);
        assert_eq!(quote(&segments, &segments[0].width), segments[0].output);
    }

    #[test]
    fn a_rising_secant_is_lowered_and_the_opening_slope_stays() {
        let knots = [
            (BigInt::from(0), BigInt::from(0)),
            (BigInt::from(1), BigInt::from(15)),
            (BigInt::from(2), BigInt::from(18)),
            (BigInt::from(3), BigInt::from(21)),
            (BigInt::from(4), BigInt::from(60)),
        ];
        let segments = segments_from_knots(&knots, segment(1, 1, 1, 1).action);
        assert_eq!(segments[0].output, BigInt::from(15));
        assert_eq!(quote(&segments, &BigInt::from(4)), BigInt::from(24));
    }

    #[test]
    fn backward_is_the_least_input_that_meets_the_target() {
        let segments = vec![segment(4, 10, 1, 1)];
        assert_eq!(backward(&segments, &BigInt::from(0)), Some(BigInt::from(0)));
        assert_eq!(backward(&segments, &BigInt::from(5)), Some(BigInt::from(2)));
        assert!(quote(&segments, &BigInt::from(1)) < BigInt::from(5));
        assert_eq!(backward(&segments, &BigInt::from(11)), None);
    }

    #[test]
    fn an_empty_member_list_is_an_empty_curve() {
        let merged = merge(&[]);
        assert!(merged.is_empty());
        assert_eq!(quote(&merged, &BigInt::from(3)), BigInt::from(0));
        assert_eq!(backward(&merged, &BigInt::from(1)), None);
    }
}
