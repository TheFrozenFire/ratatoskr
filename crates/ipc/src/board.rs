//! Gauges the status view reads besides the state stages.
//!
//! Each pipeline publishes the numbers it already has. The exporter copies
//! this snapshot into the status document.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

struct Board {
    headers: AtomicBool,
    header_cursor: AtomicU64,
    header_head: AtomicU64,
    header_head_known: AtomicBool,
    corpus: AtomicBool,
    corpus_bytes: AtomicU64,
    corpus_holes: AtomicU64,
    token_records: AtomicU64,
    token_records_known: AtomicBool,
    token_missing: AtomicU64,
    token_missing_known: AtomicBool,
    token_missing_capped: AtomicBool,
    token_reading: AtomicBool,
    feeds: AtomicBool,
    feed_count: AtomicU64,
    feed_configured: AtomicU64,
    feed_answers: AtomicU64,
    feed_jobs: AtomicU64,
    feed_follow: AtomicU64,
    feed_follow_known: AtomicBool,
    feed_lookback: AtomicBool,
    peers: AtomicBool,
    peer_sessions: AtomicU64,
    peer_summaries: AtomicU64,
    peer_sightings: AtomicU64,
}

fn board() -> &'static Board {
    static BOARD: OnceLock<Board> = OnceLock::new();
    BOARD.get_or_init(|| Board {
        headers: AtomicBool::new(false),
        header_cursor: AtomicU64::new(0),
        header_head: AtomicU64::new(0),
        header_head_known: AtomicBool::new(false),
        corpus: AtomicBool::new(false),
        corpus_bytes: AtomicU64::new(0),
        corpus_holes: AtomicU64::new(0),
        token_records: AtomicU64::new(0),
        token_records_known: AtomicBool::new(false),
        token_missing: AtomicU64::new(0),
        token_missing_known: AtomicBool::new(false),
        token_missing_capped: AtomicBool::new(false),
        token_reading: AtomicBool::new(false),
        feeds: AtomicBool::new(false),
        feed_count: AtomicU64::new(0),
        feed_configured: AtomicU64::new(0),
        feed_answers: AtomicU64::new(0),
        feed_jobs: AtomicU64::new(0),
        feed_follow: AtomicU64::new(0),
        feed_follow_known: AtomicBool::new(false),
        feed_lookback: AtomicBool::new(false),
        peers: AtomicBool::new(false),
        peer_sessions: AtomicU64::new(0),
        peer_summaries: AtomicU64::new(0),
        peer_sightings: AtomicU64::new(0),
    })
}

fn load_bool(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed)
}

fn load_u64(value: &AtomicU64) -> u64 {
    value.load(Ordering::Relaxed)
}

/// Header cursor and the highest block a connected peer has advertised.
pub fn note_headers(cursor: u64, head: u64) {
    let board = board();
    board.header_cursor.store(cursor, Ordering::Relaxed);
    board.header_head.store(head, Ordering::Relaxed);
    board.header_head_known.store(true, Ordering::Relaxed);
    board.headers.store(true, Ordering::Relaxed);
}

/// Header cursor from the chain, when the sync loop has not published a head.
pub fn note_header_cursor(cursor: u64) {
    let board = board();
    board.header_cursor.store(cursor, Ordering::Relaxed);
    board.headers.store(true, Ordering::Relaxed);
}

pub fn note_corpus(bytes: u64, holes: u64) {
    let board = board();
    board.corpus_bytes.store(bytes, Ordering::Relaxed);
    board.corpus_holes.store(holes, Ordering::Relaxed);
    board.corpus.store(true, Ordering::Relaxed);
}

pub fn note_token_records(records: u64) {
    let board = board();
    board.token_records.store(records, Ordering::Relaxed);
    board.token_records_known.store(true, Ordering::Relaxed);
}

/// `capped` means the lens stopped at its scan limit, so more tokens are missing.
pub fn note_token_lens(missing: u64, capped: bool) {
    let board = board();
    board.token_missing.store(missing, Ordering::Relaxed);
    board.token_missing_capped.store(capped, Ordering::Relaxed);
    board.token_missing_known.store(true, Ordering::Relaxed);
    board.token_reading.store(true, Ordering::Relaxed);
}

pub fn note_token_reading(reading: bool) {
    board().token_reading.store(reading, Ordering::Relaxed);
}

pub fn note_feeds(
    feeds: u64,
    configured: u64,
    answers: u64,
    jobs: u64,
    follow: Option<u64>,
    lookback: bool,
) {
    let board = board();
    board.feed_count.store(feeds, Ordering::Relaxed);
    board.feed_configured.store(configured, Ordering::Relaxed);
    board.feed_answers.store(answers, Ordering::Relaxed);
    board.feed_jobs.store(jobs, Ordering::Relaxed);
    board.feed_lookback.store(lookback, Ordering::Relaxed);
    if let Some(block) = follow {
        board.feed_follow.store(block, Ordering::Relaxed);
        board.feed_follow_known.store(true, Ordering::Relaxed);
    }
    board.feeds.store(true, Ordering::Relaxed);
}

pub fn note_peers(sessions: u64, summaries: u64, sightings: u64) {
    let board = board();
    board.peer_sessions.store(sessions, Ordering::Relaxed);
    board.peer_summaries.store(summaries, Ordering::Relaxed);
    board.peer_sightings.store(sightings, Ordering::Relaxed);
    board.peers.store(true, Ordering::Relaxed);
}

pub fn snapshot() -> Value {
    let board = board();
    json!({
        "headers": {
            "live": load_bool(&board.headers),
            "cursor": load_u64(&board.header_cursor),
            "head": load_u64(&board.header_head),
            "head_known": load_bool(&board.header_head_known),
        },
        "corpus": {
            "live": load_bool(&board.corpus),
            "bytes": load_u64(&board.corpus_bytes),
            "holes": load_u64(&board.corpus_holes),
        },
        "tokens": {
            "records": load_u64(&board.token_records),
            "records_known": load_bool(&board.token_records_known),
            "missing": load_u64(&board.token_missing),
            "missing_known": load_bool(&board.token_missing_known),
            "capped": load_bool(&board.token_missing_capped),
            "reading": load_bool(&board.token_reading),
        },
        "feeds": {
            "live": load_bool(&board.feeds),
            "feeds": load_u64(&board.feed_count),
            "configured": load_u64(&board.feed_configured),
            "answers": load_u64(&board.feed_answers),
            "jobs": load_u64(&board.feed_jobs),
            "follow": load_u64(&board.feed_follow),
            "follow_known": load_bool(&board.feed_follow_known),
            "lookback": load_bool(&board.feed_lookback),
        },
        "peers": {
            "live": load_bool(&board.peers),
            "sessions": load_u64(&board.peer_sessions),
            "summaries": load_u64(&board.peer_summaries),
            "sightings": load_u64(&board.peer_sightings),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_note_is_live_and_a_header_cursor_can_arrive_without_a_head() {
        note_header_cursor(10);
        let snap = snapshot();
        assert_eq!(snap["headers"]["live"], true);
        assert_eq!(snap["headers"]["cursor"], 10);
        assert_eq!(snap["headers"]["head_known"], false);
        note_feeds(4, 3, 9, 2, Some(80), false);
        let snap = snapshot();
        assert_eq!(snap["feeds"]["live"], true);
        assert_eq!(snap["feeds"]["feeds"], 4);
        assert_eq!(snap["feeds"]["answers"], 9);
        assert_eq!(snap["feeds"]["follow"], 80);
        assert_eq!(snap["feeds"]["lookback"], false);
    }
}
