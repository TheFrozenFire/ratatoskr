//! Open the three catalog files and set each page cache.

use eyre::WrapErr;
use redb::{Database, ReadableDatabase};
use std::path::{Path, PathBuf};

use super::migrate::migrate;
use super::{
    catalog_dir, Catalog, EntityRead, DEFAULT_CACHE, DISCOVERY_CACHE, ENTITIES_CACHE, READ_CACHE,
    WORK_CACHE,
};

impl Catalog {
    /// Open the catalog directory.
    ///
    /// A path whose file name ends in `.redb` selects that file's parent, so
    /// existing callers that pass `venues.redb` keep the same directory.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let dir = catalog_dir(path);
        std::fs::create_dir_all(&dir)?;
        migrate(&dir)?;
        Ok(Self {
            discovery: open_db(&dir.join("discovery.redb"))?,
            entities: open_db(&dir.join("entities.redb"))?,
            work: open_db(&dir.join("work.redb"))?,
        })
    }

    /// Page caches for the three catalog files.
    pub fn cache_readings(&self) -> Vec<ipc::db::CacheReading> {
        vec![
            ipc::db::reading(
                "discovery.redb",
                DISCOVERY_CACHE as u64,
                self.discovery.cache_stats(),
            ),
            ipc::db::reading(
                "entities.redb",
                ENTITIES_CACHE as u64,
                self.entities.cache_stats(),
            ),
            ipc::db::reading("work.redb", WORK_CACHE as u64, self.work.cache_stats()),
        ]
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr/entities.redb")
    }
}

impl EntityRead {
    /// Open `entities.redb` in the catalog directory.
    ///
    /// A path whose file name ends in `.redb` selects that file's parent.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let file = catalog_dir(path).join("entities.redb");
        let mut builder = redb::Builder::new();
        builder.set_cache_size(READ_CACHE);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let entities = builder
            .open_read_only(&file)
            .wrap_err_with(|| format!("open {} read-only", file.display()))?;
        Ok(Self { entities })
    }
}

/// Page cache for one catalog file.
///
/// Entities serves random pool reads. Discovery walks hit lists. Work stores
/// leases. A sequential copy of any other file keeps a small cache.
fn cache_bytes(path: &Path) -> usize {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("entities.redb" | "venues.redb") => ENTITIES_CACHE,
        Some("discovery.redb") => DISCOVERY_CACHE,
        Some("work.redb") => WORK_CACHE,
        _ => DEFAULT_CACHE,
    }
}

pub(super) fn open_at(path: &Path, create: bool) -> Result<Database, redb::DatabaseError> {
    let mut builder = redb::Builder::new();
    builder.set_cache_size(cache_bytes(path));
    builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
    if create {
        builder.create(path)
    } else {
        builder.open(path)
    }
}

pub(super) fn open_db(path: &Path) -> eyre::Result<Database> {
    let create = !path.exists();
    let verb = if create { "create" } else { "open" };
    open_at(path, create).wrap_err_with(|| format!("{verb} {}", path.display()))
}
