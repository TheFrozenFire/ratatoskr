//! Peer directory service.
//!
//! The file remembers peers this process has dialed. Dial ranking stays with
//! the dialer. Records are [`store_model`] peer types.

use crate::storage::Database;
use redb::{Durability, ReadableDatabase, ReadableTable, TableDefinition};
use reth_network_peers::PeerId;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use store_model::{
    PublishedPeer, Sighting, Summary, DIAL_FAILED, DROPPED, EMPTY, SAW_STATUS, SERVED, TOO_MANY,
};

const SIGHTINGS: TableDefinition<u64, &[u8]> = TableDefinition::new("sightings");
const SUMMARIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("summaries");
const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Summaries load once. Later writes are single sightings.
const PAGE_CACHE: usize = 16 * 1024 * 1024;

const VERSION: u8 = 1;

const HAS_EARLIEST: u8 = 1;
const HAS_LATEST: u8 = 2;
const HAS_SERVED: u8 = 4;

pub struct Peers {
    db: Database,
    pub next_seq: u64,
    pub summaries: HashMap<PeerId, Summary>,
}

impl Peers {
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let db = Database::create(path, PAGE_CACHE)?;
        db.watch();
        let next_seq = meta_get(&db, "sighting_seq")?.unwrap_or(0);
        let summaries = load_summaries(&db)?;
        Ok(Self {
            db,
            next_seq,
            summaries,
        })
    }

    pub fn record(&mut self, sighting: Sighting) {
        let previous = self.summaries.get(&sighting.id).cloned();
        let summary = merge(previous.as_ref(), &sighting, unix_now());
        let append = history_changes(previous.as_ref(), &sighting);
        self.summaries.insert(sighting.id, summary.clone());
        if let Err(err) = self.persist(&sighting, &summary, append) {
            tracing::warn!(%err, "peer sighting was not stored");
        }
    }

    fn persist(
        &mut self,
        sighting: &Sighting,
        summary: &Summary,
        append: bool,
    ) -> eyre::Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        {
            let mut summaries = txn.open_table(SUMMARIES)?;
            let encoded = encode_summary(summary);
            summaries.insert(sighting.id.as_slice(), encoded.as_slice())?;
            if append {
                let mut sightings = txn.open_table(SIGHTINGS)?;
                let mut meta = txn.open_table(META)?;
                let seq = self.next_seq;
                let encoded = encode_sighting(sighting, summary.last_unix);
                sightings.insert(seq, encoded.as_slice())?;
                self.next_seq = seq.saturating_add(1);
                meta.insert("sighting_seq", self.next_seq)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Upsert published-node rows. Dial history in the same file is left as it is.
    pub fn write_published(&self, peers: &[PublishedPeer]) -> eyre::Result<()> {
        let seen = unix_now();
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        {
            let mut table = txn.open_table(NODES)?;
            for peer in peers {
                let encoded = encode_published(peer, seen);
                table.insert(peer.id.as_slice(), encoded.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("peers.redb")
    }
}

fn encode_published(peer: &PublishedPeer, seen: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 1 + 16 + 2 + 2 + 8);
    out.push(VERSION);
    out.push(peer.source);
    push_addr(&mut out, peer.address);
    out.extend_from_slice(&peer.tcp_port.to_le_bytes());
    out.extend_from_slice(&peer.udp_port.to_le_bytes());
    out.extend_from_slice(&seen.to_le_bytes());
    out
}

fn history_changes(previous: Option<&Summary>, sighting: &Sighting) -> bool {
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

fn merge(previous: Option<&Summary>, sighting: &Sighting, now: u64) -> Summary {
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

fn encode_summary(summary: &Summary) -> Vec<u8> {
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

fn encode_sighting(sighting: &Sighting, unix: u64) -> Vec<u8> {
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

fn decode_summary(bytes: &[u8]) -> Option<Summary> {
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

fn load_summaries(db: &Database) -> eyre::Result<HashMap<PeerId, Summary>> {
    let mut summaries = HashMap::new();
    let txn = db.begin_read()?;
    let table = match txn.open_table(SUMMARIES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(summaries),
        Err(err) => return Err(err.into()),
    };
    for row in table.iter()? {
        let (key, value) = row?;
        let mut id = PeerId::ZERO;
        let bytes = key.value();
        if bytes.len() != id.len() {
            continue;
        }
        id.copy_from_slice(bytes);
        if let Some(summary) = decode_summary(value.value()) {
            summaries.insert(id, summary);
        }
    }
    Ok(summaries)
}

fn meta_get(db: &Database, key: &str) -> eyre::Result<Option<u64>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(table.get(key)?.map(|value| value.value()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    #[test]
    fn summary_roundtrip_keeps_a_changed_window() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-peers-{}-{}.redb",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_file(&path);
        let mut catalog = Peers::open(&path).unwrap();
        let id = PeerId::ZERO;
        let base = Sighting {
            id,
            address: IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
            tcp_port: 30303,
            udp_port: 30303,
            outcome: SAW_STATUS,
            eth: 69,
            earliest: Some(11_000_000),
            latest: Some(20_000_000),
            served: None,
        };
        catalog.record(base.clone());
        let mut later = base;
        later.earliest = Some(0);
        catalog.record(later);
        drop(catalog);
        let loaded = Peers::open(&path).unwrap();
        let summary = loaded.summaries.get(&id).unwrap();
        assert_eq!(summary.last_earliest, Some(0));
        assert_eq!(loaded.next_seq, 2);
        let _ = std::fs::remove_file(&path);
    }
}
