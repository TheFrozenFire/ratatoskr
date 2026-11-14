//! Records the store services read and write.
//!
//! A record is the value a file holds. Parsing a directory, following a log,
//! and pricing a round trip stay in the crates that do that work.

pub mod feed;
pub mod spot;

pub use feed::{
    Answer, Configuration, DirectoryRow, Feed, Gauges, HistoryJob, Listing, Mismatch,
    ObservedAnswer, Phase, ProxyHead, Read,
};
pub use spot::Spot;
