//! A spot is a sized round trip from one numeraire into a token and back.
//!
//! The route is one shape in both directions: the direct pair conduit, a path
//! through one hub, or a path through two hubs. The size is the smallest
//! numeraire amount both directions fully clear. A round trip that returns
//! less than half that size is not a spot. The stored numbers are the size,
//! the tokens bought, and the numeraire returned. Their squared price is
//! `(size * sold) / bought²`. This crate does not read a feed answer.

mod price;
mod run;

pub use price::{mainnet_numeraires, price, rank_hubs, CustodialWrap, Edge, Numeraire, USDC, WETH};
pub use run::{conduit_edges, run};
pub use store_model::Spot;
