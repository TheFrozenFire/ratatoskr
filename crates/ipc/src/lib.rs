//! Client for the peer service unix socket.
//!
//! One request per connection. `status` and `fetch` answer with one JSON line.
//! `chunk` and `headers` answer with a JSON line and then a raw body whose
//! length is the `bytes` field.

use eyre::{eyre, WrapErr};
use model::{decode_block_records, decode_header_records, BlockRecord, HeaderRecord};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Which batch a caller asked for. The clamp differs by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchKind {
    Headers,
    Chunk,
}

/// Headers default to 512 and clamp to `1..=1024`.
/// Chunks default to 32 and clamp to `1..=64`.
///
/// `None` is an omitted socket field. Both peer adapters call this so a
/// colocated fetch and a socket fetch agree before any bytes are encoded.
pub fn clamp_count(kind: BatchKind, count: Option<u64>) -> u64 {
    match kind {
        BatchKind::Headers => count.unwrap_or(512).clamp(1, 1024),
        BatchKind::Chunk => count.unwrap_or(32).clamp(1, 64),
    }
}

/// Outbound history reads. The methods return a concrete future, not `dyn`,
/// and the trait does not use `async-trait`. `Send` is stated because an
/// `async fn` in a public trait cannot name that bound.
pub trait HistoryPeer: Send + Sync {
    fn status(&self) -> impl std::future::Future<Output = eyre::Result<Vec<SessionInfo>>> + Send;
    fn headers(
        &self,
        start: u64,
        count: u64,
    ) -> impl std::future::Future<Output = eyre::Result<Vec<HeaderRecord>>> + Send;
    fn chunk(
        &self,
        start: u64,
        count: u64,
    ) -> impl std::future::Future<Output = eyre::Result<Vec<BlockRecord>>> + Send;
}

/// Peer socket client. Decodes the header and block bodies the server already sends.
#[derive(Debug, Clone)]
pub struct SocketPeer {
    socket: PathBuf,
}

impl SocketPeer {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

impl HistoryPeer for SocketPeer {
    async fn status(&self) -> eyre::Result<Vec<SessionInfo>> {
        status(&self.socket).await
    }

    async fn headers(&self, start: u64, count: u64) -> eyre::Result<Vec<HeaderRecord>> {
        let count = clamp_count(BatchKind::Headers, Some(count));
        let body = body_call(
            &self.socket,
            &serde_json::json!({
                "op": "headers",
                "start": start,
                "count": count,
            }),
            "header",
        )
        .await?;
        decode_header_records(&body).map_err(|err| eyre!(err))
    }

    async fn chunk(&self, start: u64, count: u64) -> eyre::Result<Vec<BlockRecord>> {
        let count = clamp_count(BatchKind::Chunk, Some(count));
        let body = body_call(
            &self.socket,
            &serde_json::json!({
                "op": "chunk",
                "start": start,
                "count": count,
            }),
            "chunk",
        )
        .await?;
        decode_block_records(&body).map_err(|err| eyre!(err))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use model::{decode_block_records, decode_header_records, encode_header_records};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ratatoskr-ipc-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn header_bytes(number: u64) -> Vec<u8> {
        let mut out = vec![0u8; 369];
        out[0] = 1;
        out[1..9].copy_from_slice(&number.to_le_bytes());
        out
    }

    fn block_bytes(number: u64) -> Vec<u8> {
        let mut out = vec![0u8; 149];
        out[0] = 1;
        out[1..9].copy_from_slice(&number.to_le_bytes());
        out
    }

    #[test]
    fn clamp_defaults_and_limits() {
        assert_eq!(clamp_count(BatchKind::Headers, None), 512);
        assert_eq!(clamp_count(BatchKind::Headers, Some(0)), 1);
        assert_eq!(clamp_count(BatchKind::Headers, Some(512)), 512);
        assert_eq!(clamp_count(BatchKind::Headers, Some(5_000)), 1024);
        assert_eq!(clamp_count(BatchKind::Chunk, None), 32);
        assert_eq!(clamp_count(BatchKind::Chunk, Some(0)), 1);
        assert_eq!(clamp_count(BatchKind::Chunk, Some(32)), 32);
        assert_eq!(clamp_count(BatchKind::Chunk, Some(1_000)), 64);
    }

    #[tokio::test]
    async fn socket_peer_decodes_clamped_batches() {
        let dir = temp_dir("sock");
        let path = dir.join("peer.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (conn, _) = listener.accept().await.unwrap();
                let (read, mut write) = conn.into_split();
                let mut lines = BufReader::new(read);
                let mut line = String::new();
                lines.read_line(&mut line).await.unwrap();
                let value: Value = serde_json::from_str(line.trim()).unwrap();
                let op = value["op"].as_str().unwrap();
                if op == "status" {
                    let bytes = serde_json::to_vec(&serde_json::json!({
                        "ok": true,
                        "peers": [{
                            "peer": "enode://abc",
                            "eth": 69,
                            "head": 100,
                            "earliest": 0
                        }]
                    }))
                    .unwrap();
                    write.write_all(&bytes).await.unwrap();
                    write.write_all(b"\n").await.unwrap();
                    continue;
                }
                let body = if op == "headers" {
                    assert_eq!(value["count"].as_u64(), Some(1024));
                    assert_eq!(value["start"].as_u64(), Some(7));
                    header_bytes(7)
                } else {
                    assert_eq!(op, "chunk");
                    assert_eq!(value["count"].as_u64(), Some(64));
                    assert_eq!(value["start"].as_u64(), Some(9));
                    block_bytes(9)
                };
                let header = serde_json::to_vec(&serde_json::json!({
                    "ok": true,
                    "bytes": body.len(),
                }))
                .unwrap();
                write.write_all(&header).await.unwrap();
                write.write_all(b"\n").await.unwrap();
                write.write_all(&body).await.unwrap();
            }
        });

        let peer = SocketPeer::new(&path);
        let sessions = peer.status().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].label, "enode://abc");
        assert_eq!(sessions[0].eth, 69);
        assert_eq!(sessions[0].head, Some(100));
        assert_eq!(sessions[0].earliest, Some(0));

        let headers = peer.headers(7, 50_000).await.unwrap();
        assert_eq!(headers, decode_header_records(&header_bytes(7)).unwrap());
        assert_eq!(encode_header_records(&headers), header_bytes(7));

        let blocks = peer.chunk(9, 1_000).await.unwrap();
        assert_eq!(blocks, decode_block_records(&block_bytes(9)).unwrap());

        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
