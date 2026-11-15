//! Probes of the catalog summary, the header cursor, the corpus, and the page caches.

use store::LocalStore;

pub(crate) fn spawn_sampler(store: LocalStore, process: config::Process) {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interval = process.sample_interval();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if busy.swap(true, std::sync::atomic::Ordering::AcqRel) {
                continue;
            }
            let store = store.clone();
            let process = process.clone();
            let busy = std::sync::Arc::clone(&busy);
            tokio::task::spawn_blocking(move || {
                sample_store(&store, &process);
                busy.store(false, std::sync::atomic::Ordering::Release);
            });
        }
    });
}

fn sample_store(store: &LocalStore, process: &config::Process) {
    observability::cache::publish(store.cache_readings());
    let summary_ms = millis(|| match store.state_summary() {
        Ok(summary) => observability::publish_summary(&summary),
        Err(err) => observability::warn!(%err, "catalog summary"),
    });
    observability::cache::record_probe("summary", summary_ms);
    let header_ms = millis(|| {
        if let Ok(cursor) = store.stored_header() {
            observability::note_header_cursor(cursor);
        }
    });
    observability::cache::record_probe("headers", header_ms);
    let corpus_ms = millis(|| {
        if let Ok((bytes, holes)) = store.corpus_gauge() {
            observability::note_corpus(bytes, holes);
        }
    });
    observability::cache::record_probe("corpus", corpus_ms);
    log_sample(summary_ms, header_ms, corpus_ms, process);
}

fn millis(sample: impl FnOnce()) -> u64 {
    let started = std::time::Instant::now();
    sample();
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn log_sample(summary_ms: u64, header_ms: u64, corpus_ms: u64, process: &config::Process) {
    let slow = process.slow_sample_ms;
    let gap = process.warn_gap();
    if summary_ms >= slow && warn_due("summary", gap) {
        observability::warn!(ms = summary_ms, "catalog summary");
    }
    if header_ms >= slow && warn_due("headers", gap) {
        observability::warn!(ms = header_ms, "header cursor read");
    }
    if corpus_ms >= slow && warn_due("corpus", gap) {
        observability::warn!(ms = corpus_ms, "corpus gauge read");
    }
    let info = info_due(process.cache_log_secs);
    for pace in observability::cache::pace() {
        let hot = pace.read_misses_delta >= process.miss_warn
            || pace.write_misses_delta >= process.miss_warn;
        if hot && warn_due(&pace.name, gap) {
            observability::warn!(
                db = %pace.name,
                read_misses = pace.read_misses_delta,
                write_misses = pace.write_misses_delta,
                used = pace.used,
                limit = pace.limit,
                evictions = pace.evictions_delta,
                "db cache misses"
            );
        }
        if info {
            observability::info!(
                db = %pace.name,
                limit = pace.limit,
                used = pace.used,
                read_hits = pace.read_hits,
                read_misses = pace.read_misses,
                write_hits = pace.write_hits,
                write_misses = pace.write_misses,
                evictions = pace.evictions,
                "db cache"
            );
        }
    }
}

fn info_due(cache_log_secs: u64) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let next = NEXT.load(Ordering::Relaxed);
    if now < next {
        return false;
    }
    NEXT.store(now.saturating_add(cache_log_secs), Ordering::Relaxed);
    true
}

fn warn_due(name: &str, gap: std::time::Duration) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    let mut slot = LAST.lock().unwrap_or_else(|err| err.into_inner());
    let seen = slot.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if let Some(then) = seen.get(name) {
        if now.duration_since(*then) < gap {
            return false;
        }
    }
    seen.insert(name.to_string(), now);
    true
}
