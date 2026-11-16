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

pub use curve::{local_curve_payload, refit_curves, selector_inputs};
pub use domain::{domain_max, grid_amounts, GRID};
pub use estimate::estimate;
pub use plan::direction_knots;

pub(crate) use curve::curve_payload;
pub(crate) use plan::{has_trade_size, trade_capacity};

#[cfg(test)]
mod tests;
