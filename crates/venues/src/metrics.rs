//! Process metrics for the state pipeline.
//!
//! Counters and gauges use Prometheus names. [`prometheus_text`] renders the
//! exposition format. [`status_json`] is the document the status TUI polls.

use crate::state::StateSummary;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const SAMPLE_LEN: usize = 48;

/// Pipeline order. Quote is stage 5 and curve is stage 4.
const STAGE_ORDER: [(u8, &str, &str); 5] = [
    (1, "immutable", "immutable"),
    (2, "mutable", "mutable"),
    (3, "ticks", "tick_pages"),
    (5, "quote", "quotes"),
    (4, "curve", "curves"),
];

struct StageMeters {
    batches: AtomicU64,
    jobs: AtomicU64,
    sum_ms: AtomicU64,
    last_ms: AtomicU64,
    inflight: AtomicU64,
    queued: AtomicU64,
    samples: Mutex<VecDeque<u64>>,
}

impl StageMeters {
    fn new() -> Self {
        Self {
            batches: AtomicU64::new(0),
            jobs: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::with_capacity(SAMPLE_LEN)),
        }
    }
}

struct Metrics {
    started: Instant,
    block: AtomicU64,
    identities: AtomicU64,
    immutable: AtomicU64,
    mutable: AtomicU64,
    tick_pages: AtomicU64,
    quotes: AtomicU64,
    curves: AtomicU64,
    mismatches: AtomicU64,
    jobs: AtomicU64,
    job_errors: AtomicU64,
    stages: [StageMeters; 6],
    errors: Mutex<Vec<(String, u64)>>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            block: AtomicU64::new(0),
            identities: AtomicU64::new(0),
            immutable: AtomicU64::new(0),
            mutable: AtomicU64::new(0),
            tick_pages: AtomicU64::new(0),
            quotes: AtomicU64::new(0),
            curves: AtomicU64::new(0),
            mismatches: AtomicU64::new(0),
            jobs: AtomicU64::new(0),
            job_errors: AtomicU64::new(0),
            stages: [
                StageMeters::new(),
                StageMeters::new(),
                StageMeters::new(),
                StageMeters::new(),
                StageMeters::new(),
                StageMeters::new(),
            ],
            errors: Mutex::new(Vec::new()),
        }
    }
}

fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

fn stage_meters(stage: u8) -> Option<&'static StageMeters> {
    metrics().stages.get(stage as usize)
}

pub fn set_block(block: u64) {
    metrics().block.store(block, Ordering::Relaxed);
}

pub fn begin_batch(stage: u8, jobs: u64) {
    if let Some(meters) = stage_meters(stage) {
        meters.inflight.store(jobs, Ordering::Relaxed);
    }
}

pub fn record_batch(stage: u8, jobs: u64, ms: u64) {
    let Some(meters) = stage_meters(stage) else {
        return;
    };
    meters.batches.fetch_add(1, Ordering::Relaxed);
    meters.jobs.fetch_add(jobs, Ordering::Relaxed);
    meters.sum_ms.fetch_add(ms, Ordering::Relaxed);
    meters.last_ms.store(ms, Ordering::Relaxed);
    meters.inflight.store(0, Ordering::Relaxed);
    if let Ok(mut samples) = meters.samples.lock() {
        if samples.len() == SAMPLE_LEN {
            samples.pop_front();
        }
        samples.push_back(ms);
    }
}

