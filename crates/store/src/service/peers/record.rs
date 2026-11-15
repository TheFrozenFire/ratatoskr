//! Sighting and summary bytes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use store_model::{
    PublishedPeer, Sighting, Summary, DIAL_FAILED, DROPPED, EMPTY, SAW_STATUS, SERVED, TOO_MANY,
};

use super::*;

pub(super) fn encode_published(peer: &PublishedPeer, seen: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 1 + 16 + 2 + 2 + 8);
    out.push(VERSION);
    out.push(peer.source);
    push_addr(&mut out, peer.address);
    out.extend_from_slice(&peer.tcp_port.to_le_bytes());
    out.extend_from_slice(&peer.udp_port.to_le_bytes());
    out.extend_from_slice(&seen.to_le_bytes());
    out
}

pub(super) fn history_changes(previous: Option<&Summary>, sighting: &Sighting) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    match sighting.outcome {
        SAW_STATUS => {
            previous.last_earliest != sighting.earliest
                || previous.last_latest != sighting.latest
                || previous.eth != sighting.eth
        }
        SERVED => {
            let block = sighting.served.unwrap_or(u64::MAX);
            previous.lowest_served.is_none_or(|lowest| block < lowest)
                || previous
                    .highest_served
                    .is_none_or(|highest| block > highest)
        }
        _ => true,
    }
}

pub(super) fn merge(previous: Option<&Summary>, sighting: &Sighting, now: u64) -> Summary {
    let mut summary = previous.cloned().unwrap_or(Summary {
        address: sighting.address,
        tcp_port: sighting.tcp_port,
        udp_port: sighting.udp_port,
        eth: sighting.eth,
        last_outcome: sighting.outcome,
        last_unix: now,
        last_earliest: None,
        last_latest: None,
        lowest_served: None,
        highest_served: None,
        success: 0,
        fail: 0,
        too_many: 0,
    });
    summary.address = sighting.address;
    summary.tcp_port = sighting.tcp_port;
    if sighting.udp_port != 0 {
        summary.udp_port = sighting.udp_port;
    }
    if sighting.eth != 0 {
        summary.eth = sighting.eth;
    }
    summary.last_outcome = sighting.outcome;
    summary.last_unix = now;
    if sighting.earliest.is_some() {
        summary.last_earliest = sighting.earliest;
    }
    if sighting.latest.is_some() {
        summary.last_latest = sighting.latest;
    }
    if let Some(block) = sighting.served {
        summary.lowest_served = Some(
            summary
                .lowest_served
                .map(|lowest| lowest.min(block))
                .unwrap_or(block),
        );
        summary.highest_served = Some(
            summary
                .highest_served
                .map(|highest| highest.max(block))
                .unwrap_or(block),
        );
    }
    match sighting.outcome {
        SERVED => summary.success = summary.success.saturating_add(1),
        TOO_MANY => {
            summary.fail = summary.fail.saturating_add(1);
            summary.too_many = summary.too_many.saturating_add(1);
        }
        EMPTY | DROPPED | DIAL_FAILED => summary.fail = summary.fail.saturating_add(1),
        _ => {}
    }
    summary
}

pub(super) fn encode_summary(summary: &Summary) -> Vec<u8> {
    let mut out = Vec::with_capacity(77);
    out.push(VERSION);
    out.push(summary.last_outcome);
    out.push(summary.eth);
    out.push(flags(
        summary.last_earliest,
        summary.last_latest,
        summary.lowest_served,
    ));
    out.extend_from_slice(&summary.last_unix.to_le_bytes());
    out.extend_from_slice(&summary.last_earliest.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&summary.last_latest.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&summary.lowest_served.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&summary.highest_served.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&summary.success.to_le_bytes());
    out.extend_from_slice(&summary.fail.to_le_bytes());
    out.extend_from_slice(&summary.too_many.to_le_bytes());
    push_addr(&mut out, summary.address);
    out.extend_from_slice(&summary.tcp_port.to_le_bytes());
    out.extend_from_slice(&summary.udp_port.to_le_bytes());
    out
}

pub(super) fn encode_sighting(sighting: &Sighting, unix: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(119);
    out.push(VERSION);
    out.push(sighting.outcome);
    out.push(sighting.eth);
    out.push(flags(sighting.earliest, sighting.latest, sighting.served));
    out.extend_from_slice(&unix.to_le_bytes());
    out.extend_from_slice(&sighting.earliest.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&sighting.latest.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&sighting.served.unwrap_or(0).to_le_bytes());
    push_addr(&mut out, sighting.address);
    out.extend_from_slice(&sighting.tcp_port.to_le_bytes());
    out.extend_from_slice(sighting.id.as_slice());
    out
}

fn flags(earliest: Option<u64>, latest: Option<u64>, served: Option<u64>) -> u8 {
    let mut flags = 0;
    if earliest.is_some() {
        flags |= HAS_EARLIEST;
    }
    if latest.is_some() {
        flags |= HAS_LATEST;
    }
    if served.is_some() {
        flags |= HAS_SERVED;
    }
    flags
}

fn push_addr(out: &mut Vec<u8>, address: IpAddr) {
    match address {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
            out.extend_from_slice(&[0u8; 12]);
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
}

pub(super) fn decode_summary(bytes: &[u8]) -> Option<Summary> {
    if bytes.len() != 77 || bytes[0] != VERSION {
        return None;
    }
    let flags = bytes[3];
    let earliest = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    let latest = u64::from_le_bytes(bytes[20..28].try_into().ok()?);
    let lowest = u64::from_le_bytes(bytes[28..36].try_into().ok()?);
    let highest = u64::from_le_bytes(bytes[36..44].try_into().ok()?);
    Some(Summary {
        last_outcome: bytes[1],
        eth: bytes[2],
        last_unix: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
        last_earliest: (flags & HAS_EARLIEST != 0).then_some(earliest),
        last_latest: (flags & HAS_LATEST != 0).then_some(latest),
        lowest_served: (flags & HAS_SERVED != 0).then_some(lowest),
        highest_served: (flags & HAS_SERVED != 0).then_some(highest),
        success: u32::from_le_bytes(bytes[44..48].try_into().ok()?),
        fail: u32::from_le_bytes(bytes[48..52].try_into().ok()?),
        too_many: u32::from_le_bytes(bytes[52..56].try_into().ok()?),
        address: decode_addr(&bytes[56..73])?,
        tcp_port: u16::from_le_bytes(bytes[73..75].try_into().ok()?),
        udp_port: u16::from_le_bytes(bytes[75..77].try_into().ok()?),
    })
}

fn decode_addr(bytes: &[u8]) -> Option<IpAddr> {
    match *bytes.first()? {
        4 => Some(IpAddr::V4(Ipv4Addr::new(
            bytes[1], bytes[2], bytes[3], bytes[4],
        ))),
        6 => {
            let octets: [u8; 16] = bytes[1..17].try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}
