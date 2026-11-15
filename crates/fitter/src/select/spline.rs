//! Chord, cubic, and residual candidates that pass through the training knots.

use crate::formula::Formula;
use crate::select::sample::*;
use crate::select::score::*;
use crate::units::*;
use num_bigint::BigInt;

use serde_json::Value;

pub(crate) fn through_sample_candidates(
    direction: &Value,
    rows: &[(BigInt, BigInt)],
) -> Result<Vec<(&'static str, Scored)>, String> {
    let (train, hold) = split_holdout(direction)?;
    let made = [
        (
            "chord",
            Formula::Chord {
                knots: train.clone(),
            },
        ),
        (
            "cubic",
            Formula::Cubic {
                knots: train.clone(),
            },
        ),
        (
            "residual",
            Formula::HyperResidual {
                knots: train.clone(),
            },
        ),
    ];
    let mut found = Vec::new();
    for (name, formula) in made {
        let Some(scored) = gated(&formula, &hold, rows) else {
            continue;
        };
        found.push((name, scored));
    }
    Ok(found)
}