/// Replace catalog gauges from one summary scan.
pub fn publish_summary(summary: &StateSummary) {
    let metrics = metrics();
    metrics
        .identities
        .store(summary.identities, Ordering::Relaxed);
    metrics
        .immutable
        .store(summary.immutable, Ordering::Relaxed);
    metrics.mutable.store(summary.mutable, Ordering::Relaxed);
    metrics
        .tick_pages
        .store(summary.tick_pages, Ordering::Relaxed);
    metrics.quotes.store(summary.quotes, Ordering::Relaxed);
    metrics.curves.store(summary.curves, Ordering::Relaxed);
    metrics
        .mismatches
        .store(summary.mismatches, Ordering::Relaxed);
    metrics.jobs.store(summary.jobs, Ordering::Relaxed);
    for meters in &metrics.stages {
        meters.queued.store(0, Ordering::Relaxed);
    }
    for (stage, count) in &summary.jobs_by_stage {
        if let Some(meters) = stage_meters(*stage) {
            meters.queued.store(*count, Ordering::Relaxed);
        }
    }
    let mut errors: Vec<(String, u64)> = summary
        .errors
        .iter()
        .map(|(error, count)| (error.clone(), *count))
        .collect();
    errors.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    errors.truncate(12);
    let job_errors = errors.iter().map(|(_, count)| *count).sum();
    metrics.job_errors.store(job_errors, Ordering::Relaxed);
    if let Ok(mut slot) = metrics.errors.lock() {
        *slot = errors;
    }
}

pub fn status_json() -> String {
    snapshot().to_string()
}

