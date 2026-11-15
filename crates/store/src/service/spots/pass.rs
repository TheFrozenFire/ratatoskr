//! One pass over the spot table.

use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::Spot;

use super::row::{decode, encode, key_of};
use super::{db_err, Error, Spots, SPOTS};

impl Spots {
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
