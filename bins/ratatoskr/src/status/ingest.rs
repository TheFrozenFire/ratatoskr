//! Ingestion figures. Process gauges come from the exporter. Stored rows come
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
}

/// One connected peer session from the peer socket.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub label: String,
    pub eth: u64,
    pub head: Option<u64>,
    pub earliest: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct StageRow {
    pub name: String,
    pub queued: String,
    pub inflight: String,
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
    pub headers_live: bool,
    pub cursor: Option<u64>,
    pub head: Option<u64>,
    pub head_known: bool,
    pub chain_number: Option<u64>,
    pub chain_time: Option<u64>,
    pub chain_hash: String,
    pub cursors: Vec<(String, u64)>,
    pub corpus_bytes: Option<u64>,
    pub corpus_holes: Option<u64>,
    pub tokens_stored: Option<u64>,
    pub tokens_missing: Option<u64>,
    pub tokens_missing_known: bool,
    pub tokens_capped: bool,
    pub tokens_reading: bool,
    pub feeds_live: bool,
    pub feeds: Option<u64>,
    pub feeds_configured: Option<u64>,
    pub feeds_answers: Option<u64>,
    pub feeds_jobs: Option<u64>,
    pub feeds_follow: Option<u64>,
    pub feeds_follow_known: bool,
    pub feeds_following: bool,
    pub peers_live: bool,
    pub peer_sessions: Option<u64>,
    pub peer_summaries: Option<u64>,
    pub peer_sightings: Option<u64>,
    pub spots_live: bool,
    pub spots: Option<u64>,
    pub identities: Option<u64>,
    pub immutable: Option<u64>,
    pub mutable: Option<u64>,
    pub tick_pages: Option<u64>,
    pub curves: Option<u64>,
    pub conduits: Option<u64>,
    pub mismatches: Option<u64>,
    pub jobs: Option<u64>,
    pub job_errors: Option<u64>,
    pub stages: Vec<StageRow>,
    pub errors: Vec<(u64, String)>,
}

impl Default for Ingest {
    fn default() -> Self {
        Self {
            exporter_up: false,
            notice: "exporter is down".into(),
            block: String::new(),
            uptime: String::new(),
            paused: None,
            headers_live: false,
            cursor: None,
            head: None,
            head_known: false,
            chain_number: None,
            chain_time: None,
            chain_hash: String::new(),
            cursors: Vec::new(),
            corpus_bytes: None,
            corpus_holes: None,
            tokens_stored: None,
            tokens_missing: None,
            tokens_missing_known: false,
            tokens_capped: false,
            tokens_reading: false,
            feeds_live: false,
            feeds: None,
            feeds_configured: None,
            feeds_answers: None,
            feeds_jobs: None,
            feeds_follow: None,
            feeds_follow_known: false,
            feeds_following: false,
            peers_live: false,
            peer_sessions: None,
            peer_summaries: None,
            peer_sightings: None,
            spots_live: false,
            spots: None,
            identities: None,
            immutable: None,
            mutable: None,
            tick_pages: None,
            curves: None,
            conduits: None,
            mismatches: None,
            jobs: None,
            job_errors: None,
            stages: Vec::new(),
            errors: Vec::new(),
        }
    }
}

