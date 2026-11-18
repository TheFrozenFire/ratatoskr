//! One pass over the spot table.

use std::collections::BTreeMap;

use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::Spot;

use super::row::{decode, encode, key_of};
use super::{db_err, Error, Spots, SPOTS};

impl Spots {
    /// Write inserts, updates, and deletes. An identical pass does not commit.
    pub fn replace(&self, spots: &[Spot]) -> Result<(), Error> {
        let incoming = encode_spots(spots);
        let stored = self.stored_rows()?;
        let remove: Vec<[u8; 40]> = stored
            .keys()
            .filter(|key| !incoming.contains_key(*key))
            .copied()
            .collect();
        let upsert: Vec<([u8; 40], Vec<u8>)> = incoming
            .iter()
            .filter(|(key, value)| stored.get(*key) != Some(value))
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        if remove.is_empty() && upsert.is_empty() {
            return Ok(());
        }
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut table = write.open_table(SPOTS).map_err(db_err)?;
            for key in &remove {
                table.remove(key.as_slice()).map_err(db_err)?;
            }
            for (key, value) in &upsert {
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }

    fn stored_rows(&self) -> Result<BTreeMap<[u8; 40], Vec<u8>>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(SPOTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut stored = BTreeMap::new();
        for item in table.iter().map_err(db_err)? {
            let (key, value) = item.map_err(db_err)?;
            let raw = key.value();
            if raw.len() != 40 {
                return Err(Error::Store(
                    "spot key is not a token and a numeraire".into(),
                ));
            }
            let mut owned = [0_u8; 40];
            owned.copy_from_slice(raw);
            stored.insert(owned, value.value().to_vec());
        }
        Ok(stored)
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

fn encode_spots(spots: &[Spot]) -> BTreeMap<[u8; 40], Vec<u8>> {
    let mut incoming = BTreeMap::new();
    for spot in spots {
        incoming.insert(key_of(spot), encode(spot));
    }
    incoming
}
