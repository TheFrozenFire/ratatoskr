//! Token rows stored beside pool identities.

use alloy_primitives::{Address, B256};
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashSet;
use store_model::{decode_token, encode_token, TokenRecord, METRIC};

use super::{Catalog, EntityRead, TOKENS};

/// Addresses named by a venue row that have no token record yet.
pub(super) const NEEDED: TableDefinition<&[u8], &[u8]> = TableDefinition::new("token_needed");

/// Backfill cursor. Absent until the first scan saves one. `done` ends the scan.
const TOKEN_SCAN: TableDefinition<&str, &[u8]> = TableDefinition::new("token_scan");

const SCAN_CURSOR: &str = "cursor";

const SCAN_DONE: &str = "done";

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
            let mut needed = write.open_table(NEEDED)?;
            for row in rows {
                let stored = match table.get(row.address.as_slice())? {
                    Some(value) => Some(decode_token(row.address, value.value())?),
                    None => None,
                };
                let merged = merge_token(stored, row);
                let bytes = encode_token(&merged);
                table.insert(row.address.as_slice(), bytes.as_slice())?;
                needed.remove(row.address.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Token addresses on venue rows that have no token record yet.
    ///
    /// The first pass walks venue identities once and remembers where it stopped.
    /// After that pass, and for identities written later, this reads `token_needed`.
    pub fn missing_tokens(&self, limit: usize) -> eyre::Result<Vec<Address>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if self.token_scan_done()? {
            return self.drain_needed(limit);
        }
        let have = self.token_set()?;
        let saved = self.token_cursor()?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        let mut kind = saved.as_ref().map(|cursor| cursor.kind).unwrap_or(1);
        let mut after = None;
        if let Some(cursor) = &saved {
            if cursor.index > 0 {
                if let Some(row) = self.read_venue(cursor.kind, cursor.id)? {
                    if let Some(index) = take_missing(
                        &row.tokens,
                        cursor.index as usize,
                        &have,
                        &mut seen,
                        &mut missing,
                        limit,
                    ) {
                        self.save_token_scan(
                            &missing,
                            ScanCursor {
                                kind: cursor.kind,
                                id: cursor.id,
                                index,
                            },
                        )?;
                        return Ok(missing);
                    }
                }
            }
            after = Some(cursor.id);
        }
        while kind <= METRIC {
            loop {
                let page = self.page_venues(kind, after, 512)?;
                if page.is_empty() {
                    break;
                }
                for row in &page {
                    if let Some(index) =
                        take_missing(&row.tokens, 0, &have, &mut seen, &mut missing, limit)
                    {
                        self.save_token_scan(
                            &missing,
                            ScanCursor {
                                kind,
                                id: row.id,
                                index,
                            },
                        )?;
                        return Ok(missing);
                    }
                    after = Some(row.id);
                }
                if page.len() < 512 {
                    break;
                }
            }
            kind = kind.saturating_add(1);
            after = None;
        }
        self.finish_token_scan(&missing)?;
        Ok(missing)
    }

    fn token_scan_done(&self) -> eyre::Result<bool> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKEN_SCAN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(SCAN_DONE)?.is_some())
    }

    fn token_cursor(&self) -> eyre::Result<Option<ScanCursor>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKEN_SCAN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let Some(value) = table.get(SCAN_CURSOR)? else {
            return Ok(None);
        };
        Ok(decode_cursor(value.value()))
    }

    fn save_token_scan(&self, missing: &[Address], cursor: ScanCursor) -> eyre::Result<()> {
        self.write_token_scan(missing, Some(cursor), false)
    }

    fn finish_token_scan(&self, missing: &[Address]) -> eyre::Result<()> {
        self.write_token_scan(missing, None, true)
    }

    fn write_token_scan(
        &self,
        missing: &[Address],
        cursor: Option<ScanCursor>,
        done: bool,
    ) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut needed = write.open_table(NEEDED)?;
            let mut scan = write.open_table(TOKEN_SCAN)?;
            for token in missing {
                needed.insert(token.as_slice(), &[][..])?;
            }
            match cursor {
                Some(cursor) => {
                    scan.insert(SCAN_CURSOR, encode_cursor(&cursor).as_slice())?;
                }
                None => {
                    scan.remove(SCAN_CURSOR)?;
                }
            }
            if done {
                scan.insert(SCAN_DONE, &[][..])?;
            }
        }
        write.commit()?;
        Ok(())
    }

    fn drain_needed(&self, limit: usize) -> eyre::Result<Vec<Address>> {
        let read = self.entities.begin_read()?;
        let needed = match read.open_table(NEEDED) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let tokens = match read.open_table(TOKENS) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(err.into()),
        };
        let mut missing = Vec::new();
        let mut stale = Vec::new();
        for item in needed.iter()? {
            if missing.len() == limit {
                break;
            }
            let (key, _) = item?;
            let key = key.value();
            if key.len() != 20 {
                continue;
            }
            let known = match &tokens {
                Some(table) => table.get(key)?.is_some(),
                None => false,
            };
            if known {
                stale.push(key.to_vec());
                continue;
            }
            missing.push(Address::from_slice(key));
        }
        drop(needed);
        drop(tokens);
        drop(read);
        if !stale.is_empty() {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            {
                let mut needed = write.open_table(NEEDED)?;
                for key in stale {
                    needed.remove(key.as_slice())?;
                }
            }
            write.commit()?;
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
    pub fn token_count(&self) -> eyre::Result<u64> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKENS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        Ok(table.len()?)
    }

    pub fn page_tokens(
        &self,
        after: Option<Address>,
        limit: usize,
    ) -> eyre::Result<Vec<TokenRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TOKENS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut page = Vec::new();
        for item in table.iter()? {
            if page.len() == limit {
                break;
            }
            let (key, value) = item?;
            let key = key.value();
            if key.len() != 20 {
                continue;
            }
            let address = Address::from_slice(key);
            if after.is_some_and(|cursor| address <= cursor) {
                continue;
            }
            page.push(decode_token(address, value.value())?);
        }
        Ok(page)
    }

    pub fn token(&self, address: Address) -> eyre::Result<Option<TokenRecord>> {
        read_token_row(&self.entities, address)
    }

    pub fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        read_token_rows(&self.entities)
    }
}

