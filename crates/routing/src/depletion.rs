//! Depletion is the reserve state a pool is quoted against.
//!
//! `kept / denominator` of a tender enters a constant-product reserve. The
//! trading set stores one tender per direction and quotes it from the
//! original reserves. Same-direction tenders are that one submission. The
//! accrued-fee update deposits the whole tender and quotes the next piece
//! against the new reserve. That update is not the ledger.

use std::collections::BTreeMap;

use num_bigint::BigInt;
use num_traits::Zero;

use crate::{ceil_div, floor_div, quote, Action, SlopeSegment};

/// A tender was negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepletionFail {
    NegativeTender,
}

/// `output * tender * kept`.
pub fn fee_num(output: &BigInt, tender: &BigInt, kept: &BigInt) -> BigInt {
    output * tender * kept
}

/// `input * denominator + tender * kept`.
pub fn fee_den(input: &BigInt, tender: &BigInt, denominator: &BigInt, kept: &BigInt) -> BigInt {
    input * denominator + tender * kept
}

/// Scaled input reserve after one submission.
pub fn trading_input(
    input: &BigInt,
    tender: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    fee_den(input, tender, denominator, kept)
}

/// Cleared numerator of the input returned by selling the receipt back.
pub fn round_trip_num(
    input: &BigInt,
    tender: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    kept * kept * tender * (input * denominator + kept * tender)
}

/// Cleared denominator of that round trip.
pub fn round_trip_den(
    input: &BigInt,
    tender: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    denominator * (input * denominator * denominator + kept * kept * tender)
}

/// `tender * round_trip_den - round_trip_num`.
pub fn round_trip_shortfall(
    input: &BigInt,
    tender: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    tender * round_trip_den(input, tender, denominator, kept)
        - round_trip_num(input, tender, denominator, kept)
}

/// Denominator of a second accrued-fee submission at input reserve `input + first`.
pub fn retained_den2(
    input: &BigInt,
    first: &BigInt,
    second: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    (input + first) * denominator + second * kept
}

/// Cleared gap by which one submission exceeds the accrued-fee sequence.
pub fn retained_split_gap(
    input: &BigInt,
    output: &BigInt,
    first: &BigInt,
    second: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> BigInt {
    output * kept * input * first * second * denominator * (denominator - kept)
}

/// Floor receipt of one trading-set submission.
pub fn quote_out(
    input: &BigInt,
    output: &BigInt,
    tender: &BigInt,
    denominator: &BigInt,
    kept: &BigInt,
) -> Option<BigInt> {
    floor_div(
        &fee_num(output, tender, kept),
        &fee_den(input, tender, denominator, kept),
    )
}

/// Least input whose constant-product quote can meet `target`.
/// The rational input is `input * target / (kept * (output - target))`.
pub fn backward_input(
    input: &BigInt,
    output: &BigInt,
    kept: &BigInt,
    target: &BigInt,
) -> Option<BigInt> {
    if kept <= &BigInt::from(0) || target <= &BigInt::from(0) {
        return Some(BigInt::from(0));
    }
    if target >= output {
        return None;
    }
    ceil_div(&(input * target), &(kept * (output - target)))
}

/// One direction, quoted from the reserves it was opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradingSet {
    pub input_reserve: BigInt,
    pub output_reserve: BigInt,
    pub denominator: BigInt,
    pub kept: BigInt,
    tender: BigInt,
}

impl TradingSet {
    pub fn open(
        input_reserve: BigInt,
        output_reserve: BigInt,
        denominator: BigInt,
        kept: BigInt,
    ) -> Self {
        Self {
            input_reserve,
            output_reserve,
            denominator,
            kept,
            tender: BigInt::from(0),
        }
    }

    /// Add a same-direction tender. The receipt stays the quote of the sum.
    pub fn submit(&mut self, extra: &BigInt) -> Result<(), DepletionFail> {
        if extra < &BigInt::from(0) {
            return Err(DepletionFail::NegativeTender);
        }
        self.tender += extra;
        Ok(())
    }

    pub fn tender(&self) -> &BigInt {
        &self.tender
    }

    pub fn receipt(&self) -> Option<BigInt> {
        quote_out(
            &self.input_reserve,
            &self.output_reserve,
            &self.tender,
            &self.denominator,
            &self.kept,
        )
    }
}

/// Net tender of each action, quoted against that action's original segments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionLedger {
    tenders: BTreeMap<Action, BigInt>,
}

