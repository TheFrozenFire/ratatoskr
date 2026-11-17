//! Directory rows, and the configuration written once.

use alloy_primitives::Address;
use redb::{Durability, ReadableDatabase};
use store_model::{Configuration, DirectoryRow, Feed, Listing};

use super::*;

impl Feeds {
    /// Replace the directory rows. The caller has already parsed the document.
    pub fn apply_directory(&self, rows: &[DirectoryRow]) -> Result<Listing, Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        let generation = {
            let mut meta = write.open_table(META).map_err(db_err)?;
            let generation = meta
                .get(LISTING)
                .map_err(db_err)?
                .map(|value| value.value())
                .unwrap_or(0)
                + 1;
            meta.insert(LISTING, generation).map_err(db_err)?;
            generation
        };
        {
            let mut identity = write.open_table(IDENTITY).map_err(db_err)?;
            for row in rows {
                let value = encode_identity(row, generation)?;
                identity
                    .insert(row.proxy.as_slice(), value.as_slice())
                    .map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(Listing {
            generation,
            named: rows.len(),
        })
    }

    pub fn listing_generation(&self) -> Result<u64, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(db_err(err)),
        };
        Ok(table
            .get(LISTING)
            .map_err(db_err)?
            .map(|value| value.value())
            .unwrap_or(0))
    }

    pub fn feed(&self, proxy: Address) -> Result<Option<Feed>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(decode_identity(proxy, value.value())?))
    }

    pub fn proxies(&self) -> Result<Vec<Address>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut out = Vec::new();
        for item in table.iter().map_err(db_err)? {
            let (key, _) = item.map_err(db_err)?;
            let bytes = key.value();
            if bytes.len() != 20 {
                return Err(Error::Store("proxy key is short".into()));
            }
            out.push(Address::from_slice(bytes));
        }
        Ok(out)
    }

    pub fn configuration(&self, proxy: Address) -> Result<Option<Configuration>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(CONFIG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(decode_configuration(value.value())?))
    }
}

fn encode_identity(row: &DirectoryRow, listed_generation: u64) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.push(IDENTITY_VERSION);
    for text in [
        row.name.as_str(),
        row.path.as_str(),
        row.category.as_str(),
        row.feed_type.as_str(),
        row.multiply.as_str(),
        row.threshold.as_str(),
        row.base_asset.as_str(),
        row.quote_asset.as_str(),
        row.product_type.as_str(),
        row.product_type_code.as_str(),
        row.attribute_type.as_str(),
        row.market_hours.as_str(),
        row.shutdown_date.as_str(),
        row.clic_product_name.as_str(),
        row.svr_label.as_str(),
    ] {
        put_str(&mut out, text)?;
    }
    out.extend_from_slice(&row.contract_version.to_le_bytes());
    out.push(row.decimals);
    out.extend_from_slice(&row.heartbeat.to_le_bytes());
    out.push(u8::from(row.hidden));
    match row.secondary_proxy {
        None => out.push(0),
        Some(proxy) => {
            out.push(1);
            out.extend_from_slice(proxy.as_slice());
        }
    }
    out.extend_from_slice(row.directory_aggregator.as_slice());
    out.extend_from_slice(&listed_generation.to_le_bytes());
    Ok(out)
}

pub(super) fn decode_identity(proxy: Address, bytes: &[u8]) -> Result<Feed, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    let version = cursor.u8()?;
    if version != IDENTITY_VERSION {
        return Err(Error::Store("identity version".into()));
    }
    let name = cursor.string()?;
    let path = cursor.string()?;
    let category = cursor.string()?;
    let feed_type = cursor.string()?;
    let multiply = cursor.string()?;
    let threshold = cursor.string()?;
    let base_asset = cursor.string()?;
    let quote_asset = cursor.string()?;
    let product_type = cursor.string()?;
    let product_type_code = cursor.string()?;
    let attribute_type = cursor.string()?;
    let market_hours = cursor.string()?;
    let shutdown_date = cursor.string()?;
    let clic_product_name = cursor.string()?;
    let svr_label = cursor.string()?;
    let contract_version = cursor.u64()?;
    let decimals = cursor.u8()?;
    let heartbeat = cursor.u64()?;
    let hidden = cursor.u8()? == 1;
    let secondary_proxy = match cursor.u8()? {
        0 => None,
        1 => Some(cursor.address()?),
        _ => return Err(Error::Store("secondary proxy".into())),
    };
    let directory_aggregator = cursor.address()?;
    let listed_generation = cursor.u64()?;
    Ok(Feed {
        row: DirectoryRow {
            proxy,
            name,
            path,
            category,
            feed_type,
            contract_version,
            decimals,
            multiply,
            heartbeat,
            threshold,
            base_asset,
            quote_asset,
            product_type,
            product_type_code,
            attribute_type,
            market_hours,
            shutdown_date,
            clic_product_name,
            hidden,
            secondary_proxy,
            svr_label,
            directory_aggregator,
        },
        listed_generation,
    })
}

pub(super) fn encode_configuration(config: &Configuration) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.push(config.decimals);
    put_str(&mut out, &config.description)?;
    out.extend_from_slice(&config.version.to_be_bytes::<32>());
    Ok(out)
}

pub(super) fn decode_configuration(bytes: &[u8]) -> Result<Configuration, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    let decimals = cursor.u8()?;
    let description = cursor.string()?;
    let version = cursor.word()?;
    Ok(Configuration {
        decimals,
        description,
        version,
    })
}

impl super::FeedRead {
    pub fn page(&self, after: Option<&[u8]>, limit: usize) -> Result<Vec<super::FeedLine>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut page = Vec::new();
        let rows = match after {
            Some(after) => table.range::<&[u8]>(after..).map_err(db_err)?,
            None => table.range::<&[u8]>(..).map_err(db_err)?,
        };
        for item in rows {
            if page.len() == limit {
                break;
            }
            let (key, value) = item.map_err(db_err)?;
            if after.is_some_and(|cursor| key.value() == cursor) {
                continue;
            }
            if key.value().len() != 20 {
                continue;
            }
            let proxy = Address::from_slice(key.value());
            let feed = decode_identity(proxy, value.value())?;
            let latest = super::answer::latest_answer(&self.db, proxy)?;
            let history = super::phase::has_history(&self.db, proxy)?;
            page.push(super::FeedLine {
                feed,
                latest,
                history,
            });
        }
        Ok(page)
    }

    pub fn detail(&self, proxy: Address) -> Result<Option<super::FeedDetail>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        let feed = decode_identity(proxy, value.value())?;
        Ok(Some(super::FeedDetail {
            feed,
            phases: super::phase::phases_of(&self.db, proxy)?,
            answers: super::answer::answers_of(&self.db, proxy)?,
        }))
    }
}
