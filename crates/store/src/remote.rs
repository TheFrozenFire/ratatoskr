//! Client for the entity-store socket. One connection, one request.

use alloy_primitives::B256;
use api::{decode_response, encode_request, payload_len, Op, Reply, READ_CAP, SCAN_PAGE};
use eyre::{eyre, WrapErr};
use model::{BlockRecord, HeaderRecord};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use venues::{Put, ReceiptBody, VenueRow};

use crate::{short_header_range, EntityStore};

pub struct RemoteStore {
    socket: PathBuf,
}

impl RemoteStore {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    fn call(&self, op: Op) -> eyre::Result<Reply> {
        let name = op.name();
        let (value, body) = encode_request(&op)?;
        let mut conn = std::os::unix::net::UnixStream::connect(&self.socket)
            .wrap_err("connect to db socket")?;
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        conn.write_all(&line)?;
        if !body.is_empty() {
            conn.write_all(&body)?;
        }
        conn.flush()?;
        let mut reader = std::io::BufReader::new(conn);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(eyre!("db process closed the socket"));
        }
        let value: serde_json::Value = serde_json::from_str(line.trim())?;
        let len = payload_len(&value)?;
        let mut body = vec![0u8; len];
        if len > 0 {
            reader.read_exact(&mut body)?;
        }
        decode_response(name, &value, &body)
    }

    async fn request(&self, op: Op) -> eyre::Result<Reply> {
        let socket = self.socket.clone();
        tokio::task::spawn_blocking(move || RemoteStore { socket }.call(op))
            .await
            .map_err(|err| eyre!("db task: {err}"))?
    }
}

impl EntityStore for RemoteStore {
    async fn header_cursor(&self) -> eyre::Result<u64> {
        match self.request(Op::HeaderCursor).await? {
            Reply::HeaderCursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn header_parent(&self, next: u64) -> eyre::Result<B256> {
        match self.request(Op::HeaderParent { next }).await? {
            Reply::Parent(hash) => Ok(hash),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord> {
        let last = records
            .last()
            .cloned()
            .ok_or_else(|| eyre!("empty header batch"))?;
        match self
            .request(Op::AppendHeaders {
                records: records.to_vec(),
            })
            .await?
        {
            Reply::Appended { next, hash } => {
                if next != last.number.saturating_add(1) || hash != last.hash {
                    return Err(eyre!("header append reply does not match the batch"));
                }
                Ok(last)
            }
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64> {
        if from > to {
            return Err(eyre!("header range {from}..{to} is backwards"));
        }
        let mut seen = 0_u64;
        let mut at = from;
        while at < to {
            let limit = (to - at).min(READ_CAP);
            let batch = match self.request(Op::ReadHeaders { from: at, limit }).await? {
                Reply::Headers(batch) => batch,
                _ => return Err(eyre!("unexpected reply")),
            };
            let rows = batch.len() as u64;
            if rows != limit {
                seen += rows;
                return Err(short_header_range(seen, from, to));
            }
            visit(&batch)?;
            seen += rows;
            at = at.saturating_add(rows);
        }
        if seen != to - from {
            return Err(short_header_range(seen, from, to));
        }
        Ok(seen)
    }

    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
        match self.request(Op::ReadBlock { number }).await? {
            Reply::Block(record) => Ok(record),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        wrote(
            self.request(Op::PutBlocks {
                records: records.to_vec(),
            })
            .await?,
        )
    }

    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        wrote(
            self.request(Op::WriteRecords {
                cursor_key: cursor_key.to_string(),
                records: records.to_vec(),
            })
            .await?,
        )
    }

    async fn write_hole(&self, number: u64) -> eyre::Result<()> {
        empty(self.request(Op::WriteHole { number }).await?)
    }

    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::Meta {
                key: key.to_string(),
            })
            .await?
        {
            Reply::Meta(value) => Ok(value),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        empty(
            self.request(Op::PutMeta {
                key: key.to_string(),
                value,
            })
            .await?,
        )
    }

    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let mut after = None;
        loop {
            let page = match self
                .request(Op::ScanBlocks {
                    after,
                    limit: SCAN_PAGE,
                })
                .await?
            {
                Reply::Scan(page) => page,
                _ => return Err(eyre!("unexpected reply")),
            };
            let short = (page.len() as u64) < SCAN_PAGE;
            let end = page.last().map(|(number, _)| *number);
            for (number, bytes) in page {
                visit(number, &bytes)?;
            }
            if short {
                break;
            }
            let Some(end) = end else {
                break;
            };
            if after.is_some_and(|after| end <= after) {
                break;
            }
            after = Some(end);
        }
        Ok(())
    }

    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::Cursor {
                job: job.to_string(),
            })
            .await?
        {
            Reply::Cursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        wrote(
            self.request(Op::CommitChunk {
                job: job.to_string(),
                next,
                rows: rows.to_vec(),
                put,
            })
            .await?,
        )
    }

    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::HitScan {
                job: job.to_string(),
            })
            .await?
        {
            Reply::HitScan(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        if from >= to {
            return Ok(Vec::new());
        }
        let mut from = from;
        let mut hits = Vec::new();
        loop {
            let page = match self
                .request(Op::HitsFrom {
                    job: job.to_string(),
                    from,
                    to,
                })
                .await?
            {
                Reply::Hits(page) => page,
                _ => return Err(eyre!("unexpected reply")),
            };
            let short = (page.len() as u64) < READ_CAP;
            let Some(last) = page.last().copied() else {
                break;
            };
            let next = last.saturating_add(1);
            hits.extend(page);
            if short || next <= from || next >= to {
                break;
            }
            from = next;
        }
        Ok(hits)
    }

    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        empty(
            self.request(Op::CommitHits {
                job: job.to_string(),
                scanned_to,
                hits: hits.to_vec(),
            })
            .await?,
        )
    }

    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let mut out = Vec::with_capacity(numbers.len());
        for chunk in numbers.chunks(READ_CAP as usize) {
            let part = match self
                .request(Op::ReadBodies {
                    numbers: chunk.to_vec(),
                })
                .await?
            {
                Reply::Bodies(part) => part,
                _ => return Err(eyre!("unexpected reply")),
            };
            if part.len() != chunk.len() {
                return Err(eyre!("bodies"));
            }
            out.extend(part);
        }
        Ok(out)
    }

    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        empty(
            self.request(Op::PutBodies {
                bodies: bodies.to_vec(),
            })
            .await?,
        )
    }
}

fn wrote(reply: Reply) -> eyre::Result<u64> {
    match reply {
        Reply::Wrote(n) => Ok(n),
        _ => Err(eyre!("unexpected reply")),
    }
}

fn empty(reply: Reply) -> eyre::Result<()> {
    match reply {
        Reply::Empty => Ok(()),
        _ => Err(eyre!("unexpected reply")),
    }
}