pub(crate) fn ingest_from(snap: Option<&Value>, local: &Local, rates: &[f64]) -> Ingest {
    let mut ingest = Ingest {
        chain_number: local.header_number,
        chain_time: local.header_timestamp,
        chain_hash: local.header_hash.clone(),
        cursors: local.cursors.clone(),
        ..Ingest::default()
    };
    let Some(snap) = snap else {
        ingest.exporter_up = false;
        ingest.notice = "exporter is down".into();
        ingest.corpus_bytes = local.corpus_bytes;
        ingest.corpus_holes = local.corpus_holes;
        ingest.tokens_stored = local.token_count;
        ingest.feeds = local.feed_count;
        ingest.feeds_live = local.feed_count.is_some();
        ingest.spots = local.spot_count;
        ingest.spots_live = local.spot_count.is_some();
        ingest.stages = local_stages(local);
        return ingest;
    };
    ingest.exporter_up = true;
    ingest.notice.clear();
    ingest.paused = snap["paused"].as_bool();
    ingest.block = snap["block"]
        .as_u64()
        .map(|block| block.to_string())
        .unwrap_or_default();
    ingest.uptime = snap["uptime_ms"]
        .as_u64()
        .map(format_uptime)
        .unwrap_or_default();
    if let Some(headers) = board(snap, "headers") {
        ingest.headers_live = headers["live"].as_bool().unwrap_or(false);
        ingest.cursor = headers["cursor"].as_u64();
        ingest.head_known = headers["head_known"].as_bool().unwrap_or(false);
        if ingest.head_known {
            ingest.head = headers["head"].as_u64();
        }
    }
    if let Some(corpus) = board(snap, "corpus") {
        if corpus["live"].as_bool().unwrap_or(false) {
            ingest.corpus_bytes = corpus["bytes"].as_u64();
            ingest.corpus_holes = corpus["holes"].as_u64();
        }
    }
    if let Some(tokens) = board(snap, "tokens") {
        if tokens["records_known"].as_bool().unwrap_or(false) {
            ingest.tokens_stored = tokens["records"].as_u64();
        }
        ingest.tokens_missing_known = tokens["missing_known"].as_bool().unwrap_or(false);
        if ingest.tokens_missing_known {
            ingest.tokens_missing = tokens["missing"].as_u64();
        }
        ingest.tokens_capped = tokens["capped"].as_bool().unwrap_or(false);
        ingest.tokens_reading = tokens["reading"].as_bool().unwrap_or(false);
    }
    if let Some(feeds) = board(snap, "feeds") {
        ingest.feeds_live = feeds["live"].as_bool().unwrap_or(false);
        if ingest.feeds_live {
            ingest.feeds = feeds["feeds"].as_u64();
            ingest.feeds_configured = feeds["configured"].as_u64();
            ingest.feeds_answers = feeds["answers"].as_u64();
            ingest.feeds_jobs = feeds["jobs"].as_u64();
            ingest.feeds_follow_known = feeds["follow_known"].as_bool().unwrap_or(false);
            if ingest.feeds_follow_known {
                ingest.feeds_follow = feeds["follow"].as_u64();
            }
            ingest.feeds_following = feeds["lookback"].as_bool().unwrap_or(false);
        }
    }
    if let Some(peers) = board(snap, "peers") {
        ingest.peers_live = peers["live"].as_bool().unwrap_or(false);
        if ingest.peers_live {
            ingest.peer_sessions = peers["sessions"].as_u64();
            ingest.peer_summaries = peers["summaries"].as_u64();
            ingest.peer_sightings = peers["sightings"].as_u64();
        }
    }
    if let Some(spots) = board(snap, "spots") {
        ingest.spots_live = spots["live"].as_bool().unwrap_or(false);
        if ingest.spots_live {
            ingest.spots = spots["spots"].as_u64();
        }
    }
    ingest.identities = snap["identities"].as_u64();
    ingest.immutable = snap["immutable"].as_u64();
    ingest.mutable = snap["mutable"].as_u64();
    ingest.tick_pages = snap["tick_pages"].as_u64();
    ingest.curves = snap["curves"].as_u64();
    ingest.conduits = snap["conduits"].as_u64();
    ingest.mismatches = snap["mismatches"].as_u64();
    ingest.jobs = snap["jobs"].as_u64();
    ingest.job_errors = snap["job_errors"].as_u64();
    ingest.stages = snap_stages(snap, rates);
    ingest.errors = snap["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .filter_map(|row| {
                    Some((
                        row["count"].as_u64()?,
                        row["error"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    ingest
}

pub(crate) fn control_path(paused: bool) -> &'static str {
    if paused {
        "/resume"
    } else {
        "/pause"
    }
}

pub(crate) fn overview_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = Vec::new();
    if !ingest.exporter_up {
        lines.push(ingest.notice.clone());
        lines.push(String::new());
    } else {
        lines.push("Process".into());
        let state = match ingest.paused {
            Some(true) => "paused",
            Some(false) => "running",
            None => "—",
        };
        lines.push(fact("state", state));
        lines.push(fact("uptime", &ingest.uptime));
        lines.push(fact("block", &shown(&ingest.block)));
        lines.push(String::new());
    }
    lines.extend(header_lines(ingest));
    lines.push(String::new());
    lines.extend(discovery_lines(ingest));
    lines.push(String::new());
    lines.extend(corpus_lines(ingest));
    lines.push(String::new());
    lines.extend(token_lines(ingest));
    lines.push(String::new());
    lines.extend(feed_lines(ingest));
    lines.push(String::new());
    lines.extend(spot_lines(ingest));
    lines.push(String::new());
    lines.push("Peers".into());
    lines.push(fact(
        "service",
        if ingest.peers_live {
            "running"
        } else {
            "not running"
        },
    ));
    if ingest.peers_live {
        lines.push(fact("sessions", &num(ingest.peer_sessions)));
        lines.push(fact("summaries", &num(ingest.peer_summaries)));
        lines.push(fact("sightings", &num(ingest.peer_sightings)));
    }
    lines.push(String::new());
    lines.extend(stage_table(&ingest.stages));
    if ingest.job_errors.is_some() || ingest.jobs.is_some() {
        lines.push(String::new());
        lines.push("Queue".into());
        lines.push(fact("waiting", &num(ingest.jobs)));
        lines.push(fact("in error", &num(ingest.job_errors)));
    }
    lines
}

pub(crate) fn pool_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Pools".into()];
    lines.push(fact("identities", &num(ingest.identities)));
    lines.push(fact(
        "immutable",
        &short_of(ingest.immutable, ingest.identities),
    ));
    lines.push(fact("mutable", &short_of(ingest.mutable, ingest.immutable)));
    lines.push(fact("tick pages", &num(ingest.tick_pages)));
    lines.push(fact("curves", &short_of(ingest.curves, ingest.mutable)));
    lines.push(fact("mismatches", &num(ingest.mismatches)));
    lines.push(String::new());
    lines.push("Still queued".into());
    for stage in &ingest.stages {
        if matches!(
            stage.name.as_str(),
            "immutable" | "mutable" | "ticks" | "curve"
        ) {
            lines.push(fact(&stage.name, &shown(&stage.queued)));
        }
    }
    lines.push(String::new());
    lines.push(
        "A short count is rows still missing against the count above it. A mismatch is a curve whose generation does not match the mutable row."
            .into(),
    );
    lines
}

pub(crate) fn conduit_lines(ingest: &Ingest) -> Vec<String> {
    let stage = ingest.stages.iter().find(|stage| stage.name == "conduit");
    let mut lines = vec!["Conduits".into()];
    lines.push(fact("stored", &num(ingest.conduits)));
    lines.push(fact(
        "waiting",
        stage.map(|stage| stage.queued.as_str()).unwrap_or("—"),
    ));
    lines.push(fact(
        "running",
        stage.map(|stage| stage.inflight.as_str()).unwrap_or("—"),
    ));
    lines.push(fact(
        "finished",
        stage.map(|stage| stage.jobs.as_str()).unwrap_or("—"),
    ));
    lines.push(fact(
        "batches",
        stage.map(|stage| stage.batches.as_str()).unwrap_or("—"),
    ));
    lines.push(fact(
        "last",
        stage.map(|stage| stage.last.as_str()).unwrap_or("—"),
    ));
    lines.push(fact(
        "rate",
        stage.map(|stage| stage.rate.as_str()).unwrap_or("—"),
    ));
    if let Some(stage) = stage {
        if !stage.spark.is_empty() {
            lines.push(fact("recent", &stage.spark));
        }
    }
    lines.push(String::new());
    lines.push("A stored conduit is one ordered token pair. Finished counts jobs this process has written.".into());
    lines
}

pub(crate) fn token_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Tokens".into()];
    lines.push(fact("stored", &num(ingest.tokens_stored)));
    if ingest.tokens_missing_known {
        let missing = match ingest.tokens_missing {
            Some(missing) if ingest.tokens_capped => format!("at least {}", commas(missing)),
            Some(missing) => commas(missing),
            None => "—".into(),
        };
        lines.push(fact("missing", &missing));
    } else if !ingest.exporter_up {
        lines.push(fact("missing", "—"));
    }
    if ingest.tokens_reading {
        lines.push(fact("lens", "reading"));
    } else if ingest.tokens_stored.is_some() || ingest.tokens_missing_known {
        lines.push(fact("lens", "idle"));
    }
    if ingest.tokens_capped {
        lines.push("The lens stopped at its scan limit, so more tokens are missing.".into());
    }
    lines
}

