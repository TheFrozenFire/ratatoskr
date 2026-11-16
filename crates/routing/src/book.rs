//! Prefix sums of one merged slope.
//!
//! A chord that is filled completely pays its output. The quote is that sum
//! plus one truncating division on the chord where the input stops.

use alloy_primitives::U256;
use num_bigint::{BigInt, Sign};
use num_traits::Zero;

use crate::pay::u128_of;

/// Where an input sits on the prefix.
pub enum At {
    /// `offset` is the input already taken on chord `index`.
    Here { index: usize, offset: BigInt },
    /// The input is past the last chord.
    Past,
    /// The prefix does not cover this input. The linear walk still does.
    Unsupported,
}

/// The open chord of a prefix whose widths and outputs fit in `u128`.
pub struct Seat {
    pub index: usize,
    /// Input already taken on this chord.
    pub offset: u128,
    pub width: u128,
    pub output: u128,
    /// Width still open on this chord.
    pub rem_w: u128,
}

/// Where an input sits when the prefix can answer in `u128`.
pub enum Spot {
    Here(Seat),
    /// The input is past the last chord.
    Past,
    /// A negative input, or a prefix that does not fit in `u128`.
    Other,
}

/// An open integer rate, or why that run is not available.
pub(crate) enum Run {
    /// The prefix is not native, or the tender is negative.
    Wide,
    /// The open chord does not pay a positive integer rate.
    Break,
    /// `allow` input stays on `rate` output per input.
    Rate { rate: u128, allow: u128 },
}

/// Whether a native slope can fund an added output.
pub(crate) enum Cover {
    /// The prefix is not native, or the tender is negative.
    Wide,
    /// The open width pays less than the asked output.
    Short,
    /// Least input whose added output meets the ask.
    Take(u128),
}

enum Body {
    Native {
        cum_w: Vec<u128>,
        cum_out: Vec<u128>,
        total_w: BigInt,
        total_out: BigInt,
    },
    General {
        cum_w: Vec<BigInt>,
        cum_out: Vec<BigInt>,
    },
}

/// Cumulative width and output of a slope whose chords have positive width.
pub struct SlopeBook {
    body: Body,
}

