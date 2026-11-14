//! The spot file. One row is one token in one numeraire.
//!
//! A pass replaces the table, so a token that lost its route is gone. The
//! table is not the feed answer table, and this file is not `feeds.redb`.

use crate::Spot;
use alloy_primitives::Address;
use num_bigint::BigInt;
use redb::{Durability, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const SPOTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("spots");
const VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Store(String),
}

fn db_err(err: impl std::fmt::Display) -> Error {
    Error::Store(err.to_string())
}

pub struct Spots {
    db: redb::Database,
}

impl Spots {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(db_err)?;
            }
        }
        let mut builder = redb::Builder::new();
        builder.set_cache_size(8 * 1024 * 1024);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let db = if path.exists() {
            builder.open(path)
        } else {
            builder.create(path)
        }
        .map_err(db_err)?;
        Ok(Self { db })
    }

    /// Replace every stored spot. The write is one commit.
    pub fn replace(&self, spots: &[Spot]) -> Result<(), Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut table = write.open_table(SPOTS).map_err(db_err)?;
            let mut keys = Vec::new();
            for item in table.iter().map_err(db_err)? {
                let (key, _) = item.map_err(db_err)?;
                keys.push(key.value().to_vec());
            }
            for key in &keys {
                table.remove(key.as_slice()).map_err(db_err)?;
            }
            for spot in spots {
                table
                    .insert(key_of(spot).as_slice(), encode(spot).as_slice())
                    .map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn read(&self) -> Result<Vec<Spot>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(SPOTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut spots = Vec::new();
        for item in table.iter().map_err(db_err)? {
            let (key, value) = item.map_err(db_err)?;
            let key = key.value();
            if key.len() != 40 {
                return Err(Error::Store(
                    "spot key is not a token and a numeraire".into(),
                ));
            }
            spots.push(decode(key, value.value())?);
        }
        spots.sort_by(|left, right| {
            left.token
                .cmp(&right.token)
                .then(left.numeraire.cmp(&right.numeraire))
        });
        Ok(spots)
    }
}

pub fn default_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr/spots.redb")
}

fn key_of(spot: &Spot) -> [u8; 40] {
    let mut key = [0_u8; 40];
    key[..20].copy_from_slice(spot.token.as_slice());
    key[20..].copy_from_slice(spot.numeraire.as_slice());
    key
}

fn encode(spot: &Spot) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(VERSION);
    out.push(spot.hops);
    out.extend_from_slice(&spot.block.to_le_bytes());
    push_int(&mut out, &spot.size);
    push_int(&mut out, &spot.bought);
    push_int(&mut out, &spot.sold);
    out
}

fn decode(key: &[u8], body: &[u8]) -> Result<Spot, Error> {
    if body.len() < 10 {
        return Err(Error::Store("spot row is short".into()));
    }
    if body[0] != VERSION {
        return Err(Error::Store(format!("spot version {} is not 1", body[0])));
    }
    let hops = body[1];
    let mut block = [0_u8; 8];
    block.copy_from_slice(&body[2..10]);
    let mut at = 10;
    let size = take_int(body, &mut at)?;
    let bought = take_int(body, &mut at)?;
    let sold = take_int(body, &mut at)?;
    if at != body.len() {
        return Err(Error::Store("spot row has trailing bytes".into()));
    }
    Ok(Spot {
        token: Address::from_slice(&key[..20]),
        numeraire: Address::from_slice(&key[20..]),
        size,
        bought,
        sold,
        hops,
        block: u64::from_le_bytes(block),
    })
}

fn push_int(out: &mut Vec<u8>, value: &BigInt) {
    let (_, bytes) = value.to_bytes_be();
    let len = u32::try_from(bytes.len()).expect("a spot integer fits in a u32 length");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes);
}

fn take_int(body: &[u8], at: &mut usize) -> Result<BigInt, Error> {
    if *at + 4 > body.len() {
        return Err(Error::Store("spot integer is short".into()));
    }
    let mut len_bytes = [0_u8; 4];
    len_bytes.copy_from_slice(&body[*at..*at + 4]);
    *at += 4;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if *at + len > body.len() {
        return Err(Error::Store("spot integer is short".into()));
    }
    let bytes = &body[*at..*at + len];
    *at += len;
    Ok(BigInt::from_bytes_be(num_bigint::Sign::Plus, bytes))
}
