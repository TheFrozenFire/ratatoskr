//! Token rows stored beside pool identities.

use alloy_primitives::Address;
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata};
use std::collections::HashSet;
use store_model::{decode_token, encode_token, TokenRecord, METRIC};

use super::{Catalog, EntityRead, TOKENS};

impl Catalog {
    pub fn token(&self, address: Address) -> eyre::Result<Option<TokenRecord>> {
        read_token_row(&self.entities, address)
    }

    pub fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        read_token_rows(&self.entities)
    }

    /// Row count stored on the tree header. Walking the table reads every leaf.
    pub fn token_count(&self) -> eyre::Result<u64> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKENS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        Ok(table.len()?)
    }

    /// Write token rows. An empty symbol leaves a stored symbol and its decimals.
    pub fn put_tokens(&self, rows: &[TokenRecord]) -> eyre::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(TOKENS)?;
            for row in rows {
                let stored = match table.get(row.address.as_slice())? {
                    Some(value) => Some(decode_token(row.address, value.value())?),
                    None => None,
                };
                let merged = merge_token(stored, row);
                let bytes = encode_token(&merged);
                table.insert(row.address.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Token addresses on venue rows that have no token record yet.
    pub fn missing_tokens(&self, limit: usize) -> eyre::Result<Vec<Address>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let have = self.token_set()?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for kind in 1..=METRIC {
            let mut after = None;
            loop {
                let page = self.page_venues(kind, after, 512)?;
                if page.is_empty() {
                    break;
                }
                after = page.last().map(|row| row.id);
                for row in &page {
                    for token in &row.tokens {
                        if *token == Address::ZERO || have.contains(token) || !seen.insert(*token) {
                            continue;
                        }
                        missing.push(*token);
                        if missing.len() == limit {
                            return Ok(missing);
                        }
                    }
                }
                if page.len() < 512 {
                    break;
                }
            }
        }
        Ok(missing)
    }

    fn token_set(&self) -> eyre::Result<HashSet<Address>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKENS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashSet::new()),
            Err(err) => return Err(err.into()),
        };
        let mut have = HashSet::new();
        for item in table.iter()? {
            let (key, _) = item?;
            let key = key.value();
            if key.len() == 20 {
                have.insert(Address::from_slice(key));
            }
        }
        Ok(have)
    }
}

impl EntityRead {
    pub fn token(&self, address: Address) -> eyre::Result<Option<TokenRecord>> {
        read_token_row(&self.entities, address)
    }

    pub fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        read_token_rows(&self.entities)
    }
}

fn merge_token(stored: Option<TokenRecord>, incoming: &TokenRecord) -> TokenRecord {
    let Some(stored) = stored else {
        return incoming.clone();
    };
    let (symbol, decimals) = if incoming.symbol.is_empty() {
        (stored.symbol, stored.decimals)
    } else {
        (incoming.symbol.clone(), incoming.decimals)
    };
    TokenRecord {
        address: incoming.address,
        symbol,
        decimals,
        wraps: incoming.wraps.or(stored.wraps),
    }
}

pub(crate) fn read_token_row(
    db: &impl crate::storage::Handle,
    address: Address,
) -> eyre::Result<Option<TokenRecord>> {
    let read = db.begin_read()?;
    let table = match read.open_table(TOKENS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    match table.get(address.as_slice())? {
        Some(value) => Ok(Some(decode_token(address, value.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn read_token_rows(db: &impl crate::storage::Handle) -> eyre::Result<Vec<TokenRecord>> {
    let read = db.begin_read()?;
    let table = match read.open_table(TOKENS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut rows = Vec::new();
    for item in table.iter()? {
        let (key, value) = item?;
        let key = key.value();
        if key.len() != 20 {
            return Err(eyre::eyre!("token key"));
        }
        rows.push(decode_token(Address::from_slice(key), value.value())?);
    }
    Ok(rows)
}
