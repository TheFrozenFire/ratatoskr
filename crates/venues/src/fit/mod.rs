//! Quote samples and the curve stored for a pool snapshot.
//!
//! [`domain`] is the largest input that still has a price, and the grid of
//! fractions of that input. [`plan`] quotes that grid on the ingested snapshot.
//! [`curve`] chooses the stored formula from those quotes. [`extract`] copies
//! stored chain samples to a JSONL file. [`prepare`] turns a snapshot into one
//! lens call, [`abi`] encodes it, and [`execute`] runs the calls. The state
//! pipeline calls [`curve`] and does not call [`execute`].

mod abi;
mod curve;
mod domain;
mod execute;
mod extract;
mod formula;
mod plan;
mod prepare;

use alloy_primitives::Address;

/// Runtime is installed with a state override. Nothing is deployed at this address.
pub(super) const QUOTE_LENS: Address =
    alloy_primitives::address!("0000000000000000000000000000000000510e51");

pub(super) fn multicall() -> Address {
    chain::seeds::document().multicall
}

pub use curve::refit_curves;
pub use extract::{drop_partial_quotes, export_curve_fit, import_curve_fit, FitReport};

pub(crate) use curve::{curve_payload, local_curve_payload};
pub(crate) use plan::{direction_knots, has_trade_size, trade_capacity};

#[cfg(test)]
mod tests;