impl ActionLedger {
    pub fn submit(&mut self, action: Action, input: &BigInt) -> Result<(), DepletionFail> {
        if input < &BigInt::from(0) {
            return Err(DepletionFail::NegativeTender);
        }
        if input.is_zero() {
            return Ok(());
        }
        *self.tenders.entry(action).or_default() += input;
        Ok(())
    }

    pub fn tender(&self, action: &Action) -> BigInt {
        self.tenders
            .get(action)
            .cloned()
            .unwrap_or_else(BigInt::zero)
    }

    pub fn receipt(&self, action: &Action, original: &[SlopeSegment]) -> BigInt {
        quote(original, &self.tender(action))
    }
}

/// Fee-free three-coin product. Reserves are the input and two outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreeCoin {
    pub input: BigInt,
    pub left: BigInt,
    pub right: BigInt,
}

/// Spend `tender` of the input for one output. The receipt is
/// `output * tender / (input + tender)`. The whole tender enters the input reserve.
pub fn spend_fee_free(
    input: &BigInt,
    output: &BigInt,
    tender: &BigInt,
) -> Option<(BigInt, BigInt, BigInt)> {
    let denominator = input + tender;
    let receipt = floor_div(&(output * tender), &denominator)?;
    Some((input + tender, output - &receipt, receipt))
}

