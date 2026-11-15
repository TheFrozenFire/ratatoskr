//! Dial ranking for peers this process has already seen.
//!
//! The file is [`store::service::Peers`]. A sighting and its summary are
//! [`store_model`] records. Ranking a summary does not open the file.

use reth_network_peers::{NodeRecord, PeerId};
pub use store::service::Peers as Catalog;
pub use store_model::{
    PublishedPeer, Sighting, Summary, DIAL_FAILED, DROPPED, EMPTY, SAW_STATUS, SERVED, TOO_MANY,
};

/// Lower is a better dial for `wanted`.
///
/// A block we have already received outranks an advertisement. An advertisement
/// that starts at or below `wanted` outranks a peer we have never measured.
/// A peer whose window starts after `wanted`, or whose head was unusable, waits.
pub fn dial_rank(summary: Option<&Summary>, wanted: u64) -> u8 {
    let Some(summary) = summary else {
        return 3;
    };
    if summary.last_latest.is_some_and(|latest| latest <= 32) {
        return 5;
    }
    if summary.lowest_served.is_some_and(|block| block <= wanted) {
        return 0;
    }
    if summary
        .last_earliest
        .is_some_and(|earliest| earliest <= wanted)
    {
        return 1;
    }
    if summary.success == 0 && summary.fail > 0 && summary.last_earliest.is_none() {
        return 5;
    }
    if summary.last_earliest.is_none() {
        return 2;
    }
    4
}

pub fn node_from_summary(id: PeerId, summary: &Summary) -> NodeRecord {
    NodeRecord {
        address: summary.address,
        tcp_port: summary.tcp_port,
        udp_port: summary.udp_port,
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn summary(earliest: Option<u64>, served: Option<u64>, latest: Option<u64>) -> Summary {
        Summary {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            tcp_port: 30303,
            udp_port: 30303,
            eth: 69,
            last_outcome: SAW_STATUS,
            last_unix: 1,
            last_earliest: earliest,
            last_latest: latest,
            lowest_served: served,
            highest_served: served,
            success: u32::from(served.is_some()),
            fail: 0,
            too_many: 0,
        }
    }

    #[test]
    fn served_block_outranks_an_advertisement() {
        assert_eq!(dial_rank(Some(&summary(Some(0), Some(0), Some(100))), 0), 0);
        assert_eq!(dial_rank(Some(&summary(Some(0), None, Some(100))), 0), 1);
        assert_eq!(dial_rank(None, 0), 3);
        assert_eq!(
            dial_rank(Some(&summary(Some(11_000_000), None, Some(26_000_000))), 0),
            4
        );
    }
}
