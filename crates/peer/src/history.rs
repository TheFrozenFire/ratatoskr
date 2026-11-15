//! In-process history reads, and the JSON call a short-lived command sends
//! to the running peer service.

use eyre::eyre;
use serde_json::Value;
use std::path::Path;
use store_model::{BlockRecord, HeaderRecord};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
/// `None` is an omitted count. The socket server and [`crate::LocalPeer`]
/// both call this before a batch is fetched.
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

/// One live session, as status reports it.
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
    let bytes = serde_json::to_vec(request)?;
    conn.write_all(&bytes).await?;
    conn.write_all(b"\n").await?;
    let mut lines = BufReader::new(conn).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| eyre!("peer process closed the socket"))?;
    Ok(serde_json::from_str(&line)?)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