impl SlopeBook {
    /// `None` when a width is not positive. The linear quote stops on that chord.
    pub fn prepare<'a>(pairs: impl IntoIterator<Item = (&'a BigInt, &'a BigInt)>) -> Option<Self> {
        let pairs: Vec<(&BigInt, &BigInt)> = pairs.into_iter().collect();
        if pairs
            .iter()
            .any(|(width, _)| width.sign() != Sign::Plus || width.is_zero())
        {
            return None;
        }
        if let Some(body) = native(&pairs) {
            return Some(Self { body });
        }
        Some(Self {
            body: general(&pairs),
        })
    }

    pub fn at(&self, input: &BigInt) -> At {
        if input.sign() == Sign::Minus {
            return At::Unsupported;
        }
        match &self.body {
            Body::Native { cum_w, .. } => {
                let Some(input) = u128_of(input) else {
                    return At::Past;
                };
                let total = *cum_w.last().unwrap_or(&0);
                if input >= total {
                    return At::Past;
                }
                let end = cum_w.partition_point(|width| *width <= input);
                let index = end - 1;
                At::Here {
                    index,
                    offset: BigInt::from(input - cum_w[index]),
                }
            }
            Body::General { cum_w, .. } => {
                let total = cum_w.last().cloned().unwrap_or_else(BigInt::zero);
                if input >= &total {
                    return At::Past;
                }
                let end = cum_w.partition_point(|width| width <= input);
                let index = end - 1;
                At::Here {
                    index,
                    offset: input - &cum_w[index],
                }
            }
        }
    }

    /// The open chord. `Past` once `input` has spent the native prefix.
    pub fn spot(&self, input: &BigInt) -> Spot {
        if input.sign() == Sign::Minus {
            return Spot::Other;
        }
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return Spot::Other;
        };
        let Some(input) = u128_of(input) else {
            return Spot::Past;
        };
        let total = *cum_w.last().unwrap_or(&0);
        if input >= total {
            return Spot::Past;
        }
        let index = cum_w.partition_point(|width| *width <= input) - 1;
        let offset = input - cum_w[index];
        let width = cum_w[index + 1] - cum_w[index];
        let output = cum_out[index + 1] - cum_out[index];
        Spot::Here(Seat {
            index,
            offset,
            width,
            output,
            rem_w: width - offset,
        })
    }

    pub fn quote(&self, input: &BigInt) -> BigInt {
        if input.sign() != Sign::Plus {
            return BigInt::from(0);
        }
        match &self.body {
            Body::Native {
                cum_w,
                cum_out,
                total_out,
                ..
            } => {
                let Some(input) = u128_of(input) else {
                    return total_out.clone();
                };
                BigInt::from(quote_native(cum_w, cum_out, input))
            }
            Body::General { cum_w, cum_out } => {
                let total = cum_w.last().cloned().unwrap_or_else(BigInt::zero);
                if input >= &total {
                    return cum_out.last().cloned().unwrap_or_else(BigInt::zero);
                }
                let index = cum_w.partition_point(|width| width <= input) - 1;
                let taken = input - &cum_w[index];
                let width = &cum_w[index + 1] - &cum_w[index];
                let output = &cum_out[index + 1] - &cum_out[index];
                &cum_out[index] + &output * &taken / &width
            }
        }
    }

    pub fn total_width(&self) -> &BigInt {
        match &self.body {
            Body::Native { total_w, .. } => total_w,
            Body::General { cum_w, .. } => cum_w.last().expect("prefix"),
        }
    }

    /// Quote when every chord fits in `u128`. `None` on the general prefix.
    pub fn quote_u128(&self, input: &BigInt) -> Option<u128> {
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return None;
        };
        if input.sign() != Sign::Plus {
            return Some(0);
        }
        let Some(input) = u128_of(input) else {
            return cum_out.last().copied();
        };
        Some(quote_native(cum_w, cum_out, input))
    }

    /// Output added by spending `take` after `tender`.
    ///
    /// `None` when the prefix is not native, or when either argument is negative.
    /// A `take` past the last chord pays the output still open at `tender`.
    pub fn delta_u128(&self, tender: &BigInt, take: &BigInt) -> Option<u128> {
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return None;
        };
        if tender.sign() == Sign::Minus || take.sign() == Sign::Minus {
            return None;
        }
        let Some(start) = u128_of(tender) else {
            return Some(0);
        };
        let before = quote_native(cum_w, cum_out, start);
        let Some(take) = u128_of(take) else {
            let total = cum_out.last().copied().unwrap_or(0);
            return Some(total - before);
        };
        let Some(next) = start.checked_add(take) else {
            let total = cum_out.last().copied().unwrap_or(0);
            return Some(total - before);
        };
        Some(quote_native(cum_w, cum_out, next) - before)
    }

    /// Width still open at `input`. `None` when `input` is negative.
    pub fn remaining(&self, input: &BigInt) -> Option<BigInt> {
        if input.sign() == Sign::Minus {
            return None;
        }
        match &self.body {
            Body::Native { cum_w, .. } => {
                let total = *cum_w.last().unwrap_or(&0);
                let Some(input) = u128_of(input) else {
                    return Some(BigInt::from(0));
                };
                if input >= total {
                    Some(BigInt::from(0))
                } else {
                    Some(BigInt::from(total - input))
                }
            }
            Body::General { cum_w, .. } => {
                let total = cum_w.last().expect("prefix");
                if input >= total {
                    Some(BigInt::from(0))
                } else {
                    Some(total - input)
                }
            }
        }
    }

    /// Width still open when the prefix is native. `None` when `input` is negative.
    pub fn remaining_u128(&self, input: &BigInt) -> Option<u128> {
        let Body::Native { cum_w, .. } = &self.body else {
            return None;
        };
        if input.sign() == Sign::Minus {
            return None;
        }
        let total = *cum_w.last().unwrap_or(&0);
        let Some(input) = u128_of(input) else {
            return Some(0);
        };
        if input >= total {
            Some(0)
        } else {
            Some(total - input)
        }
    }

    /// Least input after `tender` whose added output is at least `need`.
    pub(crate) fn least_input(&self, tender: &BigInt, need: &BigInt) -> Cover {
        if need.sign() == Sign::Minus || need.is_zero() {
            return Cover::Take(0);
        }
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return Cover::Wide;
        };
        if tender.sign() == Sign::Minus {
            return Cover::Wide;
        }
        let Some(mut need) = u128_of(need) else {
            return Cover::Short;
        };
        let (mut index, mut offset) = match self.spot(tender) {
            Spot::Here(seat) => (seat.index, seat.offset),
            Spot::Past => return Cover::Short,
            Spot::Other => return Cover::Wide,
        };
        let mut spent = 0u128;
        while index + 1 < cum_w.len() {
            let width = cum_w[index + 1] - cum_w[index];
            let output = cum_out[index + 1] - cum_out[index];
            if width == 0 {
                index += 1;
                continue;
            }
            if offset >= width {
                offset -= width;
                index += 1;
                continue;
            }
            let rem_w = width - offset;
            let paid = mul_div(output, offset, width);
            if paid >= output {
                let Some(next) = spent.checked_add(rem_w) else {
                    return Cover::Wide;
                };
                spent = next;
                offset = 0;
                index += 1;
                continue;
            }
            let rem_out = output - paid;
            if need <= rem_out {
                let Some(take) = ceil_take(output, width, offset, need) else {
                    return Cover::Short;
                };
                let Some(total) = spent.checked_add(take) else {
                    return Cover::Wide;
                };
                return Cover::Take(total);
            }
            need -= rem_out;
            let Some(next) = spent.checked_add(rem_w) else {
                return Cover::Wide;
            };
            spent = next;
            offset = 0;
            index += 1;
        }
        Cover::Short
    }

    /// Width of the open chord that still pays output.
    ///
    /// `Some(None)` when `tender` is on the prefix and that chord is already
    /// spent. `None` when the prefix is not native or `tender` is negative.
    pub(crate) fn paying_width(&self, tender: &BigInt) -> Option<Option<u128>> {
        let Body::Native { .. } = &self.body else {
            return None;
        };
        if tender.sign() == Sign::Minus {
            return None;
        }
        match self.spot(tender) {
            Spot::Other => None,
            Spot::Past => Some(None),
            Spot::Here(seat) => {
                if seat.width == 0 || seat.rem_w == 0 || seat.output == 0 {
                    return Some(None);
                }
                let paid = mul_div(seat.output, seat.offset, seat.width);
                if paid >= seat.output {
                    Some(None)
                } else {
                    Some(Some(seat.rem_w))
                }
            }
        }
    }

    /// Consecutive input that stays on the open chord's integer rate.
    pub(crate) fn integer_run(&self, tender: &BigInt) -> Run {
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return Run::Wide;
        };
        if tender.sign() == Sign::Minus {
            return Run::Wide;
        }
        let Spot::Here(seat) = self.spot(tender) else {
            return Run::Break;
        };
        if seat.width == 0 || seat.output == 0 || seat.rem_w == 0 || seat.output % seat.width != 0 {
            return Run::Break;
        }
        let paid = mul_div(seat.output, seat.offset, seat.width);
        if paid >= seat.output {
            return Run::Break;
        }
        let rate = seat.output / seat.width;
        let mut allow = seat.rem_w;
        let mut index = seat.index + 1;
        while index + 1 < cum_w.len() {
            let width = cum_w[index + 1] - cum_w[index];
            let output = cum_out[index + 1] - cum_out[index];
            if width == 0 || output % width != 0 || output / width != rate {
                break;
            }
            let Some(sum) = allow.checked_add(width) else {
                return Run::Wide;
            };
            allow = sum;
            index += 1;
        }
        Run::Rate { rate, allow }
    }

    /// Input from `tender` that reaches the end of the chord holding `tender + take`.
    ///
    /// `None` when `take` is not a positive native amount.
    pub(crate) fn landing_room(&self, tender: &BigInt, take: &BigInt) -> Option<u128> {
        if take.sign() != Sign::Plus {
            return None;
        }
        let Body::Native { cum_w, .. } = &self.body else {
            return None;
        };
        let start = u128_of(tender)?;
        let take = u128_of(take)?;
        let pos = start.checked_add(take)?;
        let total = *cum_w.last().unwrap_or(&0);
        if start >= total {
            return Some(0);
        }
        if pos >= total {
            return Some(total - start);
        }
        let end = cum_w.partition_point(|width| *width < pos);
        Some(cum_w[end] - start)
    }

    /// `take` after `tender` stays on one output-per-width.
    ///
    /// `None` when the prefix is not native or an amount does not fit in `u128`.
    pub(crate) fn one_rate(&self, tender: &BigInt, take: &BigInt) -> Option<bool> {
        if take.sign() != Sign::Plus {
            return Some(false);
        }
        let Body::Native { cum_w, cum_out, .. } = &self.body else {
            return None;
        };
        let start = u128_of(tender)?;
        let take = u128_of(take)?;
        let total = *cum_w.last().unwrap_or(&0);
        if start >= total {
            return Some(false);
        }
        let mut index = cum_w.partition_point(|width| *width <= start) - 1;
        let mut offset = start - cum_w[index];
        let width0 = cum_w[index + 1] - cum_w[index];
        let output0 = cum_out[index + 1] - cum_out[index];
        if width0 == 0 || offset >= width0 {
            return Some(false);
        }
        let paid = mul_div(output0, offset, width0);
        if output0 == 0 || paid >= output0 {
            return Some(false);
        }
        let mut left = take;
        let mut first = true;
        while left > 0 && index + 1 < cum_w.len() {
            let width = cum_w[index + 1] - cum_w[index];
            let output = cum_out[index + 1] - cum_out[index];
            if width == 0 || offset >= width {
                offset = offset.saturating_sub(width);
                index += 1;
                continue;
            }
            if !first && !same_ratio(output, width, output0, width0) {
                return Some(false);
            }
            first = false;
            let room = width - offset;
            if left <= room {
                return Some(true);
            }
            left -= room;
            offset = 0;
            index += 1;
        }
        Some(false)
    }
}

