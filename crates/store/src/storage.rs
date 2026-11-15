//! redb file handles.
//!
//! A service names the file and the page cache. This module creates the parent
//! directory, opens the file, and keeps the handle. One process writes each
//! file. A reader that follows a writer opens the file read-only. Table reads
//! and writes stay with the service.

use eyre::WrapErr;
use redb::ReadableDatabase;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A redb file this process writes.
pub struct Database {
    inner: Arc<redb::Database>,
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

/// One service file, opened for writing or for reading.
pub enum File {
    Write(Database),
    Read(ReadDatabase),
}

impl Database {
    /// Create the file, or open it when it already exists.
    ///
    /// The parent directory is created when it is missing.
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

    /// Publish cache counters until the last strong reference of this handle is dropped.
    pub fn watch(&self) {
        observability::cache::watch_db(self.label.clone(), self.cache, &self.inner);
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> observability::cache::CacheReading {
        observability::cache::reading(&self.label, self.cache, self.inner.cache_stats())
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
        let inner = builder(cache)
            .open_read_only(path)
            .wrap_err_with(|| format!("open {} read-only", path.display()))?;
        Ok(Self {
            inner,
            cache: cache as u64,
            label: observability::cache::file_label(path),
        })
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> observability::cache::CacheReading {
        observability::cache::reading(&self.label, self.cache, self.inner.cache_stats())
    }
}

impl std::ops::Deref for ReadDatabase {
    type Target = redb::ReadOnlyDatabase;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl File {
    /// Create the file, or open it for writing when it already exists.
    pub fn create(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self::Write(Database::create(path, cache)?))
    }

    /// Open a file another process may be writing. Callers only read.
    pub fn read(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self::Read(ReadDatabase::open(path, cache)?))
    }

    pub fn begin_read(&self) -> Result<redb::ReadTransaction, redb::TransactionError> {
        match self {
            Self::Write(db) => db.begin_read(),
            Self::Read(db) => db.begin_read(),
        }
    }

    pub fn begin_write(&self) -> eyre::Result<redb::WriteTransaction> {
        match self {
            Self::Write(db) => Ok(db.begin_write()?),
            Self::Read(db) => Err(eyre::eyre!("{} is open read-only", db.label)),
        }
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> observability::cache::CacheReading {
        match self {
            Self::Write(db) => db.reading(),
            Self::Read(db) => db.reading(),
        }
    }
}

/// `data/ratatoskr` beside the store crate.
pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}

/// One file in [`data_dir`].
pub fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

fn builder(cache: usize) -> redb::Builder {
    let mut builder = redb::Builder::new();
    builder.set_cache_size(cache);
    builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
    builder
}

fn open_writer(path: &Path, cache: usize, create: bool) -> eyre::Result<Database> {
    let verb = if create { "create" } else { "open" };
    let inner = if create {
        builder(cache).create(path)
    } else {
        builder(cache).open(path)
    }
    .wrap_err_with(|| format!("{verb} {}", path.display()))?;
    Ok(Database {
        inner: Arc::new(inner),
        cache: cache as u64,
        label: observability::cache::file_label(path),
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