pub(crate) fn feed_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Feeds".into()];
    if !ingest.feeds_live && ingest.feeds.is_none() {
        lines.push(fact("service", "not running"));
        return lines;
    }
    lines.push(fact("feeds", &num(ingest.feeds)));
    lines.push(fact("configured", &num(ingest.feeds_configured)));
    lines.push(fact("answers", &num(ingest.feeds_answers)));
    if let (Some(feeds), Some(answers)) = (ingest.feeds, ingest.feeds_answers) {
        if feeds > answers {
            lines.push(fact("unanswered", &commas(feeds - answers)));
        }
    }
    lines.push(fact("history", &num(ingest.feeds_jobs)));
    if ingest.feeds_follow_known {
        lines.push(fact("follow", &num(ingest.feeds_follow)));
        if let (Some(follow), Some(header)) = (ingest.feeds_follow, ingest.chain_number) {
            if header > follow {
                lines.push(fact("behind header", &commas(header - follow)));
            } else {
                lines.push(fact("behind header", "0"));
            }
        }
    }
    if ingest.feeds_live {
        lines.push(fact(
            "mode",
            if ingest.feeds_following {
                "following"
            } else {
                "lookback"
            },
        ));
    }
    if ingest.feeds_jobs.is_some_and(|jobs| jobs > 0) {
        lines.push("History jobs are feeds whose backfill has not finished.".into());
    }
    lines
}

