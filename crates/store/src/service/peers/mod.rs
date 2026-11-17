//! Peer directory service.
//!
//! The file remembers peers this process has dialed. Dial ranking stays with
//! the dialer. Records are [`store_model`] peer types.

mod record;

use crate::storage::{Database, ReadDatabase};
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use reth_network_peers::PeerId;
use std::{
    collections::HashMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use store_model::{PublishedPeer, Sighting, Summary};

const SIGHTINGS: TableDefinition<u64, &[u8]> = TableDefinition::new("sightings");

const SUMMARIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("summaries");

const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const VERSION: u8 = 1;

const HAS_EARLIEST: u8 = 1;

const HAS_LATEST: u8 = 2;

const HAS_SERVED: u8 = 4;

pub struct Peers {
    db: Database,
    pub next_seq: u64,
    pub summaries: HashMap<PeerId, Summary>,
}

/// One stored dial summary.
#[derive(Clone, Debug)]
pub struct PeerSummary {
    pub id: PeerId,
    pub summary: Summary,
}

/// `peers.redb` while another process writes it.
pub struct PeerRead {
    db: ReadDatabase,
}

impl PeerRead {
    pub fn open(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: ReadDatabase::open(path, cache)?,
        })
    }

    pub fn page(&self, after: Option<&[u8]>, limit: usize) -> eyre::Result<Vec<PeerSummary>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(SUMMARIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut page = Vec::new();
        let rows = match after {
            Some(after) => table.range::<&[u8]>(after..)?,
            None => table.range::<&[u8]>(..)?,
        };
        for item in rows {
            if page.len() == limit {
                break;
            }
            let (key, value) = item?;
            if after.is_some_and(|cursor| key.value() == cursor) {
                continue;
            }
            let Some(summary) = record::decode_summary(value.value()) else {
                continue;
            };
            let id = PeerId::from_slice(key.value());
            page.push(PeerSummary { id, summary });
        }
        Ok(page)
    }

    pub fn sightings(&self) -> eyre::Result<u64> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(SIGHTINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        Ok(table.len()?)
    }
}

impl Peers {
    pub fn open(path: &Path) -> eyre::Result<Self> {
        Self::open_with(path, config::Store::default().peers)
    }

    pub fn open_with(path: &Path, cache: usize) -> eyre::Result<Self> {
        let db = Database::create(path, cache)?;
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
        let summary = record::merge(previous.as_ref(), &sighting, unix_now());
        let append = record::history_changes(previous.as_ref(), &sighting);
        self.summaries.insert(sighting.id, summary.clone());
        if let Err(err) = self.persist(&sighting, &summary, append) {
            observability::warn!(%err, "peer sighting was not stored");
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
            let encoded = record::encode_summary(summary);
            summaries.insert(sighting.id.as_slice(), encoded.as_slice())?;
            if append {
                let mut sightings = txn.open_table(SIGHTINGS)?;
                let mut meta = txn.open_table(META)?;
                let seq = self.next_seq;
                let encoded = record::encode_sighting(sighting, summary.last_unix);
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
                let encoded = record::encode_published(peer, seen);
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
        if let Some(summary) = record::decode_summary(value.value()) {
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
    use std::net::{IpAddr, Ipv4Addr};
    use store_model::SAW_STATUS;
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
