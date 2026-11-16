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
                let total = *cum_w.last().unwrap_or(&0);
                if input >= total {
                    return total_out.clone();
                }
                let index = cum_w.partition_point(|width| *width <= input) - 1;
                let taken = input - cum_w[index];
                let width = cum_w[index + 1] - cum_w[index];
                let output = cum_out[index + 1] - cum_out[index];
                BigInt::from(cum_out[index] + mul_div(output, taken, width))
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
        let total = *cum_w.last().unwrap_or(&0);
        if input >= total {
            return cum_out.last().copied();
        }
        let index = cum_w.partition_point(|width| *width <= input) - 1;
        let taken = input - cum_w[index];
        let width = cum_w[index + 1] - cum_w[index];
        let output = cum_out[index + 1] - cum_out[index];
        Some(cum_out[index] + mul_div(output, taken, width))
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
    }
}
