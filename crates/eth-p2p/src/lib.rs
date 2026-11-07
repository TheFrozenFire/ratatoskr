//! One outbound Ethereum devp2p session.
//!
//! This crate dials a peer, reads headers and receipts, and returns logs.
//! It does not listen, discover peers, import blocks, or retain chain state.

mod discover;
mod fetch;
mod session;

pub use discover::{census_mainnet_dns, dns_candidates, find_mainnet_peers, DnsPeer, DNS_ALL, DNS_SNAP};
pub use fetch::{encode_block_records, fetch_block_logs, fetch_block_range, fetch_header_range};
pub use corpus::logs_in_block;
pub use session::{shared_eth_version, OutboundSession};
