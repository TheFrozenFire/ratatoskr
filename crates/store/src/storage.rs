//! redb file handles.
//!
//! A service names the file and the page cache. This module creates the parent
//! directory, opens the file, and keeps the handle. One process writes each
//! file. A reader that follows a writer opens the file read-only. Table reads
//! and writes stay with the service.

use eyre::WrapErr;
use redb::ReadableDatabase;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Start a write. Each commit stores allocator state, so a crash reopens
    /// the file instead of walking it.
    pub fn begin_write(&self) -> Result<redb::WriteTransaction, redb::TransactionError> {
        let mut txn = self.inner.begin_write()?;
        txn.set_quick_repair(true);
        Ok(txn)
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn reading(&self) -> observability::cache::CacheReading {
        observability::cache::reading(&self.label, self.cache, self.inner.cache_stats())
    }

    /// Rewrite the file so deleted pages are not left allocated.
    ///
    /// The handle must be the only owner. Closing it afterwards records allocator state.
    pub fn compact(&mut self) -> eyre::Result<bool> {
        let db = Arc::get_mut(&mut self.inner)
            .ok_or_else(|| eyre::eyre!("database handle is shared"))?;
        db.compact().wrap_err("compact database")
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

    /// Rewrite a writer so deleted pages are not left allocated.
    pub fn compact(&mut self) -> eyre::Result<bool> {
        match self {
            Self::Write(db) => db.compact(),
            Self::Read(db) => Err(eyre::eyre!("{} is open read-only", db.label)),
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

fn note_repair(label: &str) {
    #[cfg(test)]
    if let Ok(mut names) = REPAIR_FILES.lock() {
        names.push(label.to_string());
    }
    #[cfg(not(test))]
    let _ = label;
}

#[cfg(test)]
static REPAIR_FILES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn open_writer(path: &Path, cache: usize, create: bool) -> eyre::Result<Database> {
    let verb = if create { "create" } else { "open" };
    let label = observability::cache::file_label(path);
    let repaired = Arc::new(AtomicBool::new(false));
    let mut opener = builder(cache);
    let flag = Arc::clone(&repaired);
    let repair_label = label.clone();
    opener.set_repair_callback(move |session| {
        flag.store(true, Ordering::Relaxed);
        let percent = (session.progress() * 100.0).round() as u32;
        note_repair(&repair_label);
        observability::warn!(file = %repair_label, percent, "database repair");
    });
    let inner = if create {
        opener.create(path)
    } else {
        opener.open(path)
    }
    .wrap_err_with(|| format!("{verb} {}", path.display()))?;
    let db = Database {
        inner: Arc::new(inner),
        cache: cache as u64,
        label,
    };
    // A repair rebuilds allocator state in memory and does not store it. The
    // next crash would walk the file again unless this commit records it.
    if repaired.load(Ordering::Relaxed) {
        let txn = db.begin_write()?;
        txn.commit()
            .wrap_err_with(|| format!("record allocator state in {}", db.label))?;
    }
    Ok(db)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("probe");

    fn temp_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ratatoskr-repair-{}-{nanos}.redb",
            std::process::id()
        ))
    }

    /// A commit records allocator state. Reopening after the process skips
    /// `Drop` reads that record and does not walk the file.
    #[test]
    fn a_crashed_commit_reopens_without_walking_the_file() {
        if std::env::var_os("RATATOSKR_CRASH_COMMIT").is_some() {
            let path = std::env::var("RATATOSKR_CRASH_PATH").unwrap();
            let db = Database::create(Path::new(&path), 1024 * 1024).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut table = write.open_table(TABLE).unwrap();
                table.insert("k", &b"v"[..]).unwrap();
            }
            write.commit().unwrap();
            std::mem::forget(db);
            std::process::exit(0);
        }

        let path = temp_path();
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("storage::tests::a_crashed_commit_reopens_without_walking_the_file")
            .arg("--exact")
            .env("RATATOSKR_CRASH_COMMIT", "1")
            .env("RATATOSKR_CRASH_PATH", &path)
            .status()
            .unwrap();
        assert!(status.success(), "child status {status}");

        let db = Database::open(&path, 1024 * 1024).unwrap();
        let names = REPAIR_FILES.lock().unwrap().clone();
        let file = path.file_name().unwrap().to_string_lossy();
        assert!(
            !names.iter().any(|name| name == file.as_ref()),
            "reopen repaired {file}: {names:?}"
        );
        let read = db.begin_read().unwrap();
        let table = read.open_table(TABLE).unwrap();
        let value = table.get("k").unwrap().unwrap();
        assert_eq!(value.value(), b"v");
        drop(value);
        drop(table);
        drop(read);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A repair stores allocator state before the handle is returned. Killing
    /// the process after that open does not walk the file the next time.
    #[test]
    fn a_repaired_file_reopens_without_walking_it_again() {
        if let Ok(mode) = std::env::var("RATATOSKR_REPAIR_CHILD") {
            let path = std::env::var("RATATOSKR_CRASH_PATH").unwrap();
            match mode.as_str() {
                "plain" => {
                    let mut opener = redb::Builder::new();
                    opener.set_cache_size(1024 * 1024);
                    opener.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
                    let db = opener.create(Path::new(&path)).unwrap();
                    let write = db.begin_write().unwrap();
                    {
                        let mut table = write.open_table(TABLE).unwrap();
                        table.insert("k", &b"v"[..]).unwrap();
                    }
                    write.commit().unwrap();
                    std::mem::forget(db);
                }
                "open" => {
                    let db = Database::open(Path::new(&path), 1024 * 1024).unwrap();
                    std::mem::forget(db);
                }
                other => panic!("unknown child {other}"),
            }
            std::process::exit(0);
        }

        let path = temp_path();
        for mode in ["plain", "open"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("storage::tests::a_repaired_file_reopens_without_walking_it_again")
                .arg("--exact")
                .env("RATATOSKR_REPAIR_CHILD", mode)
                .env("RATATOSKR_CRASH_PATH", &path)
                .status()
                .unwrap();
            assert!(status.success(), "{mode} status {status}");
        }

        let db = Database::open(&path, 1024 * 1024).unwrap();
        let names = REPAIR_FILES.lock().unwrap().clone();
        let file = path.file_name().unwrap().to_string_lossy();
        assert!(
            !names.iter().any(|name| name == file.as_ref()),
            "second reopen repaired {file}: {names:?}"
        );
        let read = db.begin_read().unwrap();
        let table = read.open_table(TABLE).unwrap();
        let value = table.get("k").unwrap().unwrap();
        assert_eq!(value.value(), b"v");
        drop(value);
        drop(table);
        drop(read);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
