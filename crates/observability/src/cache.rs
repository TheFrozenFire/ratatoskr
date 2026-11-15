//! Page-cache counters for the databases this process has open.
//!
//! redb's builder default is 1 GiB. Each open sets its own budget and either
//! publishes a reading or registers a watch. The status document does not
//! read this module. `readings` is the metrics exposition.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counters {
    used: u64,
    evictions: u64,
    read_hits: u64,
    read_misses: u64,
    write_hits: u64,
    write_misses: u64,
}

/// One database's cache budget and the counters redb has accumulated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheReading {
    pub name: String,
    pub limit: u64,
    pub used: u64,
    pub evictions: u64,
    pub read_hits: u64,
    pub read_misses: u64,
    pub write_hits: u64,
    pub write_misses: u64,
}

/// Change since the previous `pace` call. Totals are the current counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pace {
    pub name: String,
    pub limit: u64,
    pub used: u64,
    pub read_hits: u64,
    pub read_misses: u64,
    pub write_hits: u64,
    pub write_misses: u64,
    pub evictions: u64,
    pub read_misses_delta: u64,
    pub write_misses_delta: u64,
    pub evictions_delta: u64,
}

struct Watch {
    name: String,
    limit: u64,
    sample: Box<dyn Fn() -> Option<Counters> + Send + Sync>,
}

struct Slot {
    owned: Vec<CacheReading>,
    watched: Vec<Watch>,
    prev: HashMap<String, Counters>,
    probes: Vec<(&'static str, u64)>,
}

fn slot() -> &'static Mutex<Slot> {
    static SLOT: OnceLock<Mutex<Slot>> = OnceLock::new();
    SLOT.get_or_init(|| {
        Mutex::new(Slot {
            owned: Vec::new(),
            watched: Vec::new(),
            prev: HashMap::new(),
            probes: Vec::new(),
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Slot> {
    slot().lock().unwrap_or_else(|err| err.into_inner())
}

fn counters_of(stats: redb::CacheStats) -> Counters {
    Counters {
        used: u64::try_from(stats.used_bytes()).unwrap_or(u64::MAX),
        evictions: stats.evictions(),
        read_hits: stats.read_hits(),
        read_misses: stats.read_misses(),
        write_hits: stats.write_hits(),
        write_misses: stats.write_misses(),
    }
}

fn reading_from(name: String, limit: u64, counters: Counters) -> CacheReading {
    CacheReading {
        name,
        limit,
        used: counters.used,
        evictions: counters.evictions,
        read_hits: counters.read_hits,
        read_misses: counters.read_misses,
        write_hits: counters.write_hits,
        write_misses: counters.write_misses,
    }
}

/// File name used as the metrics label.
pub fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "redb".to_string())
}

/// A reading from a database this caller still owns.
pub fn reading(name: impl Into<String>, limit: u64, stats: redb::CacheStats) -> CacheReading {
    reading_from(name.into(), limit, counters_of(stats))
}

/// Replace the readings published by the owner. Watched databases stay.
pub fn publish(rows: Vec<CacheReading>) {
    lock().owned = rows;
}

/// Follow `db` until the last strong reference is dropped.
pub fn watch_db<T>(name: impl Into<String>, limit: u64, db: &Arc<T>)
where
    T: redb::ReadableDatabase + Send + Sync + 'static,
{
    let weak: Weak<T> = Arc::downgrade(db);
    let name = name.into();
    let mut slot = lock();
    slot.watched.retain(|watch| (watch.sample)().is_some());
    slot.watched.push(Watch {
        name,
        limit,
        sample: Box::new(move || weak.upgrade().map(|db| counters_of(db.cache_stats()))),
    });
}

fn collect(slot: &mut Slot) -> Vec<CacheReading> {
    let mut rows = slot.owned.clone();
    slot.watched.retain(|watch| {
        if let Some(counters) = (watch.sample)() {
            rows.push(reading_from(watch.name.clone(), watch.limit, counters));
            true
        } else {
            false
        }
    });
    rows.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.limit.cmp(&right.limit))
    });
    rows
}

/// Owned readings plus databases that are still open.
pub fn readings() -> Vec<CacheReading> {
    let mut slot = lock();
    collect(&mut slot)
}

/// Totals and the change since the last call.
pub fn pace() -> Vec<Pace> {
    let mut slot = lock();
    let rows = collect(&mut slot);
    let mut out = Vec::with_capacity(rows.len());
    let mut seen = HashMap::with_capacity(rows.len());
    for row in rows {
        let prev = slot.prev.get(&row.name).copied().unwrap_or_default();
        seen.insert(
            row.name.clone(),
            Counters {
                used: row.used,
                evictions: row.evictions,
                read_hits: row.read_hits,
                read_misses: row.read_misses,
                write_hits: row.write_hits,
                write_misses: row.write_misses,
            },
        );
        out.push(Pace {
            name: row.name,
            limit: row.limit,
            used: row.used,
            read_hits: row.read_hits,
            read_misses: row.read_misses,
            write_hits: row.write_hits,
            write_misses: row.write_misses,
            evictions: row.evictions,
            read_misses_delta: row.read_misses.saturating_sub(prev.read_misses),
            write_misses_delta: row.write_misses.saturating_sub(prev.write_misses),
            evictions_delta: row.evictions.saturating_sub(prev.evictions),
        });
    }
    slot.prev = seen;
    out
}

/// Last duration, in milliseconds, of one sampler probe.
pub fn record_probe(name: &'static str, ms: u64) {
    let mut slot = lock();
    if let Some(probe) = slot.probes.iter_mut().find(|(probe, _)| *probe == name) {
        probe.1 = ms;
    } else {
        slot.probes.push((name, ms));
    }
}

/// Sampler probe durations, in the order they were first recorded.
pub fn probes() -> Vec<(&'static str, u64)> {
    lock().probes.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{ReadableDatabase, TableDefinition};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const TABLE: TableDefinition<&str, u64> = TableDefinition::new("probe");

    fn temp_path(label: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ratatoskr-db-{label}-{}-{}-{}",
            std::process::id(),
            nanos,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_watched_database_reports_its_cache_budget_and_then_leaves() {
        let path = temp_path("watch");
        let mut builder = redb::Builder::new();
        let limit = 1024 * 1024;
        builder.set_cache_size(limit);
        let db = Arc::new(builder.create(&path).unwrap());
        let name = file_label(&path);
        watch_db(name.clone(), limit as u64, &db);
        {
            let write = db.begin_write().unwrap();
            {
                let mut table = write.open_table(TABLE).unwrap();
                table.insert("k", 1_u64).unwrap();
            }
            write.commit().unwrap();
            let read = db.begin_read().unwrap();
            let table = read.open_table(TABLE).unwrap();
            assert_eq!(table.get("k").unwrap().unwrap().value(), 1);
        }
        let row = readings()
            .into_iter()
            .find(|row| row.name == name)
            .expect("watched database");
        assert_eq!(row.limit, limit as u64);
        assert!(
            row.used > 0 || row.read_hits + row.read_misses + row.write_hits + row.write_misses > 0,
            "cache counters stayed at zero"
        );
        drop(db);
        assert!(readings().iter().all(|row| row.name != name));
        let _ = std::fs::remove_file(&path);
    }
}
