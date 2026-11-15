//! List cursors, bloom hits, receipt bodies, and notes.

use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::{decode_body, encode_body, Put, ReceiptBody, VenueRow};

use super::identity::write_identities;
use super::{Catalog, BODIES, CURSORS, HITS, HIT_SCAN, NOTES};

impl Catalog {
    pub fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(CURSORS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(job)?.map(|value| value.value()))
    }

    /// Write identities, then set `job` so the next read starts at `next`.
    ///
    /// The two files commit separately. A crash after the identities and
    /// before the cursor runs this chunk again. Merge is idempotent.
    pub fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        if let Some(stored) = self.cursor(job)? {
            if next < stored {
                return Err(eyre::eyre!("cursor {job} is {stored}; {next} is below it"));
            }
        }
        let written = write_identities(&self.entities, rows, put)?;
        let mut write = self.discovery.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut cursors = write.open_table(CURSORS)?;
            if let Some(stored) = cursors.get(job)?.map(|value| value.value()) {
                if next < stored {
                    return Err(eyre::eyre!("cursor {job} is {stored}; {next} is below it"));
                }
            }
            cursors.insert(job, next)?;
        }
        write.commit()?;
        Ok(written)
    }

    /// Next block `job` has already tested against the header blooms.
    pub fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(HIT_SCAN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(job)?.map(|value| value.value()))
    }

    /// Bloom hits in `[from, to)`.
    pub fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        self.hits_from_limit(job, from, to, usize::MAX)
    }

    /// At most `limit` bloom hits in `[from, to)`, in key order.
    ///
    /// The range stops once `limit` rows have been read.
    pub fn hits_from_limit(
        &self,
        job: &str,
        from: u64,
        to: u64,
        limit: usize,
    ) -> eyre::Result<Vec<u64>> {
        if from >= to || limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(HITS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let start = hit_key(job, from);
        let end = hit_key(job, to);
        let mut hits = Vec::new();
        for item in table.range(start.as_slice()..end.as_slice())? {
            if hits.len() == limit {
                break;
            }
            let (key, _) = item?;
            hits.push(block_of_hit_key(job, key.value())?);
        }
        Ok(hits)
    }

    /// Store `hits` and remember that every block before `scanned_to` was tested.
    ///
    /// A later call with a smaller `scanned_to` does not rewind the scan.
    pub fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        let mut write = self.discovery.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(HITS)?;
            let mut scan = write.open_table(HIT_SCAN)?;
            for block in hits {
                let key = hit_key(job, *block);
                table.insert(key.as_slice(), &[1u8][..])?;
            }
            let previous = scan.get(job)?.map(|value| value.value()).unwrap_or(0);
            if scanned_to > previous {
                scan.insert(job, scanned_to)?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(BODIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(numbers.iter().map(|_| None).collect());
            }
            Err(err) => return Err(err.into()),
        };
        let mut out = Vec::with_capacity(numbers.len());
        for number in numbers {
            let body = match table.get(*number)? {
                Some(value) => Some(decode_body(value.value())?),
                None => None,
            };
            out.push(body);
        }
        Ok(out)
    }

    pub fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        if bodies.is_empty() {
            return Ok(());
        }
        let mut write = self.discovery.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(BODIES)?;
            for (number, body) in bodies {
                let bytes = encode_body(body);
                table.insert(*number, bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn note(&self, key: &str) -> eyre::Result<Option<u64>> {
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(NOTES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(key)?.map(|value| value.value()))
    }

    pub fn put_note(&self, key: &str, value: u64) -> eyre::Result<()> {
        let mut write = self.discovery.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut notes = write.open_table(NOTES)?;
            notes.insert(key, value)?;
        }
        write.commit()?;
        Ok(())
    }
}

fn hit_key(job: &str, block: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(job.len() + 1 + 8);
    key.extend_from_slice(job.as_bytes());
    key.push(0xff);
    key.extend_from_slice(&block.to_be_bytes());
    key
}

fn block_of_hit_key(job: &str, key: &[u8]) -> eyre::Result<u64> {
    let prefix = job.len() + 1;
    if key.len() != prefix + 8 || &key[..job.len()] != job.as_bytes() || key[job.len()] != 0xff {
        return Err(eyre::eyre!("hit key"));
    }
    Ok(u64::from_be_bytes(key[prefix..].try_into().unwrap()))
}
