//! Ports over the open files.
//!
//! [`crate::LocalStore`] holds an optional header chain, receipt corpus, and
//! venue catalog. Each trait names one of those services. A caller bounds the
//! ports its work uses. [`crate::RemoteStore`] implements the ports the
//! entity-store socket speaks: chain, corpus, discovery, and venue.

mod chain;
mod conduit;
mod corpus;
mod discovery;
mod token;
mod venue;

pub use chain::ChainStore;
pub use conduit::ConduitStore;
pub use corpus::CorpusStore;
pub use discovery::DiscoveryStore;
pub use token::TokenStore;
pub use venue::VenueStore;
