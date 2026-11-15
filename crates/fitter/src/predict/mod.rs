//! Integer quotes for one pool direction.
//!
//! Amounts stay integers. Each quote is the output of one exact input.
//! Floor division is the integer quotient. A rational converted to an integer
//! truncates toward zero.

mod book;
mod crypto;
mod product;
mod score;
mod share;
mod spline;
mod stable;
mod weighted;

pub(crate) use book::*;
pub(crate) use crypto::*;
pub use crypto::{crypto2_out, crypto3_out};
pub(crate) use product::*;
pub(crate) use score::*;
pub(crate) use share::{Predict, A_MULTIPLIER, DUST, FEE_1E10, PRECISIONS, WAD};
pub(crate) use spline::*;
pub(crate) use stable::*;
pub(crate) use weighted::*;

#[cfg(test)]
mod tests;
