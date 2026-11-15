//! Proxy phases, history jobs, and mismatches.

use alloy_primitives::{Address, I256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::{Configuration, HistoryJob, Mismatch, Phase, ProxyHead, Read};

use super::directory::{decode_configuration, encode_configuration};
use super::*;

impl Feeds {
    pub fn phases(&self, proxy: Address) -> Result<Vec<Phase>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(PHASES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let phases = read_phases(phase_rows(&table, proxy)?)?;
        Ok(phases)
    }

    pub fn history_jobs(&self) -> Result<Vec<HistoryJob>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut out = Vec::new();
        for item in table.iter().map_err(db_err)? {
            let (key, value) = item.map_err(db_err)?;
            let (proxy, phase_id) = split_phase_key(key.value())?;
            let aggregator = address_of(value.value())?;
            out.push(HistoryJob {
                proxy,
                phase_id,
                aggregator,
            });
        }
        Ok(out)
    }

    pub fn mismatches(&self, proxy: Address) -> Result<Vec<Mismatch>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(MISMATCHES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let start = mismatch_key(proxy, 0);
        let end = mismatch_key(proxy, u32::MAX);
        let mut out = Vec::new();
        for item in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(db_err)?
        {
            let (_, value) = item.map_err(db_err)?;
            out.push(decode_mismatch(value.value())?);
        }
        Ok(out)
    }

    pub fn failure(&self, proxy: Address) -> Result<Option<String>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(FAILURES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(value.value())
            .map_err(|_| Error::Store("failure is not utf-8".into()))?;
        Ok(Some(text.to_string()))
    }

    pub fn apply_read(&self, proxy: Address, read: Read) -> Result<(), Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let identity = write.open_table(IDENTITY).map_err(db_err)?;
            if identity.get(proxy.as_slice()).map_err(db_err)?.is_none() {
                return Err(Error::Unknown { proxy });
            }
            let mut failures = write.open_table(FAILURES).map_err(db_err)?;
            match read {
                Read::Failed(text) => {
                    failures
                        .insert(proxy.as_slice(), text.as_bytes())
                        .map_err(db_err)?;
                }
                Read::Configured {
                    head,
                    min_answer,
                    max_answer,
                } => {
                    failures.remove(proxy.as_slice()).map_err(db_err)?;
                    drop(failures);
                    drop(identity);
                    let mut config = write.open_table(CONFIG).map_err(db_err)?;
                    let mut mismatches = write.open_table(MISMATCHES).map_err(db_err)?;
                    let existing = match config.get(proxy.as_slice()).map_err(db_err)? {
                        None => None,
                        Some(stored) => Some(decode_configuration(stored.value())?),
                    };
                    match existing {
                        None => {
                            let value = encode_configuration(&Configuration {
                                decimals: head.decimals,
                                description: head.description.clone(),
                                version: head.version,
                            })?;
                            config
                                .insert(proxy.as_slice(), value.as_slice())
                                .map_err(db_err)?;
                        }
                        Some(stored)
                            if stored.decimals != head.decimals
                                || stored.description != head.description
                                || stored.version != head.version =>
                        {
                            let mismatch = Mismatch::Configuration {
                                stored_decimals: stored.decimals,
                                observed_decimals: head.decimals,
                                stored_description: stored.description,
                                observed_description: head.description.clone(),
                                stored_version: stored.version,
                                observed_version: head.version,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        Some(_) => {}
                    }
                    drop(config);
                    let mut phases = write.open_table(PHASES).map_err(db_err)?;
                    let mut jobs = write.open_table(JOBS).map_err(db_err)?;
                    let stored = read_phases(phase_rows(&phases, proxy)?)?;
                    match stored.last() {
                        Some(phase)
                            if phase.phase_id == head.phase_id
                                && phase.aggregator == head.aggregator => {}
                        Some(phase) if phase.phase_id == head.phase_id => {
                            let mismatch = Mismatch::Aggregator {
                                phase_id: head.phase_id,
                                stored: phase.aggregator,
                                observed: head.aggregator,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        Some(phase) if head.phase_id > phase.phase_id => {
                            insert_phase(
                                &mut phases,
                                &mut jobs,
                                proxy,
                                &head,
                                min_answer,
                                max_answer,
                            )?;
                        }
                        Some(phase) => {
                            let mismatch = Mismatch::Phase {
                                newest: phase.phase_id,
                                observed: head.phase_id,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        None => {
                            insert_phase(
                                &mut phases,
                                &mut jobs,
                                proxy,
                                &head,
                                min_answer,
                                max_answer,
                            )?;
                        }
                    }
                }
            }
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }
}

fn insert_phase(
    phases: &mut redb::Table<&[u8], &[u8]>,
    jobs: &mut redb::Table<&[u8], &[u8]>,
    proxy: Address,
    head: &ProxyHead,
    min_answer: Option<I256>,
    max_answer: Option<I256>,
) -> Result<(), Error> {
    let key = phase_key(proxy, head.phase_id);
    let value = encode_phase(head.aggregator, min_answer, max_answer);
    phases
        .insert(key.as_slice(), value.as_slice())
        .map_err(db_err)?;
    jobs.insert(key.as_slice(), head.aggregator.as_slice())
        .map_err(db_err)?;
    Ok(())
}

fn insert_mismatch(
    table: &mut redb::Table<&[u8], &[u8]>,
    proxy: Address,
    mismatch: &Mismatch,
) -> Result<(), Error> {
    let start = mismatch_key(proxy, 0);
    let end = mismatch_key(proxy, u32::MAX);
    let mut seq = 0u32;
    for item in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(db_err)?
    {
        let (key, _) = item.map_err(db_err)?;
        let (_, stored) = split_mismatch_key(key.value())?;
        seq = seq.max(stored.saturating_add(1));
    }
    let value = encode_mismatch(mismatch)?;
    table
        .insert(mismatch_key(proxy, seq).as_slice(), value.as_slice())
        .map_err(db_err)?;
    Ok(())
}

fn read_phases<'g, I>(rows: I) -> Result<Vec<Phase>, Error>
where
    I: Iterator<
        Item = Result<
            (
                redb::AccessGuard<'g, &'static [u8]>,
                redb::AccessGuard<'g, &'static [u8]>,
            ),
            redb::StorageError,
        >,
    >,
{
    let mut out = Vec::new();
    for item in rows {
        let (key, value) = item.map_err(db_err)?;
        let (_, phase_id) = split_phase_key(key.value())?;
        let (aggregator, min_answer, max_answer) = decode_phase(value.value())?;
        out.push(Phase {
            phase_id,
            aggregator,
            min_answer,
            max_answer,
        });
    }
    Ok(out)
}

fn phase_rows<'a, T>(
    table: &'a T,
    proxy: Address,
) -> Result<
    impl Iterator<
            Item = Result<
                (
                    redb::AccessGuard<'a, &'static [u8]>,
                    redb::AccessGuard<'a, &'static [u8]>,
                ),
                redb::StorageError,
            >,
        > + 'a,
    Error,
>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    let start = phase_key(proxy, 0);
    let end = phase_key(proxy, u16::MAX);
    table
        .range(start.as_slice()..=end.as_slice())
        .map_err(db_err)
}

fn encode_phase(
    aggregator: Address,
    min_answer: Option<I256>,
    max_answer: Option<I256>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(21 + 64);
    out.extend_from_slice(aggregator.as_slice());
    let mut flags = 0u8;
    if min_answer.is_some() {
        flags |= 1;
    }
    if max_answer.is_some() {
        flags |= 2;
    }
    out.push(flags);
    if let Some(min_answer) = min_answer {
        out.extend_from_slice(&min_answer.to_be_bytes::<32>());
    }
    if let Some(max_answer) = max_answer {
        out.extend_from_slice(&max_answer.to_be_bytes::<32>());
    }
    out
}

pub(super) fn decode_phase(bytes: &[u8]) -> Result<(Address, Option<I256>, Option<I256>), Error> {
    if bytes.len() < 21 {
        return Err(Error::Store("phase is short".into()));
    }
    let aggregator = Address::from_slice(&bytes[..20]);
    let flags = bytes[20];
    let mut at = 21;
    let min_answer = if flags & 1 == 1 {
        Some(take_i256(bytes, &mut at)?)
    } else {
        None
    };
    let max_answer = if flags & 2 == 2 {
        Some(take_i256(bytes, &mut at)?)
    } else {
        None
    };
    Ok((aggregator, min_answer, max_answer))
}

fn encode_mismatch(mismatch: &Mismatch) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    match mismatch {
        Mismatch::Configuration {
            stored_decimals,
            observed_decimals,
            stored_description,
            observed_description,
            stored_version,
            observed_version,
        } => {
            out.push(1);
            out.push(*stored_decimals);
            out.push(*observed_decimals);
            put_str(&mut out, stored_description)?;
            put_str(&mut out, observed_description)?;
            out.extend_from_slice(&stored_version.to_be_bytes::<32>());
            out.extend_from_slice(&observed_version.to_be_bytes::<32>());
        }
        Mismatch::Phase { newest, observed } => {
            out.push(2);
            out.extend_from_slice(&newest.to_be_bytes());
            out.extend_from_slice(&observed.to_be_bytes());
        }
        Mismatch::Aggregator {
            phase_id,
            stored,
            observed,
        } => {
            out.push(3);
            out.extend_from_slice(&phase_id.to_be_bytes());
            out.extend_from_slice(stored.as_slice());
            out.extend_from_slice(observed.as_slice());
        }
    }
    Ok(out)
}

fn decode_mismatch(bytes: &[u8]) -> Result<Mismatch, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    match cursor.u8()? {
        1 => {
            let stored_decimals = cursor.u8()?;
            let observed_decimals = cursor.u8()?;
            let stored_description = cursor.string()?;
            let observed_description = cursor.string()?;
            let stored_version = cursor.word()?;
            let observed_version = cursor.word()?;
            Ok(Mismatch::Configuration {
                stored_decimals,
                observed_decimals,
                stored_description,
                observed_description,
                stored_version,
                observed_version,
            })
        }
        2 => Ok(Mismatch::Phase {
            newest: cursor.u16()?,
            observed: cursor.u16()?,
        }),
        3 => Ok(Mismatch::Aggregator {
            phase_id: cursor.u16()?,
            stored: cursor.address()?,
            observed: cursor.address()?,
        }),
        _ => Err(Error::Store("mismatch tag".into())),
    }
}

fn take_i256(bytes: &[u8], at: &mut usize) -> Result<I256, Error> {
    let end = at
        .checked_add(32)
        .ok_or_else(|| Error::Store("phase bound".into()))?;
    if end > bytes.len() {
        return Err(Error::Store("phase bound".into()));
    }
    let mut word = [0u8; 32];
    word.copy_from_slice(&bytes[*at..end]);
    *at = end;
    Ok(I256::from_be_bytes(word))
}

fn address_of(bytes: &[u8]) -> Result<Address, Error> {
    if bytes.len() != 20 {
        return Err(Error::Store("aggregator is short".into()));
    }
    Ok(Address::from_slice(bytes))
}

fn phase_key(proxy: Address, phase_id: u16) -> [u8; 22] {
    let mut key = [0u8; 22];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..].copy_from_slice(&phase_id.to_be_bytes());
    key
}

pub(super) fn split_phase_key(bytes: &[u8]) -> Result<(Address, u16), Error> {
    if bytes.len() != 22 {
        return Err(Error::Store("phase key".into()));
    }
    let phase_id = u16::from_be_bytes([bytes[20], bytes[21]]);
    Ok((Address::from_slice(&bytes[..20]), phase_id))
}

fn mismatch_key(proxy: Address, seq: u32) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..].copy_from_slice(&seq.to_be_bytes());
    key
}

fn split_mismatch_key(bytes: &[u8]) -> Result<(Address, u32), Error> {
    if bytes.len() != 24 {
        return Err(Error::Store("mismatch key".into()));
    }
    let seq = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Ok((Address::from_slice(&bytes[..20]), seq))
}
