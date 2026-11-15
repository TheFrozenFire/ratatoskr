//! redb file handles.
//!
//! A service names the file and the page cache. This module creates the parent
//! directory, opens the file, and keeps the handle. One process writes each
//! file. A reader that follows a writer opens the file read-only. Table reads
//! and writes stay with the service.

use eyre::WrapErr;
use redb::ReadableDatabase;
use std::path::Path;

/// A redb file this process writes.
pub struct Database {
    inner: redb::Database,
    cache: u64,
    label: String,
}

/// A file this crate opened. Table code starts a read through the redb handle.
pub trait Handle: std::ops::Deref<Target: redb::ReadableDatabase> {}

impl<T> Handle for T where T: std::ops::Deref<Target: redb::ReadableDatabase> {}

/// A redb file this process only reads.
pub struct ReadDatabase {
    inner: redb::ReadOnlyDatabase,
    cache: u64,
    label: String,
}

impl Database {
    /// Create the file. The parent directory is created when it is missing.
    pub fn create(path: &Path, cache: usize) -> eyre::Result<Self> {
        ensure_parent(path)?;
        open_writer(path, cache, true)
    }

    /// Open a file that already exists.
    pub fn open(path: &Path, cache: usize) -> eyre::Result<Self> {
        open_writer(path, cache, false)
    }

    /// Open the file, or create it when it is missing.
    pub fn open_or_create(path: &Path, cache: usize) -> eyre::Result<Self> {
        if path.exists() {
            Self::open(path, cache)
        } else {
            Self::create(path, cache)
        }
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> ipc::db::CacheReading {
        ipc::db::reading(&self.label, self.cache, self.inner.cache_stats())
    }
}

impl std::ops::Deref for Database {
    type Target = redb::Database;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ReadDatabase {
    /// Open a file another process may be writing.
    pub fn open(path: &Path, cache: usize) -> eyre::Result<Self> {
        let mut builder = redb::Builder::new();
        builder.set_cache_size(cache);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let inner = builder
            .open_read_only(path)
            .wrap_err_with(|| format!("open {} read-only", path.display()))?;
        Ok(Self {
            inner,
            cache: cache as u64,
            label: ipc::db::file_label(path),
        })
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> ipc::db::CacheReading {
        ipc::db::reading(&self.label, self.cache, self.inner.cache_stats())
    }
}

impl std::ops::Deref for ReadDatabase {
    type Target = redb::ReadOnlyDatabase;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn open_writer(path: &Path, cache: usize, create: bool) -> eyre::Result<Database> {
    let mut builder = redb::Builder::new();
    builder.set_cache_size(cache);
    builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
    let verb = if create { "create" } else { "open" };
    let inner = if create {
        builder.create(path)
    } else {
        builder.open(path)
    }
    .wrap_err_with(|| format!("{verb} {}", path.display()))?;
    Ok(Database {
        inner,
        cache: cache as u64,
        label: ipc::db::file_label(path),
    })
}

fn ensure_parent(path: &Path) -> eyre::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