fn same_ratio(output: u128, width: u128, output0: u128, width0: u128) -> bool {
    U256::from(output) * U256::from(width0) == U256::from(output0) * U256::from(width)
}

fn ceil_take(output: u128, width: u128, offset: u128, need: u128) -> Option<u128> {
    if need == 0 {
        return Some(0);
    }
    if output == 0 {
        return None;
    }
    let paid = mul_div(output, offset, width);
    let target = paid.checked_add(need)?;
    if target > output {
        return None;
    }
    let numer = U256::from(target) * U256::from(width) + U256::from(output - 1);
    let (end, _) = numer.div_rem(U256::from(output));
    let end = u128::try_from(end).ok()?;
    if end <= offset {
        return None;
    }
    let take = end - offset;
    if take > width - offset {
        return None;
    }
    Some(take)
}

fn quote_native(cum_w: &[u128], cum_out: &[u128], input: u128) -> u128 {
    let total = *cum_w.last().unwrap_or(&0);
    if input >= total {
        return *cum_out.last().unwrap_or(&0);
    }
    let index = cum_w.partition_point(|width| *width <= input) - 1;
    let taken = input - cum_w[index];
    let width = cum_w[index + 1] - cum_w[index];
    let output = cum_out[index + 1] - cum_out[index];
    cum_out[index] + mul_div(output, taken, width)
}

