//! Local samples and the curve stored for a pool snapshot.
//!
//! [`domain`] is the largest input that still has a price, and the grid of
//! fractions of that input. [`plan`] quotes that grid on the ingested snapshot.
//! [`curve`] chooses the stored formula from those quotes.
//! [`estimate`] quotes a stored curve.

mod curve;
mod domain;
mod estimate;
mod formula;
mod plan;

pub use curve::refit_curves;
pub use estimate::estimate;

pub(crate) use curve::{curve_payload, local_curve_payload};
pub(crate) use plan::{direction_knots, has_trade_size, trade_capacity};

#[cfg(test)]
mod tests;
