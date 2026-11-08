//! Durable memory of peers we have actually dialed.
//!
//! DNS and ENR records do not carry a history window. `earliest` and `latest`
//! arrive in eth Status, and a served block is stronger than that advertisement
//! because a peer can change what it claims. Sightings are the history.
//! Summaries are the dial hint derived from that history.

use redb::{Database, Durability, ReadableTable, TableDefinition};
use reth_network_peers::{NodeRecord, PeerId};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const SIGHTINGS: TableDefinition<u64, &[u8]> = TableDefinition::new("sightings");
const SUMMARIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("summaries");
const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const VERSION: u8 = 1;
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

const HAS_EARLIEST: u8 = 1;
const HAS_LATEST: u8 = 2;
const HAS_SERVED: u8 = 4;

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

pub struct Catalog {
    db: Database,
    pub next_seq: u64,
    pub summaries: HashMap<PeerId, Summary>,
}

impl Catalog {
    pub fn open(path: &Path) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
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
        txn.set_durability(Durability::Eventual);
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
        txn.set_durability(Durability::Immediate);
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

pub fn default_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/ratatoskr")
        .join("peers.redb")
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
    use std::net::Ipv4Addr;

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

    #[test]
    fn summary_roundtrip_keeps_a_changed_window() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-peers-{}-{}.redb",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_file(&path);
        let mut catalog = Catalog::open(&path).unwrap();
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
        let loaded = Catalog::open(&path).unwrap();
        let summary = loaded.summaries.get(&id).unwrap();
        assert_eq!(summary.last_earliest, Some(0));
        assert_eq!(loaded.next_seq, 2);
        let _ = std::fs::remove_file(&path);
    }
}