struct ScanCursor {
    kind: u8,
    id: B256,
    index: u16,
}

fn encode_cursor(cursor: &ScanCursor) -> [u8; 35] {
    let mut out = [0_u8; 35];
    out[0] = cursor.kind;
    out[1..33].copy_from_slice(cursor.id.as_slice());
    out[33..].copy_from_slice(&cursor.index.to_le_bytes());
    out
}

fn decode_cursor(bytes: &[u8]) -> Option<ScanCursor> {
    if bytes.len() != 35 {
        return None;
    }
    let mut id = [0_u8; 32];
    id.copy_from_slice(&bytes[1..33]);
    Some(ScanCursor {
        kind: bytes[0],
        id: B256::from(id),
        index: u16::from_le_bytes([bytes[33], bytes[34]]),
    })
}

/// `Some(index)` is the next token offset when `limit` is filled.
fn take_missing(
    tokens: &[Address],
    start: usize,
    have: &HashSet<Address>,
    seen: &mut HashSet<Address>,
    missing: &mut Vec<Address>,
    limit: usize,
) -> Option<u16> {
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if *token == Address::ZERO || have.contains(token) || !seen.insert(*token) {
            continue;
        }
        missing.push(*token);
        if missing.len() == limit {
            return Some(u16::try_from(index.saturating_add(1)).unwrap_or(u16::MAX));
        }
    }
    None
}

/// Queue venue tokens that have no record yet. The caller holds the entity write.
pub(super) fn queue_needed(
    tokens: &redb::Table<'_, &[u8], &[u8]>,
    needed: &mut redb::Table<'_, &[u8], &[u8]>,
    addresses: &[Address],
) -> eyre::Result<()> {
    for token in addresses {
        if *token == Address::ZERO {
            continue;
        }
        if tokens.get(token.as_slice())?.is_some() {
            continue;
        }
        needed.insert(token.as_slice(), &[][..])?;
    }
    Ok(())
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
    token_row(&read, address)
}

pub(crate) fn token_row(
    read: &redb::ReadTransaction,
    address: Address,
) -> eyre::Result<Option<TokenRecord>> {
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
    token_rows(&read)
}

pub(crate) fn token_rows(read: &redb::ReadTransaction) -> eyre::Result<Vec<TokenRecord>> {
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
