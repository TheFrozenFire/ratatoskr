//! Domain hooks the catalog calls when a stored row has to be priced.
//!
//! `venues` installs these before it opens a catalog. A missing hook is an
//! error, so a queue decision is not silently changed.

mod capacity;
mod segment;

pub use capacity::{install_capacity, Capacity};
pub use segment::{install_segments, DirectionRows};

pub(crate) use capacity::{capacity, has_trade_size};
pub(crate) use segment::{priced_pairs, segments};
