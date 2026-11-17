//! Read-only figures for the status screen. Nothing here writes a catalog file.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use store::service::Shelf;

use super::ingest::{Local, Session};

pub(crate) fn local_of(shelf: &Shelf, exporter_up: bool) -> Local {
    let tip = shelf.tip();
    let mut local = Local {
        header_number: tip.as_ref().map(|header| header.number),
        header_timestamp: tip.as_ref().map(|header| header.timestamp),
        header_hash: tip
            .as_ref()
            .map(|header| header.hash.to_string())
            .unwrap_or_default(),
        corpus_bytes: shelf.corpus_bytes(),
        corpus_holes: shelf.corpus_holes(),
        cursors: shelf.cursors(),
        ..Local::default()
    };
    if exporter_up {
        return local;
    }
    local.token_count = shelf
        .entities
        .as_ref()
        .and_then(|entities| entities.token_count().ok());
    local.feed_count = shelf.feeds.as_ref().and_then(|feeds| feeds.count().ok());
    local.spot_count = shelf.spots.as_ref().and_then(|spots| spots.count().ok());
    if let Some(work) = shelf.work.as_ref() {
        for (index, stage) in [1_u8, 2, 3, 4, 6].into_iter().enumerate() {
            local.queued[index] = work.count_stage(stage).ok();
        }
    }
    local
}

pub(crate) fn live_peers(socket: &Path) -> Result<Vec<Session>, String> {
    let mut stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(400)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok();
    stream
        .write_all(b"{\"op\":\"status\"}\n")
        .map_err(|err| err.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| err.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|err| err.to_string())?;
    let Some(peers) = value["peers"].as_array() else {
        let err = value["error"].as_str().unwrap_or("peer service is down");
        return Err(err.to_string());
    };
    let mut sessions = peers
        .iter()
        .map(|peer| Session {
            label: peer["peer"].as_str().unwrap_or("").to_string(),
            eth: peer["eth"].as_u64().unwrap_or(0),
            head: peer["head"].as_u64(),
            earliest: peer["earliest"].as_u64(),
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .head
            .cmp(&left.head)
            .then_with(|| left.label.cmp(&right.label))
    });
    Ok(sessions)
}
