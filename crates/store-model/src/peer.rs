//! Peer directory records.
//!
//! A sighting is one dial outcome. A summary is the dial hint derived from
//! that history. A published peer is one node from a DNS tree.

use reth_network_peers::PeerId;
use std::net::IpAddr;

/// eth Status received. `earliest` is absent for eth/68.
pub const SAW_STATUS: u8 = 1;
/// The peer returned a header or a receipt batch that includes this block.
pub const SERVED: u8 = 2;
/// The peer answered, and the block was not there.
pub const EMPTY: u8 = 3;
/// The session died after the handshake.
pub const DROPPED: u8 = 4;
/// TCP, RLPx, or Status failed before we learned a window.
pub const DIAL_FAILED: u8 = 5;
/// Status was followed by `too many peers`.
pub const TOO_MANY: u8 = 6;

#[derive(Clone, Debug)]
pub struct Sighting {
    pub id: PeerId,
    pub address: IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub outcome: u8,
    pub eth: u8,
    pub earliest: Option<u64>,
    pub latest: Option<u64>,
    pub served: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub address: IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub eth: u8,
    pub last_outcome: u8,
    pub last_unix: u64,
    pub last_earliest: Option<u64>,
    pub last_latest: Option<u64>,
    pub lowest_served: Option<u64>,
    pub highest_served: Option<u64>,
    pub success: u32,
    pub fail: u32,
    pub too_many: u32,
}

/// One node from a published DNS tree. `source` bits are owned by the caller.
#[derive(Clone, Debug)]
pub struct PublishedPeer {
    pub id: PeerId,
    pub address: IpAddr,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub source: u8,
}
