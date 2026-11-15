//! Client for the entity-store socket. One connection, one request.

use api::{decode_response, encode_request, payload_len, Op, Reply, SCAN_PAGE};
use eyre::{eyre, WrapErr};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use store_model::VenueRow;

use crate::wire::{decode_state_reply, encode_state_call, StateCall, StateReply};

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

    pub(crate) async fn request(&self, op: Op) -> eyre::Result<Reply> {
        let socket = self.socket.clone();
        tokio::task::spawn_blocking(move || RemoteStore { socket }.call(op))
            .await
            .map_err(|err| eyre!("db task: {err}"))?
    }
}

impl RemoteStore {
    pub(crate) async fn state(&self, call: StateCall) -> eyre::Result<StateReply> {
        let body = encode_state_call(&call)?;
        match self.request(Op::PoolState { body }).await? {
            Reply::PoolState(body) => decode_state_reply(&body),
            _ => Err(eyre!("unexpected reply")),
        }
    }
}

pub(crate) fn empty_state(reply: StateReply) -> eyre::Result<()> {
    match reply {
        StateReply::Empty => Ok(()),
        _ => Err(eyre!("unexpected reply")),
    }
}

pub(crate) fn wrote(reply: Reply) -> eyre::Result<u64> {
    match reply {
        Reply::Wrote(n) => Ok(n),
        _ => Err(eyre!("unexpected reply")),
    }
}

pub(crate) fn empty(reply: Reply) -> eyre::Result<()> {
    match reply {
        Reply::Empty => Ok(()),
        _ => Err(eyre!("unexpected reply")),
    }
}
