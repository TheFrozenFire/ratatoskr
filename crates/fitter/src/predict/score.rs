//! Worst relative error of a quote against labeled samples, in basis points.

use super::share::*;
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive};

pub(crate) fn worst_bps(
    predict: &dyn Fn(&BigInt) -> Option<BigInt>,
    rows: &[(BigInt, BigInt)],
) -> Option<i64> {
    let mut best: Option<i64> = None;
    for (amount, label) in rows {
        if !label.is_positive() {
            return None;
        }
        let pred = predict(amount)?;
        if pred.is_negative() {
            return None;
        }
        let gap = (&pred - label).abs();
        if gap <= *DUST {
            continue;
        }
        let score = floor_div(&(&gap * 10_000), label)
            .to_i64()
            .unwrap_or(i64::MAX);
        best = Some(match best {
            Some(so_far) => so_far.max(score),
            None => score,
        });
    }
    Some(best.unwrap_or(0))
}
