//! Client for the entity-store socket. One connection, one request.

use alloy_primitives::B256;
use api::{decode_response, encode_request, payload_len, Op, Reply, READ_CAP, SCAN_PAGE};
use eyre::{eyre, WrapErr};
use model::{BlockRecord, HeaderRecord};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use venues::{
    decode_state_reply, encode_state_call, CurveRecord, ImmutablePut, ImmutableRow, MutableStored,
    MutableWrite, PoolState, Put, QuoteRecord, ReceiptBody, StateCall, StateReply, TickPage,
    VenueRow, WatchHit, WorkFail, WorkJob,
};

use crate::{short_header_range, EntityStore};

#[derive(Clone)]
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

    /// Every stored row of `kind`, one page at a time.
    pub async fn scan_venues(&self, kind: u8) -> eyre::Result<Vec<VenueRow>> {
        let mut after = None;
        let mut rows = Vec::new();
        loop {
            let page = match self
                .request(Op::ScanVenues {
                    kind,
                    after,
                    limit: SCAN_PAGE,
                })
                .await?
            {
                Reply::Venues(page) => page,
                _ => return Err(eyre!("unexpected reply")),
            };
            let short = (page.len() as u64) < SCAN_PAGE;
            let end = page.last().map(|row| row.id);
            rows.extend(page);
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
        Ok(rows)
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

    async fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        match self.state(StateCall::ReadVenue { kind, id }).await? {
            StateReply::Venue(row) => Ok(row),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn reconcile(&self, now: u64) -> eyre::Result<u64> {
        match self.state(StateCall::Reconcile { now }).await? {
            StateReply::Wrote(n) => Ok(n),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>> {
        match self
            .state(StateCall::ClaimWork {
                stage,
                limit,
                now,
                lease_for,
            })
            .await?
        {
            StateReply::Jobs(jobs) => Ok(jobs),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::FinishWork {
                done: done.to_vec(),
                failed: failed.to_vec(),
            })
            .await?,
        )
    }

    async fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::EnqueueWork {
                jobs: jobs.to_vec(),
                now,
            })
            .await?,
        )
    }

    async fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>> {
        match self
            .state(StateCall::PutImmutable {
                rows: rows.to_vec(),
            })
            .await?
        {
            StateReply::Puts(puts) => Ok(puts),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        match self
            .state(StateCall::PutMutable { row: row.clone() })
            .await?
        {
            StateReply::Stored(stored) => Ok(stored),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutTickPages {
                pages: pages.to_vec(),
            })
            .await?,
        )
    }

    async fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutCurve {
                curve: curve.clone(),
            })
            .await?,
        )
    }

    async fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutQuote {
                quote: quote.clone(),
            })
            .await?,
        )
    }

    async fn read_state(&self, kind: u8, id: B256) -> eyre::Result<PoolState> {
        match self.state(StateCall::ReadState { kind, id }).await? {
            StateReply::State(state) => Ok(*state),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn apply_watch(&self, through: u64, hits: &[WatchHit]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::ApplyWatch {
                through,
                hits: hits.to_vec(),
            })
            .await?,
        )
    }

    async fn watch_through(&self) -> eyre::Result<Option<u64>> {
        match self.state(StateCall::WatchThrough).await? {
            StateReply::Cursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }
}

impl RemoteStore {
    async fn state(&self, call: StateCall) -> eyre::Result<StateReply> {
        let body = encode_state_call(&call)?;
        match self.request(Op::PoolState { body }).await? {
            Reply::PoolState(body) => decode_state_reply(&body),
            _ => Err(eyre!("unexpected reply")),
        }
    }
}

fn empty_state(reply: StateReply) -> eyre::Result<()> {
    match reply {
        StateReply::Empty => Ok(()),
        _ => Err(eyre!("unexpected reply")),
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