pub(crate) fn spot_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Spots".into()];
    if !ingest.spots_live && ingest.spots.is_none() {
        lines.push(fact("service", "not running"));
        return lines;
    }
    lines.push(fact("spots", &num(ingest.spots)));
    lines.push("Each spot is a stored round trip of one token into the numeraire.".into());
    lines
}

pub(crate) fn work_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Coverage".into()];
    lines.push(fact("identities", &num(ingest.identities)));
    lines.push(fact(
        "immutable",
        &short_of(ingest.immutable, ingest.identities),
    ));
    lines.push(fact("mutable", &short_of(ingest.mutable, ingest.immutable)));
    lines.push(fact("tick pages", &num(ingest.tick_pages)));
    lines.push(fact("curves", &short_of(ingest.curves, ingest.mutable)));
    lines.push(fact("conduits", &num(ingest.conduits)));
    lines.push(fact("mismatches", &num(ingest.mismatches)));
    lines.push(String::new());
    lines.extend(stage_table(&ingest.stages));
    lines.push(String::new());
    lines.push("Stuck".into());
    if ingest.errors.is_empty() {
        let waiting = num(ingest.jobs);
        lines.push(format!(
            "{waiting} jobs are waiting. None of them is carrying an error."
        ));
    } else {
        lines.push(fact("in error", &num(ingest.job_errors)));
        lines.push("These errors are still on queued jobs.".into());
    }
    lines
}

pub(crate) fn peer_lines(ingest: &Ingest, sessions: &[Session], down: bool) -> Vec<String> {
    let mut lines = vec!["Peers".into()];
    if down {
        lines.push(fact("service", "down"));
    } else if !ingest.peers_live && sessions.is_empty() {
        lines.push(fact("service", "not running"));
    } else {
        lines.push(fact("service", "running"));
    }
    lines.push(fact("sessions", &sessions.len().to_string()));
    let best = sessions.iter().filter_map(|session| session.head).max();
    lines.push(fact("best head", &num(best)));
    lines.push(fact("stored header", &num(ingest.chain_number)));
    match (best, ingest.chain_number) {
        (Some(best), Some(stored)) if stored < best => {
            lines.push(fact("chain behind", &commas(best - stored)));
        }
        (Some(best), Some(stored)) if stored > best => {
            lines.push(fact("peers behind", &commas(stored - best)));
        }
        (Some(_), Some(_)) => lines.push(fact("chain", "at the best head")),
        _ => {}
    }
    if let Some(best) = best {
        let at_tip = sessions
            .iter()
            .filter(|session| session.head == Some(best))
            .count();
        lines.push(fact("at the tip", &at_tip.to_string()));
    }
    let windows: Vec<u64> = sessions
        .iter()
        .filter_map(|session| session.earliest)
        .collect();
    if let Some(oldest) = windows.iter().copied().min() {
        lines.push(fact("oldest window", &commas(oldest)));
        lines.push(fact(
            "windows",
            &format!("{} of {}", windows.len(), sessions.len()),
        ));
    }
    if ingest.peers_live {
        lines.push(fact("known peers", &num(ingest.peer_summaries)));
        lines.push(fact("sightings", &num(ingest.peer_sightings)));
    }
    lines.push(String::new());
    lines.push(
        "A fetch uses a session at the best head. Sessions further behind are connected and are not the first choice."
            .into(),
    );
    lines
}