pub fn prometheus_text() -> String {
    let snap = snapshot();
    let mut out = String::new();
    gauge(&mut out, "ratatoskr_up", "Exporter is serving.", &[]);
    line(&mut out, "ratatoskr_up", &[], "1");
    gauge(
        &mut out,
        "ratatoskr_state_block",
        "Block the state pipeline is reading.",
        &[],
    );
    line(
        &mut out,
        "ratatoskr_state_block",
        &[],
        &snap["block"].to_string(),
    );
    gauge(
        &mut out,
        "ratatoskr_state_uptime_seconds",
        "Seconds since this process started exporting.",
        &[],
    );
    let uptime = snap["uptime_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
    line(
        &mut out,
        "ratatoskr_state_uptime_seconds",
        &[],
        &format!("{uptime:.3}"),
    );
    gauge(
        &mut out,
        "ratatoskr_catalog_rows",
        "Rows stored in the catalog.",
        &[("table", "")],
    );
    for (table, field) in [
        ("identities", "identities"),
        ("immutable", "immutable"),
        ("mutable", "mutable"),
        ("tick_pages", "tick_pages"),
        ("quotes", "quotes"),
        ("curves", "curves"),
        ("mismatches", "mismatches"),
        ("jobs", "jobs"),
    ] {
        line(
            &mut out,
            "ratatoskr_catalog_rows",
            &[("table", table)],
            &snap[field].to_string(),
        );
    }
    gauge(
        &mut out,
        "ratatoskr_state_job_errors",
        "Queued jobs that carry an error string.",
        &[],
    );
    line(
        &mut out,
        "ratatoskr_state_job_errors",
        &[],
        &snap["job_errors"].to_string(),
    );
    counter(
        &mut out,
        "ratatoskr_state_batches_total",
        "Batches finished in this process.",
        &[("stage", "")],
    );
    counter(
        &mut out,
        "ratatoskr_state_jobs_total",
        "Jobs finished in this process.",
        &[("stage", "")],
    );
    counter(
        &mut out,
        "ratatoskr_state_batch_milliseconds_sum",
        "Sum of batch durations.",
        &[("stage", "")],
    );
    gauge(
        &mut out,
        "ratatoskr_state_batch_milliseconds_last",
        "Duration of the last batch.",
        &[("stage", "")],
    );
    gauge(
        &mut out,
        "ratatoskr_state_jobs_queued",
        "Jobs waiting in this stage.",
        &[("stage", "")],
    );
    gauge(
        &mut out,
        "ratatoskr_state_jobs_inflight",
        "Jobs in the batch running now.",
        &[("stage", "")],
    );
    if let Some(stages) = snap["stages"].as_array() {
        for stage in stages {
            let name = stage["name"].as_str().unwrap_or("");
            let labels = [("stage", name)];
            line(
                &mut out,
                "ratatoskr_state_batches_total",
                &labels,
                &stage["batches"].to_string(),
            );
            line(
                &mut out,
                "ratatoskr_state_jobs_total",
                &labels,
                &stage["jobs"].to_string(),
            );
            line(
                &mut out,
                "ratatoskr_state_batch_milliseconds_sum",
                &labels,
                &stage["sum_ms"].to_string(),
            );
            line(
                &mut out,
                "ratatoskr_state_batch_milliseconds_last",
                &labels,
                &stage["last_ms"].to_string(),
            );
            line(
                &mut out,
                "ratatoskr_state_jobs_queued",
                &labels,
                &stage["queued"].to_string(),
            );
            line(
                &mut out,
                "ratatoskr_state_jobs_inflight",
                &labels,
                &stage["inflight"].to_string(),
            );
        }
    }
    out
}

fn snapshot() -> Value {
    let metrics = metrics();
    let errors = metrics
        .errors
        .lock()
        .map(|errors| {
            errors
                .iter()
                .map(|(error, count)| json!({"error": error, "count": count}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let stored = |field: &str| match field {
        "immutable" => metrics.immutable.load(Ordering::Relaxed),
        "mutable" => metrics.mutable.load(Ordering::Relaxed),
        "tick_pages" => metrics.tick_pages.load(Ordering::Relaxed),
        "quotes" => metrics.quotes.load(Ordering::Relaxed),
        "curves" => metrics.curves.load(Ordering::Relaxed),
        _ => 0,
    };
    let stages = STAGE_ORDER
        .iter()
        .filter_map(|(stage, name, field)| {
            let meters = stage_meters(*stage)?;
            let samples = meters
                .samples
                .lock()
                .map(|samples| samples.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            Some(json!({
                "stage": stage,
                "name": name,
                "stored": stored(field),
                "queued": meters.queued.load(Ordering::Relaxed),
                "batches": meters.batches.load(Ordering::Relaxed),
                "jobs": meters.jobs.load(Ordering::Relaxed),
                "inflight": meters.inflight.load(Ordering::Relaxed),
                "last_ms": meters.last_ms.load(Ordering::Relaxed),
                "sum_ms": meters.sum_ms.load(Ordering::Relaxed),
                "samples_ms": samples,
            }))
        })
        .collect::<Vec<_>>();
    json!({
        "block": metrics.block.load(Ordering::Relaxed),
        "uptime_ms": metrics.started.elapsed().as_millis() as u64,
        "identities": metrics.identities.load(Ordering::Relaxed),
        "immutable": metrics.immutable.load(Ordering::Relaxed),
        "mutable": metrics.mutable.load(Ordering::Relaxed),
        "tick_pages": metrics.tick_pages.load(Ordering::Relaxed),
        "quotes": metrics.quotes.load(Ordering::Relaxed),
        "curves": metrics.curves.load(Ordering::Relaxed),
        "mismatches": metrics.mismatches.load(Ordering::Relaxed),
        "jobs": metrics.jobs.load(Ordering::Relaxed),
        "job_errors": metrics.job_errors.load(Ordering::Relaxed),
        "stages": stages,
        "errors": errors,
    })
}

fn counter(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)]) {
    help_type(out, name, help, "counter", labels);
}

fn gauge(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)]) {
    help_type(out, name, help, "gauge", labels);
}

fn help_type(out: &mut String, name: &str, help: &str, kind: &str, _labels: &[(&str, &str)]) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn line(out: &mut String, name: &str, labels: &[(&str, &str)], value: &str) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (key, value)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(key);
            out.push_str("=\"");
            out.push_str(&escape_label(value));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_shows_up_in_the_status_and_the_exposition() {
        record_batch(1, 16, 200);
        let status = status_json();
        assert!(status.contains("\"name\":\"immutable\""));
        assert!(status.contains("\"batches\":"));
        let text = prometheus_text();
        assert!(text.contains("ratatoskr_state_batches_total{stage=\"immutable\"}"));
        assert!(text.contains("# TYPE ratatoskr_state_batches_total counter"));
    }
}
