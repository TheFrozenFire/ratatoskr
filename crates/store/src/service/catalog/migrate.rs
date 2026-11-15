//! Move a legacy `venues.redb` onto the three catalog files.

use eyre::WrapErr;
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

use super::open::{open_at, open_db};
use super::{BODIES, CURSORS, HITS, HIT_SCAN};

/// Move a legacy `venues.redb` onto the three-file layout.
///
/// Identities stay where they are: the file is renamed to `entities.redb`.
/// Crawl tables are copied into `discovery.redb` and then removed. When
/// `entities.redb` is already present, `venues.redb` is not opened.
pub(super) fn migrate(dir: &Path) -> eyre::Result<()> {
    let venues = dir.join("venues.redb");
    let entities = dir.join("entities.redb");
    let discovery = dir.join("discovery.redb");
    if entities.exists() {
        // `venues.redb` beside an existing `entities.redb` is the pre-migration
        // file. Leave it closed so a second open cannot copy it again.
        if venues.exists() {
            return Ok(());
        }
        if !discovery.exists() {
            let from = open_at(&entities, false).wrap_err("open entities for crawl copy")?;
            let to = open_db(&discovery)?;
            copy_crawl(&from, &to)?;
            drop(from);
        }
        let entities = open_at(&entities, false).wrap_err("open entities")?;
        delete_crawl(&entities)?;
        return Ok(());
    }
    if !venues.exists() {
        return Ok(());
    }
    let from = open_at(&venues, false).wrap_err("open venues.redb")?;
    let to = open_db(&discovery)?;
    copy_crawl(&from, &to)?;
    drop(from);
    drop(to);
    std::fs::rename(&venues, &entities).wrap_err("rename venues.redb to entities.redb")?;
    let entities = open_at(&entities, false).wrap_err("open renamed entities")?;
    delete_crawl(&entities)?;
    Ok(())
}

fn copy_crawl(from: &Database, to: &Database) -> eyre::Result<()> {
    copy_str_u64(from, to, CURSORS)?;
    copy_str_u64(from, to, HIT_SCAN)?;
    copy_bytes(from, to, HITS)?;
    copy_u64_bytes(from, to, BODIES)?;
    Ok(())
}

fn copy_str_u64(
    from: &Database,
    to: &Database,
    table: TableDefinition<&str, u64>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn copy_bytes(
    from: &Database,
    to: &Database,
    table: TableDefinition<&[u8], &[u8]>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn copy_u64_bytes(
    from: &Database,
    to: &Database,
    table: TableDefinition<u64, &[u8]>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn delete_crawl(db: &Database) -> eyre::Result<()> {
    let mut write = db.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    write
        .delete_table(CURSORS)
        .map_err(|err| eyre::eyre!("delete cursors: {err}"))?;
    write
        .delete_table(HITS)
        .map_err(|err| eyre::eyre!("delete hits: {err}"))?;
    write
        .delete_table(HIT_SCAN)
        .map_err(|err| eyre::eyre!("delete hit scan: {err}"))?;
    write
        .delete_table(BODIES)
        .map_err(|err| eyre::eyre!("delete bodies: {err}"))?;
    write.commit()?;
    Ok(())
}
