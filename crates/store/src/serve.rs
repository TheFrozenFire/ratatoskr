//! Unix socket adapter in front of [`LocalStore`](crate::LocalStore).
//!
//! One connection, one request. Parent checks stay in `HeaderChain`. Cursor
//! monotonicity stays in `Catalog`. `put_blocks` does not move `cursor-*`.

use api::{decode_request, encode_error, encode_response, payload_len, Op, Reply, READ_CAP};
use eyre::WrapErr;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::{EntityStore, LocalStore};

/// `~/.config/ratatoskr/db.sock`. The directory is created mode 0700.
pub fn db_socket() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").wrap_err("HOME")?;
    let dir = PathBuf::from(home).join(".config/ratatoskr");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir.join("db.sock"))
}

/// Remove a stale socket file, bind mode 0600, and serve until the listener fails.
pub async fn serve(store: Arc<LocalStore>, socket: &Path) -> eyre::Result<()> {
    if let Some(parent) = socket.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if socket.exists() {
        std::fs::remove_file(socket).wrap_err("remove stale socket")?;
    }
    let listener = UnixListener::bind(socket).wrap_err("bind db socket")?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    // `for_each_*` keeps a non-Send visitor across `.await`, so connections
    // stay on this task's `LocalSet` instead of `tokio::spawn`.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                let (conn, _) = listener.accept().await.wrap_err("accept")?;
                let store = Arc::clone(&store);
                tokio::task::spawn_local(async move {
                    if let Err(err) = handle(conn, store).await {
                        tracing::debug!(%err, "db connection");
                    }
                });
            }
        })
        .await
}

async fn handle(conn: UnixStream, store: Arc<LocalStore>) -> eyre::Result<()> {
    let (read, mut write) = conn.into_split();
    let mut lines = BufReader::new(read);
    let mut line = String::new();
    if lines.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let value: Value = match serde_json::from_str(line.trim()) {
        Ok(value) => value,
        Err(err) => return write_value(&mut write, &encode_error(&err.to_string()), &[]).await,
    };
    let len = match payload_len(&value) {
        Ok(len) => len,
        Err(err) => return write_value(&mut write, &encode_error(&err.to_string()), &[]).await,
    };
    let mut body = vec![0u8; len];
    if len > 0 {
        if let Err(err) = lines.read_exact(&mut body).await {
            return write_value(&mut write, &encode_error(&err.to_string()), &[]).await;
        }
    }
    let reply = match decode_request(&value, &body) {
        Ok(op) => dispatch(&store, op).await,
        Err(err) => Err(err),
    };
    match reply {
        Ok(reply) => {
            let (value, body) = encode_response(&reply)?;
            write_value(&mut write, &value, &body).await
        }
        Err(err) => write_value(&mut write, &encode_error(&err.to_string()), &[]).await,
    }
}

async fn write_value(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    value: &Value,
    body: &[u8],
) -> eyre::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    if !body.is_empty() {
        write.write_all(body).await?;
    }
    write.flush().await?;
    Ok(())
}

async fn dispatch(store: &LocalStore, op: Op) -> eyre::Result<Reply> {
    match op {
        Op::HeaderCursor => Ok(Reply::HeaderCursor(store.header_cursor().await?)),
        Op::HeaderParent { next } => Ok(Reply::Parent(store.header_parent(next).await?)),
        Op::AppendHeaders { records } => {
            let last = store.append_headers(&records).await?;
            Ok(Reply::Appended {
                next: last.number.saturating_add(1),
                hash: last.hash,
            })
        }
        Op::ReadHeaders { from, limit } => read_headers(store, from, limit).await,
        Op::ReadBlock { number } => Ok(Reply::Block(store.read_block(number).await?)),
        Op::PutBlocks { records } => Ok(Reply::Wrote(store.put_blocks(&records).await?)),
        Op::WriteRecords {
            cursor_key,
            records,
        } => Ok(Reply::Wrote(
            store.write_records(&cursor_key, &records).await?,
        )),
        Op::WriteHole { number } => {
            store.write_hole(number).await?;
            Ok(Reply::Empty)
        }
        Op::Meta { key } => Ok(Reply::Meta(store.meta(&key).await?)),
        Op::PutMeta { key, value } => {
            store.put_meta(&key, value).await?;
            Ok(Reply::Empty)
        }
        Op::ScanBlocks { after, limit } => {
            Ok(Reply::Scan(store.page_blocks(after, limit as usize).await?))
        }
        Op::Cursor { job } => Ok(Reply::Cursor(store.cursor(&job).await?)),
        Op::CommitChunk {
            job,
            next,
            rows,
            put,
        } => Ok(Reply::Wrote(
            store.commit_chunk(&job, next, &rows, put).await?,
        )),
        Op::HitScan { job } => Ok(Reply::HitScan(store.hit_scan(&job).await?)),
        Op::HitsFrom { job, from, to } => Ok(Reply::Hits(
            store.hits_page(&job, from, to, READ_CAP as usize).await?,
        )),
        Op::CommitHits {
            job,
            scanned_to,
            hits,
        } => {
            store.commit_hits(&job, scanned_to, &hits).await?;
            Ok(Reply::Empty)
        }
        Op::ReadBodies { numbers } => Ok(Reply::Bodies(store.read_bodies(&numbers).await?)),
        Op::PutBodies { bodies } => {
            store.put_bodies(&bodies).await?;
            Ok(Reply::Empty)
        }
    }
}

async fn read_headers(store: &LocalStore, from: u64, limit: u64) -> eyre::Result<Reply> {
    let limit = limit.min(READ_CAP);
    if limit == 0 {
        return Ok(Reply::Headers(Vec::new()));
    }
    let end = from.saturating_add(limit);
    if end == from {
        return Ok(Reply::Headers(Vec::new()));
    }
    let mut batch = Vec::new();
    store
        .for_each_header(from, end, &mut |rows| {
            batch.extend_from_slice(rows);
            Ok(())
        })
        .await?;
    Ok(Reply::Headers(batch))
}
