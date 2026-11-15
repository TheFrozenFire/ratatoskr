//! Stored answers.

use alloy_primitives::{Address, I256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::{Answer, ObservedAnswer};

use super::*;

impl Feeds {
    pub fn answers(&self, proxy: Address) -> Result<Vec<Answer>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(ANSWERS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let start = answer_key(proxy, 0, 0);
        let end = answer_key(proxy, u16::MAX, u64::MAX);
        let mut out = Vec::new();
        for item in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(db_err)?
        {
            let (key, value) = item.map_err(db_err)?;
            out.push(decode_answer(key.value(), value.value())?);
        }
        Ok(out)
    }

    /// Count stored answers once so later commits can add to that total.
    pub fn remember_answers(&self) -> Result<u64, Error> {
        let count = {
            let read = self.db.begin_read().map_err(db_err)?;
            if let Ok(meta) = read.open_table(META) {
                if let Some(count) = meta.get(ANSWER_COUNT).map_err(db_err)? {
                    return Ok(count.value());
                }
            }
            count_rows(&read, ANSWERS)?
        };
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut meta = write.open_table(META).map_err(db_err)?;
            if meta.get(ANSWER_COUNT).map_err(db_err)?.is_none() {
                meta.insert(ANSWER_COUNT, count).map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(count)
    }
}

pub(super) fn answer_key(proxy: Address, phase_id: u16, round_id: u64) -> [u8; 30] {
    let mut key = [0u8; 30];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..22].copy_from_slice(&phase_id.to_be_bytes());
    key[22..].copy_from_slice(&round_id.to_be_bytes());
    key
}

pub(super) fn encode_answer(observed: &ObservedAnswer) -> [u8; 56] {
    let mut out = [0u8; 56];
    out[..32].copy_from_slice(&observed.value.to_be_bytes::<32>());
    out[32..40].copy_from_slice(&observed.updated_at.to_le_bytes());
    out[40..48].copy_from_slice(&observed.block.to_le_bytes());
    out[48..].copy_from_slice(&observed.log_index.to_le_bytes());
    out
}

fn decode_answer(key: &[u8], value: &[u8]) -> Result<Answer, Error> {
    if key.len() != 30 || value.len() != 56 {
        return Err(Error::Store("answer row".into()));
    }
    let mut round = [0u8; 8];
    round.copy_from_slice(&key[22..30]);
    let mut word = [0u8; 32];
    word.copy_from_slice(&value[..32]);
    let mut updated = [0u8; 8];
    updated.copy_from_slice(&value[32..40]);
    let mut block = [0u8; 8];
    block.copy_from_slice(&value[40..48]);
    let mut log_index = [0u8; 8];
    log_index.copy_from_slice(&value[48..56]);
    Ok(Answer {
        proxy: Address::from_slice(&key[..20]),
        phase_id: u16::from_be_bytes([key[20], key[21]]),
        round_id: u64::from_be_bytes(round),
        value: I256::from_be_bytes(word),
        updated_at: u64::from_le_bytes(updated),
        block: u64::from_le_bytes(block),
        log_index: u64::from_le_bytes(log_index),
    })
}
