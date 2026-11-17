//! Ingestion lines. Process gauges come from the exporter. Stored rows come
//! from the read-only shelf. A down exporter blanks the process columns.

use serde_json::Value;

pub(crate) const STAGE_NAMES: [&str; 5] = ["immutable", "mutable", "ticks", "curve", "conduit"];

#[derive(Clone, Debug, Default)]
pub(crate) struct Local {
    pub header_number: Option<u64>,
    pub header_timestamp: Option<u64>,
    pub header_hash: String,
    pub corpus_bytes: Option<u64>,
    pub corpus_holes: Option<u64>,
    pub cursors: Vec<(String, u64)>,
    pub token_count: Option<u64>,
    pub feed_count: Option<u64>,
    pub spot_count: Option<u64>,
    pub queued: [Option<u64>; 5],
    pub identities: Option<u64>,
    pub immutable: Option<u64>,
    pub mutable: Option<u64>,
    pub tick_pages: Option<u64>,
    pub curves: Option<u64>,
    pub conduits: Option<u64>,
    pub mismatches: Option<u64>,
    pub jobs: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct StageRow {
    pub name: String,
    pub queued: String,
    pub batches: String,
    pub jobs: String,
    pub last: String,
    pub rate: String,
    pub spark: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Ingest {
    pub exporter_up: bool,
    pub notice: String,
    pub block: String,
    pub uptime: String,
    pub paused: Option<bool>,
    pub pipelines: Vec<String>,
    pub catalog: String,
    pub stages: Vec<StageRow>,
}

impl Default for Ingest {
    fn default() -> Self {
        Self {
            exporter_up: false,
            notice: "exporter is down".into(),
            block: String::new(),
            uptime: String::new(),
            paused: None,
            pipelines: Vec::new(),
            catalog: String::new(),
            stages: Vec::new(),
        }
    }
}

pub(crate) fn ingest_from(snap: Option<&Value>, local: &Local, rates: &[f64]) -> Ingest {
    let Some(snap) = snap else {
        return Ingest {
            exporter_up: false,
            notice: "exporter is down".into(),
            block: String::new(),
            uptime: String::new(),
            paused: None,
            pipelines: local_pipelines(local),
            catalog: catalog_from_local(local),
            stages: local_stages(local),
        };
    };
    let paused = snap["paused"].as_bool();
    Ingest {
        exporter_up: true,
        notice: String::new(),
        block: snap["block"]
            .as_u64()
            .map(|block| block.to_string())
            .unwrap_or_default(),
        uptime: snap["uptime_ms"]
            .as_u64()
            .map(format_uptime)
            .unwrap_or_default(),
        paused,
        pipelines: {
            let mut lines = pipeline_text(Some(snap));
            if local.header_number.is_some() {
                lines.push(chain_line(local));
            }
            if !local.cursors.is_empty() {
                lines.push(cursor_line(local));
            }
            lines
        },
        catalog: catalog_from_snap(snap),
        stages: snap_stages(snap, rates),
    }
}

pub(crate) fn control_path(paused: bool) -> &'static str {
    if paused {
        "/resume"
    } else {
        "/pause"
    }
}

pub(crate) fn control_label(paused: bool) -> &'static str {
    if paused {
        "p resumes"
    } else {
        "p pauses"
    }
}

fn local_pipelines(local: &Local) -> Vec<String> {
    vec![
        chain_line(local),
        cursor_line(local),
        row(
            "token lens",
            "token records",
            &match local.token_count {
                Some(count) => format!("{} stored", commas(count)),
                None => "—".into(),
            },
        ),
        row(
            "oracles",
            "feeds",
            &match local.feed_count {
                Some(count) => format!("{} feeds", commas(count)),
                None => "—".into(),
            },
        ),
        row(
            "spots",
            "spots",
            &match local.spot_count {
                Some(count) => format!("{} spots", commas(count)),
                None => "—".into(),
            },
        ),
        corpus_line(local),
    ]
}

fn chain_line(local: &Local) -> String {
    match local.header_number {
        Some(number) => row(
            "header chain",
            "headers",
            &format!(
                "block {}   {}   {}",
                commas(number),
                local
                    .header_timestamp
                    .map(|stamp| stamp.to_string())
                    .unwrap_or_default(),
                local.header_hash
            ),
        ),
        None => row("header chain", "headers", "not stored"),
    }
}

