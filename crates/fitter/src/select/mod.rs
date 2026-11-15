//! Pool-direction selector. The search order matches `cas/select_fit.py`.

mod cap;
mod choose;
mod curve;
mod harness;
mod residual;
mod sample;
mod score;
mod shape;
mod spline;

pub use choose::choose;
pub use harness::{logistic_outs, poke_choose, poke_formula, select_fit};
pub use sample::{amount, labels_of, whole};
pub use score::Chosen;

#[cfg(test)]
use cap::*;
#[cfg(test)]
use curve::*;
#[cfg(test)]
use residual::*;
#[cfg(test)]
use sample::*;
#[cfg(test)]
use shape::*;

#[cfg(test)]
mod tests;