fn native(pairs: &[(&BigInt, &BigInt)]) -> Option<Body> {
    let mut cum_w = Vec::with_capacity(pairs.len() + 1);
    let mut cum_out = Vec::with_capacity(pairs.len() + 1);
    cum_w.push(0u128);
    cum_out.push(0u128);
    for (width, output) in pairs {
        let width = u128_of(width)?;
        let output = u128_of(output)?;
        let next_w = cum_w.last()?.checked_add(width)?;
        let next_out = cum_out.last()?.checked_add(output)?;
        cum_w.push(next_w);
        cum_out.push(next_out);
    }
    let total_w = BigInt::from(*cum_w.last()?);
    let total_out = BigInt::from(*cum_out.last()?);
    Some(Body::Native {
        cum_w,
        cum_out,
        total_w,
        total_out,
    })
}

fn general(pairs: &[(&BigInt, &BigInt)]) -> Body {
    let mut cum_w = Vec::with_capacity(pairs.len() + 1);
    let mut cum_out = Vec::with_capacity(pairs.len() + 1);
    cum_w.push(BigInt::from(0));
    cum_out.push(BigInt::from(0));
    for (width, output) in pairs {
        cum_w.push(cum_w.last().expect("width") + *width);
        cum_out.push(cum_out.last().expect("output") + *output);
    }
    Body::General { cum_w, cum_out }
}

