//! One outbound Ethereum devp2p session.
//!
//! This crate dials a peer, reads headers and receipts, and returns logs.
//! It does not listen, discover peers, import blocks, or retain chain state.

mod discover;
mod fetch;
mod session;

pub use discover::{collect_dns, find_mainnet_peers};
pub use fetch::{encode_block_records, fetch_block_logs, fetch_block_range, fetch_header_range};
pub use session::{shared_eth_version, Egress, OutboundSession};