pub(crate) fn peer_detail(session: &Session, sessions: &[Session], stored: Option<u64>) -> String {
    let best = sessions.iter().filter_map(|row| row.head).max();
    let mut lines = vec![session.label.clone(), String::new()];
    lines.push(fact("eth", &session.eth.to_string()));
    lines.push(fact("head", &num(session.head)));
    lines.push(fact("earliest", &num(session.earliest)));
    if let (Some(head), Some(best)) = (session.head, best) {
        lines.push(fact("behind best", &commas(best.saturating_sub(head))));
        if head == best {
            lines.push(fact("fetch", "preferred"));
        } else {
            lines.push(fact("fetch", "later"));
        }
    } else {
        lines.push(fact("fetch", "no head"));
    }
    if let (Some(head), Some(stored)) = (session.head, stored) {
        if head >= stored {
            lines.push(fact("ahead of chain", &commas(head - stored)));
        } else {
            lines.push(fact("behind chain", &commas(stored - head)));
        }
    }
    lines.join("\n")
}

pub(crate) fn error_detail(count: u64, error: &str) -> String {
    format!(
        "{}\n\n{} queued jobs still carry this error.\nThe count falls as those jobs finish.",
        error,
        commas(count)
    )
}

fn header_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Headers".into()];
    if ingest.headers_live {
        lines.push(fact("cursor", &num(ingest.cursor)));
        if ingest.head_known {
            lines.push(fact("head", &num(ingest.head)));
            let behind = ingest
                .head
                .unwrap_or(0)
                .saturating_sub(ingest.cursor.unwrap_or(0));
            lines.push(fact("behind", &commas(behind)));
        } else {
            lines.push(fact("head", "unknown"));
        }
    } else if ingest.exporter_up {
        lines.push(fact("sync", "not running"));
    }
    match ingest.chain_number {
        Some(number) => {
            lines.push(fact("stored", &commas(number)));
            if let Some(time) = ingest.chain_time {
                lines.push(fact("time", &time.to_string()));
            }
            if !ingest.chain_hash.is_empty() {
                lines.push(fact("hash", &short_hash(&ingest.chain_hash)));
            }
        }
        None => lines.push(fact("stored", "not stored")),
    }
    lines
}

fn discovery_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Discovery".into()];
    if ingest.exporter_up {
        lines.push(fact("identities", &num(ingest.identities)));
    }
    if ingest.cursors.is_empty() {
        lines.push(fact("cursors", "none"));
    }
    for (name, block) in &ingest.cursors {
        lines.push(fact(name, &commas(*block)));
    }
    lines
}

fn corpus_lines(ingest: &Ingest) -> Vec<String> {
    let mut lines = vec!["Corpus".into()];
    match ingest.corpus_bytes {
        Some(bytes) => {
            lines.push(fact("size", &format_bytes(bytes)));
            lines.push(fact("holes", &num(ingest.corpus_holes)));
        }
        None => lines.push(fact("size", "—")),
    }
    lines
}

fn stage_table(stages: &[StageRow]) -> Vec<String> {
    if stages.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "State".into(),
        format!(
            "  {:<12}{:>10}{:>10}{:>10}{:>10}{:>12}{:>10}",
            "stage", "waiting", "running", "finished", "batches", "last", "rate"
        ),
    ];
    for stage in stages {
        lines.push(format!(
            "  {:<12}{:>10}{:>10}{:>10}{:>10}{:>12}{:>10}",
            stage.name,
            shown(&stage.queued),
            shown(&stage.inflight),
            shown(&stage.jobs),
            shown(&stage.batches),
            shown(&stage.last),
            shown(&stage.rate)
        ));
        if !stage.spark.is_empty() {
            lines.push(format!("  {:<12}{}", "", stage.spark));
        }
    }
    lines
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
            inflight: String::new(),
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
                        inflight: stage["inflight"].as_u64().unwrap_or(0).to_string(),
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

fn board<'a>(snap: &'a Value, name: &str) -> Option<&'a Value> {
    snap.get("board").and_then(|board| board.get(name))
}

fn fact(label: &str, value: &str) -> String {
    format!("  {label:<16}{value}")
}

fn num(value: Option<u64>) -> String {
    match value {
        Some(value) => commas(value),
        None => "—".into(),
    }
}

fn shown(value: &str) -> String {
    if value.is_empty() {
        "—".into()
    } else {
        value.to_string()
    }
}

fn short_of(have: Option<u64>, against: Option<u64>) -> String {
    let Some(have) = have else {
        return "—".into();
    };
    match against {
        Some(against) if have < against => {
            format!("{}    {} short", commas(have), commas(against - have))
        }
        _ => commas(have),
    }
}

fn short_hash(hash: &str) -> String {
    if hash.len() <= 14 {
        hash.to_string()
    } else {
        format!("{}…", &hash[..12])
    }
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