/// Apply `order` to a fee-free three-coin pool. `0` spends the left output
/// and `1` spends the right. Each leg tenders the same input amount.
pub fn settle_three_coin(
    start: &ThreeCoin,
    order: &[usize],
    tender: &BigInt,
) -> Option<(ThreeCoin, Vec<BigInt>)> {
    let mut pool = start.clone();
    let mut receipts = Vec::with_capacity(order.len());
    for leg in order {
        let output = match leg {
            0 => pool.left.clone(),
            1 => pool.right.clone(),
            _ => return None,
        };
        let (next_input, next_output, receipt) = spend_fee_free(&pool.input, &output, tender)?;
        pool.input = next_input;
        match leg {
            0 => pool.left = next_output,
            1 => pool.right = next_output,
            _ => return None,
        }
        receipts.push(receipt);
    }
    Some((pool, receipts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SlopeSegment;

    fn z(value: i64) -> BigInt {
        BigInt::from(value)
    }

    fn action(byte: u8) -> Action {
        let mut pool = [0_u8; 32];
        pool[31] = byte;
        Action { kind: 1, pool }
    }

    fn segment(width: i64, output: i64, pool_byte: u8) -> SlopeSegment {
        SlopeSegment {
            width: z(width),
            output: z(output),
            action: action(pool_byte),
        }
    }

    #[test]
    fn same_direction_tenders_are_one_submission() {
        let mut split = TradingSet::open(z(1000), z(1000), z(1000), z(997));
        split.submit(&z(100)).expect("first");
        split.submit(&z(50)).expect("second");
        let mut once = TradingSet::open(z(1000), z(1000), z(1000), z(997));
        once.submit(&z(150)).expect("sum");
        assert_eq!(split.receipt(), once.receipt());
        assert_eq!(
            trading_input(&z(1000), &z(100), &z(1000), &z(997)) + z(997) * z(50),
            trading_input(&z(1000), &z(150), &z(1000), &z(997))
        );
        let output = z(7);
        let first = z(2);
        let second = z(3);
        let input = z(11);
        let denominator = z(9);
        let kept = z(5);
        let sum = &first + &second;
        assert_eq!(
            fee_num(&output, &sum, &kept) * fee_den(&input, &first, &denominator, &kept),
            fee_num(&output, &first, &kept) * fee_den(&input, &sum, &denominator, &kept)
                + &output * &input * &denominator * &kept * &second
        );
    }

    #[test]
    fn a_round_trip_loses_input_when_kept_is_below_the_denominator() {
        let short = round_trip_shortfall(&z(1000), &z(100), &z(1000), &z(997));
        let factored = z(100)
            * (z(1000) - z(997))
            * (z(1000) * z(1000) * (z(1000) + z(997)) + z(997) * z(997) * z(100));
        assert_eq!(short, factored);
        assert!(short > z(0));
        let den = round_trip_den(&z(1000), &z(100), &z(1000), &z(997));
        assert_eq!(short * z(109940090), z(62892027) * &den);
        assert_eq!(
            round_trip_shortfall(&z(1000), &z(100), &z(1000), &z(1000)),
            z(0)
        );
        assert!(round_trip_den(&z(4), &z(0), &z(3), &z(1)) > z(0));
    }

    #[test]
    fn separate_quotes_of_the_original_reserves_meet_or_exceed_the_net() {
        for input in 0..=3 {
            for output in 0..=3 {
                for left in 0..=3 {
                    for right in 0..=3 {
                        for denominator in 0..=3 {
                            for kept in 0..=3 {
                                let scale = z(input) * z(denominator);
                                let net_num = z(output) * z(kept) * z(left + right);
                                let left_den = &scale + z(kept) * z(left);
                                let right_den = &scale + z(kept) * z(right);
                                let net_den = &scale + z(kept) * z(left + right);
                                let separate = z(output) * z(kept) * z(left) * &right_den
                                    + z(output) * z(kept) * z(right) * &left_den;
                                assert!(net_num * (&left_den * &right_den) <= separate * net_den);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_accrued_second_submission_pays_less_than_one_submission() {
        let gap = retained_split_gap(&z(1000), &z(1000), &z(100), &z(50), &z(1000), &z(997));
        assert_eq!(
            gap,
            z(1000) * z(997) * z(1000) * z(100) * z(50) * z(1000) * (z(1000) - z(997))
        );
        assert!(gap > z(0));
        assert_eq!(
            retained_split_gap(&z(1000), &z(1000), &z(100), &z(50), &z(1000), &z(1000)),
            z(0)
        );
        assert_eq!(
            retained_split_gap(&z(1000), &z(1000), &z(100), &z(0), &z(1000), &z(997)),
            z(0)
        );
        let first_then_second = fee_den(&z(1000), &z(100), &z(1000), &z(997))
            * retained_den2(&z(1000), &z(100), &z(50), &z(1000), &z(997));
        let second_then_first = fee_den(&z(1000), &z(50), &z(1000), &z(997))
            * retained_den2(&z(1000), &z(50), &z(100), &z(1000), &z(997));
        assert_ne!(first_then_second, second_then_first);
    }

    #[test]
    fn two_submits_of_one_action_quote_the_original_segments_once() {
        let original = [segment(4, 10, 1), segment(4, 6, 1)];
        let mut ledger = ActionLedger::default();
        ledger.submit(action(1), &z(3)).expect("first");
        ledger.submit(action(1), &z(3)).expect("second");
        assert_eq!(ledger.tender(&action(1)), z(6));
        assert_eq!(
            ledger.receipt(&action(1), &original),
            quote(&original, &z(6))
        );
        let separate = quote(&original, &z(3)) + quote(&original, &z(3));
        assert!(separate >= ledger.receipt(&action(1), &original));
        assert_eq!(
            ledger.submit(action(1), &z(-1)),
            Err(DepletionFail::NegativeTender)
        );
    }

    #[test]
    fn swapping_the_outputs_swaps_the_ending_reserves() {
        let start = ThreeCoin {
            input: z(12),
            left: z(12),
            right: z(12),
        };
        let (left_first, left_receipts) = settle_three_coin(&start, &[0, 1], &z(6)).expect("left");
        let (right_first, right_receipts) =
            settle_three_coin(&start, &[1, 0], &z(6)).expect("right");
        assert_eq!(left_receipts, vec![z(4), z(3)]);
        assert_eq!(right_receipts, vec![z(4), z(3)]);
        assert_eq!(left_first.input, z(24));
        assert_eq!(left_first.left, z(8));
        assert_eq!(left_first.right, z(9));
        assert_eq!(right_first.left, z(9));
        assert_eq!(right_first.right, z(8));
        assert_ne!(left_first.left, left_first.right);
    }

    #[test]
    fn a_depleted_reserve_asks_at_least_as_much_input() {
        let fresh = backward_input(&z(6), &z(6), &z(1), &z(2)).expect("fresh");
        let grown = backward_input(&z(8), &z(6), &z(1), &z(2)).expect("grown");
        let shrunk = backward_input(&z(6), &z(5), &z(1), &z(2)).expect("shrunk");
        let both = backward_input(&z(8), &z(5), &z(1), &z(2)).expect("both");
        assert!(fresh <= grown);
        assert!(fresh <= shrunk);
        assert!(fresh <= both);
        assert!(backward_input(&z(6), &z(6), &z(1), &z(6)).is_none());
        for input in 0..=4 {
            for raised in input..=5 {
                for output in 1..=5 {
                    for reduced in 1..=output {
                        for target in 0..reduced {
                            let low = backward_input(&z(input), &z(output), &z(1), &z(target));
                            let high = backward_input(&z(raised), &z(reduced), &z(1), &z(target));
                            if let (Some(low), Some(high)) = (low, high) {
                                assert!(low <= high);
                            }
                        }
                    }
                }
            }
        }
    }
}
