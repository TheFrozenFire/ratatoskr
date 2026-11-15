//! Domain hooks the catalog calls when a stored row has to be priced.
//!
//! `venues` installs these before it opens a catalog. A missing hook is an
//! error, so a priority is not silently changed.

mod capacity;

pub use capacity::{install_capacity, Capacity};

pub(crate) use capacity::capacity;
