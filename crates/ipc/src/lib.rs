//! Client for the `ratatoskr-peer` unix socket.
//!
//! One request per connection. `status` and `fetch` answer with one JSON line.
//! `chunk` and `headers` answer with a JSON line and then a raw body whose
//! length is the `bytes` field.

use eyre::{eyre, WrapErr};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub fn socket_path() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").wrap_err("HOME")?;
    Ok(PathBuf::from(home).join(".config/ratatoskr/peer.sock"))
}

/// One live session, as `status` reports it.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub label: String,
    pub eth: u64,
    pub head: Option<u64>,
    pub earliest: Option<u64>,
}

/// Send one JSON request and read one JSON line.
pub async fn json_call(socket: &Path, request: &Value) -> eyre::Result<Value> {
    let mut conn = UnixStream::connect(socket).await?;
    write_request(&mut conn, request).await?;
    let mut lines = BufReader::new(conn).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| eyre!("peer process closed the socket"))?;
    Ok(serde_json::from_str(&line)?)
}

/// Send one JSON request and read the raw body that follows the header line.
///
/// `what` names the request in the missing-length error, for example `header`.
pub async fn body_call(socket: &Path, request: &Value, what: &str) -> eyre::Result<Vec<u8>> {
    let mut conn = UnixStream::connect(socket)
        .await
        .wrap_err("connect to peer socket")?;
    write_request(&mut conn, request).await?;
    let mut lines = BufReader::new(conn);
    let mut line = String::new();
    let read = lines.read_line(&mut line).await?;
    if read == 0 {
        return Err(eyre!("peer process closed the socket"));
    }
    let value: Value = serde_json::from_str(line.trim())?;
    if value.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        let err = value
            .get("error")
            .and_then(|err| err.as_str())
            .unwrap_or("request failed");
        return Err(eyre!(err.to_string()));
    }
    let len = value
        .get("bytes")
        .and_then(|len| len.as_u64())
        .ok_or_else(|| eyre!("{what} response has no length"))? as usize;
    let mut body = vec![0u8; len];
    lines.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn status(socket: &Path) -> eyre::Result<Vec<SessionInfo>> {
    let value = json_call(socket, &serde_json::json!({"op": "status"})).await?;
    let peers = value["peers"]
        .as_array()
        .ok_or_else(|| eyre!("status has no peers"))?;
    Ok(peers
        .iter()
        .map(|peer| SessionInfo {
            label: peer["peer"].as_str().unwrap_or("").to_string(),
            eth: peer["eth"].as_u64().unwrap_or(0),
            head: peer["head"].as_u64(),
            earliest: peer["earliest"].as_u64(),
        })
        .collect())
}

pub fn max_head(sessions: &[SessionInfo]) -> Option<u64> {
    sessions.iter().filter_map(|session| session.head).max()
}

/// Sessions whose advertised earliest covers `start`. A missing earliest
/// (eth/68) still counts. The result is at least 1 so a caller can keep one
/// request in flight when status itself failed to parse a useful set.
pub fn serving(sessions: &[SessionInfo], start: u64) -> usize {
    sessions
        .iter()
        .filter(|session| match session.earliest {
            Some(earliest) => earliest <= start,
            None => true,
        })
        .count()
        .max(1)
}

/// Lowest advertised earliest. [`Earliest::Open`] means some session did not
/// advertise one, so the history window is not known to be closed.
pub enum Earliest {
    Known(u64),
    Open,
}

pub fn earliest_floor(sessions: &[SessionInfo]) -> eyre::Result<Earliest> {
    if sessions.is_empty() {
        return Err(eyre!("no peers connected"));
    }
    let mut earliest = u64::MAX;
    for session in sessions {
        match session.earliest {
            Some(block) => earliest = earliest.min(block),
            None => return Ok(Earliest::Open),
        }
    }
    Ok(Earliest::Known(earliest))
}

async fn write_request(conn: &mut UnixStream, request: &Value) -> eyre::Result<()> {
    let bytes = serde_json::to_vec(request)?;
    conn.write_all(&bytes).await?;
    conn.write_all(b"\n").await?;
    Ok(())
}