fn cursor_line(local: &Local) -> String {
    if local.cursors.is_empty() {
        return row("discovery", "cursors", "none");
    }
    let body = local
        .cursors
        .iter()
        .map(|(name, block)| format!("{name} {}", commas(*block)))
        .collect::<Vec<_>>()
        .join("   ");
    row("discovery", "cursors", &body)
}

fn corpus_line(local: &Local) -> String {
    match (local.corpus_bytes, local.corpus_holes) {
        (Some(bytes), holes) => row(
            "corpus",
            "corpus",
            &format!(
                "{}   {} holes",
                format_bytes(bytes),
                commas(holes.unwrap_or(0))
            ),
        ),
        _ => row("corpus", "corpus", "—"),
    }
}

fn local_stages(local: &Local) -> Vec<StageRow> {
    STAGE_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| StageRow {
            name: (*name).to_string(),
            queued: local.queued[index]
                .map(|queued| queued.to_string())
                .unwrap_or_default(),
            batches: String::new(),
            jobs: String::new(),
            last: String::new(),
            rate: String::new(),
            spark: String::new(),
        })
        .collect()
}

fn snap_stages(snap: &Value, rates: &[f64]) -> Vec<StageRow> {
    snap["stages"]
        .as_array()
        .map(|stages| {
            stages
                .iter()
                .enumerate()
                .map(|(index, stage)| {
                    let batches = stage["batches"].as_u64().unwrap_or(0);
                    let rate = if batches == 0 {
                        "—".into()
                    } else {
                        format!("{:.1}/s", rates.get(index).copied().unwrap_or(0.0))
                    };
                    let last = if batches == 0 {
                        "—".into()
                    } else {
                        format!("{} ms", stage["last_ms"].as_u64().unwrap_or(0))
                    };
                    StageRow {
                        name: stage["name"].as_str().unwrap_or("").to_string(),
                        queued: stage["queued"].as_u64().unwrap_or(0).to_string(),
                        batches: batches.to_string(),
                        jobs: stage["jobs"].as_u64().unwrap_or(0).to_string(),
                        last,
                        rate,
                        spark: spark(&stage["samples_ms"]),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn catalog_from_snap(snap: &Value) -> String {
    let num = |field: &str| snap[field].as_u64().unwrap_or(0);
    format!(
        "identities {}   immutable {}   mutable {}   tick pages {}   curves {}   conduits {}   mismatches {}   jobs {}",
        commas(num("identities")),
        commas(num("immutable")),
        commas(num("mutable")),
        commas(num("tick_pages")),
        commas(num("curves")),
        commas(num("conduits")),
        commas(num("mismatches")),
        commas(num("jobs")),
    )
}

fn catalog_from_local(local: &Local) -> String {
    let cell = |value: Option<u64>| match value {
        Some(value) => commas(value),
        None => "—".into(),
    };
    format!(
        "identities {}   immutable {}   mutable {}   tick pages {}   curves {}   conduits {}   mismatches {}   jobs {}",
        cell(local.identities),
        cell(local.immutable),
        cell(local.mutable),
        cell(local.tick_pages),
        cell(local.curves),
        cell(local.conduits),
        cell(local.mismatches),
        cell(local.jobs),
    )
}

fn pipeline_text(snap: Option<&Value>) -> Vec<String> {
    let board = snap.and_then(|snap| snap.get("board"));
    let identities = snap
        .and_then(|snap| snap["identities"].as_u64())
        .unwrap_or(0);
    vec![
        row("header sync", "header chain", &header_detail(board)),
        row(
            "discovery",
            "venue catalog",
            &format!("identities {}", commas(identities)),
        ),
        row("token lens", "token records", &token_detail(board)),
        row("oracles", "feeds", &feed_detail(board)),
        row("spots", "spots", &spot_detail(board)),
        row("peer service", "peer directory", &peer_detail(board)),
        row("corpus", "corpus", &corpus_detail(board)),
    ]
}

fn row(pipeline: &str, catalog: &str, detail: &str) -> String {
    format!(" {pipeline:<13} {catalog:<16} {detail}")
}

fn header_detail(board: Option<&Value>) -> String {
    let Some(headers) = board_row(board, "headers") else {
        return "not running".into();
    };
    if !headers["live"].as_bool().unwrap_or(false) {
        return "not running".into();
    }
    let cursor = headers["cursor"].as_u64().unwrap_or(0);
    if !headers["head_known"].as_bool().unwrap_or(false) {
        return format!("cursor {}", commas(cursor));
    }
    let head = headers["head"].as_u64().unwrap_or(0);
    format!(
        "cursor {}   head {}   behind {}",
        commas(cursor),
        commas(head),
        commas(head.saturating_sub(cursor))
    )
}

fn token_detail(board: Option<&Value>) -> String {
    let Some(tokens) = board_row(board, "tokens") else {
        return "not running".into();
    };
    let records_known = tokens["records_known"].as_bool().unwrap_or(false);
    let missing_known = tokens["missing_known"].as_bool().unwrap_or(false);
    if !records_known && !missing_known {
        return "not running".into();
    }
    let mut parts = Vec::new();
    if records_known {
        parts.push(format!(
            "{} stored",
            commas(tokens["records"].as_u64().unwrap_or(0))
        ));
    }
    if missing_known {
        let missing = commas(tokens["missing"].as_u64().unwrap_or(0));
        if tokens["capped"].as_bool().unwrap_or(false) {
            parts.push(format!("at least {missing} missing"));
        } else {
            parts.push(format!("{missing} missing"));
        }
    }
    if tokens["reading"].as_bool().unwrap_or(false) {
        parts.push("reading".into());
    }
    parts.join("   ")
}

fn feed_detail(board: Option<&Value>) -> String {
    let Some(feeds) = board_row(board, "feeds") else {
        return "not running".into();
    };
    if !feeds["live"].as_bool().unwrap_or(false) {
        return "not running".into();
    }
    let mut text = format!(
        "{} feeds   {} configured   {} answers",
        commas(feeds["feeds"].as_u64().unwrap_or(0)),
        commas(feeds["configured"].as_u64().unwrap_or(0)),
        commas(feeds["answers"].as_u64().unwrap_or(0))
    );
    let jobs = feeds["jobs"].as_u64().unwrap_or(0);
    if jobs > 0 {
        text.push_str(&format!("   {} history jobs", commas(jobs)));
    }
    if feeds["follow_known"].as_bool().unwrap_or(false) {
        text.push_str(&format!(
            "   at {}",
            commas(feeds["follow"].as_u64().unwrap_or(0))
        ));
    }
    if feeds["lookback"].as_bool().unwrap_or(false) {
        text.push_str("   following");
    } else {
        text.push_str("   lookback");
    }
    text
}

fn spot_detail(board: Option<&Value>) -> String {
    let Some(spots) = board_row(board, "spots") else {
        return "not running".into();
    };
    if !spots["live"].as_bool().unwrap_or(false) {
        return "not running".into();
    }
    format!("{} spots", commas(spots["spots"].as_u64().unwrap_or(0)))
}

fn peer_detail(board: Option<&Value>) -> String {
    let Some(peers) = board_row(board, "peers") else {
        return "not running".into();
    };
    if !peers["live"].as_bool().unwrap_or(false) {
        return "not running".into();
    }
    format!(
        "{} sessions   {} summaries   {} sightings",
        commas(peers["sessions"].as_u64().unwrap_or(0)),
        commas(peers["summaries"].as_u64().unwrap_or(0)),
        commas(peers["sightings"].as_u64().unwrap_or(0))
    )
}

fn corpus_detail(board: Option<&Value>) -> String {
    let Some(corpus) = board_row(board, "corpus") else {
        return "not running".into();
    };
    if !corpus["live"].as_bool().unwrap_or(false) {
        return "not running".into();
    }
    format!(
        "{}   {} holes",
        format_bytes(corpus["bytes"].as_u64().unwrap_or(0)),
        commas(corpus["holes"].as_u64().unwrap_or(0))
    )
}

fn board_row<'a>(board: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    board.and_then(|board| board.get(name))
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn spark(samples: &Value) -> String {
    const BARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = samples
        .as_array()
        .map(|samples| {
            samples
                .iter()
                .filter_map(|value| value.as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let max = values.iter().copied().max().unwrap_or(1).max(1000);
    values
        .iter()
        .map(|value| {
            let index = ((*value * 7) / max) as usize;
            BARS[index.min(7)]
        })
        .collect()
}

pub(crate) fn commas(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub(crate) fn format_uptime(ms: u64) -> String {
    let secs = ms / 1000;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    format!("{hours}h {mins:02}m {rem:02}s")
}