fn mul_div(output: u128, taken: u128, width: u128) -> u128 {
    if taken == 0 || width == 0 {
        return 0;
    }
    let (quot, _) = (U256::from(output) * U256::from(taken)).div_rem(U256::from(width));
    u128::try_from(quot).unwrap_or(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{quote, Action, SlopeSegment};

    fn segment(width: i64, output: i64) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: Action {
                kind: 1,
                pool: [1; 32],
            },
        }
    }

    fn book(segments: &[SlopeSegment]) -> SlopeBook {
        SlopeBook::prepare(
            segments
                .iter()
                .map(|segment| (&segment.width, &segment.output)),
        )
        .expect("prefix")
    }

    #[test]
    fn the_prefix_matches_the_linear_quote() {
        let segments = vec![segment(4, 10), segment(4, 6), segment(5, 5), segment(1, 3)];
        let prepared = book(&segments);
        for input in 0..=20 {
            let input = BigInt::from(input);
            assert_eq!(prepared.quote(&input), quote(&segments, &input));
        }
    }

    #[test]
    fn a_wide_partial_chord_keeps_the_truncating_quotient() {
        let width = BigInt::from(10u64).pow(40);
        let output = BigInt::from(10u64).pow(30);
        let segments = vec![SlopeSegment {
            width: width.clone(),
            output: output.clone(),
            action: Action {
                kind: 1,
                pool: [2; 32],
            },
        }];
        let prepared = book(&segments);
        let taken = &width / 3;
        assert_eq!(prepared.quote(&taken), quote(&segments, &taken));
        assert_eq!(prepared.quote(&width), output);
        assert!(matches!(prepared.spot(&BigInt::from(1)), Spot::Other));
        assert!(prepared
            .delta_u128(&BigInt::from(0), &BigInt::from(1))
            .is_none());
    }

    #[test]
    fn the_seat_is_the_open_chord() {
        let segments = vec![segment(4, 10), segment(4, 6), segment(5, 5)];
        let prepared = book(&segments);
        match prepared.spot(&BigInt::from(0)) {
            Spot::Here(seat) => {
                assert_eq!(seat.index, 0);
                assert_eq!(seat.offset, 0);
                assert_eq!(seat.width, 4);
                assert_eq!(seat.output, 10);
                assert_eq!(seat.rem_w, 4);
            }
            Spot::Past | Spot::Other => panic!("open"),
        }
        match prepared.spot(&BigInt::from(5)) {
            Spot::Here(seat) => {
                assert_eq!(seat.index, 1);
                assert_eq!(seat.offset, 1);
                assert_eq!(seat.width, 4);
                assert_eq!(seat.output, 6);
                assert_eq!(seat.rem_w, 3);
            }
            Spot::Past | Spot::Other => panic!("partial"),
        }
        assert!(matches!(prepared.spot(&BigInt::from(13)), Spot::Past));
        assert!(matches!(prepared.spot(&BigInt::from(-1)), Spot::Other));
        let tender = BigInt::from(5);
        let take = BigInt::from(6);
        let delta = prepared.delta_u128(&tender, &take).expect("delta");
        assert_eq!(
            BigInt::from(delta),
            prepared.quote(&(&tender + &take)) - prepared.quote(&tender)
        );
        assert_eq!(prepared.remaining_u128(&tender), Some(8));
    }
}
