//! JSON-line-plus-raw-body codec for the entity store socket.
//!
//! One JSON object, then a raw body when `bytes` is set. This crate does not
//! dial and it does not open a database.

use alloy_primitives::B256;
use eyre::eyre;
use model::{
    decode_block_records, decode_header_records, encode_block_record, encode_header_records,
    BlockRecord, HeaderRecord,
};
use serde_json::{json, Value};
use venues::{decode_body, decode_row, encode_body, encode_row, Put, ReceiptBody, VenueRow};

/// `scan_blocks` returns at most this many corpus rows.
pub const SCAN_PAGE: u64 = 64;
/// `read_headers` and `hits_from` return at most this many records.
pub const READ_CAP: u64 = 8192;
/// A `bytes` field is the allocation size. Reject anything larger.
pub const MAX_BODY: usize = 256 * 1024 * 1024;

const ROW_HEAD: usize = 1 + 32 + 4;
const SCAN_HEAD: usize = 8 + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    HeaderCursor,
    HeaderParent {
        next: u64,
    },
    AppendHeaders {
        records: Vec<HeaderRecord>,
    },
    ReadHeaders {
        from: u64,
        limit: u64,
    },
    ReadBlock {
        number: u64,
    },
    PutBlocks {
        records: Vec<BlockRecord>,
    },
    WriteRecords {
        cursor_key: String,
        records: Vec<BlockRecord>,
    },
    WriteHole {
        number: u64,
    },
    Meta {
        key: String,
    },
    PutMeta {
        key: String,
        value: u64,
    },
    ScanBlocks {
        after: Option<u64>,
        limit: u64,
    },
    ScanVenues {
        kind: u8,
        after: Option<B256>,
        limit: u64,
    },
    Cursor {
        job: String,
    },
    CommitChunk {
        job: String,
        next: u64,
        rows: Vec<VenueRow>,
        put: Put,
    },
    HitScan {
        job: String,
    },
    HitsFrom {
        job: String,
        from: u64,
        to: u64,
    },
    CommitHits {
        job: String,
        scanned_to: u64,
        hits: Vec<u64>,
    },
    ReadBodies {
        numbers: Vec<u64>,
    },
    PutBodies {
        bodies: Vec<(u64, ReceiptBody)>,
    },
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::HeaderCursor => "header_cursor",
            Op::HeaderParent { .. } => "header_parent",
            Op::AppendHeaders { .. } => "append_headers",
            Op::ReadHeaders { .. } => "read_headers",
            Op::ReadBlock { .. } => "read_block",
            Op::PutBlocks { .. } => "put_blocks",
            Op::WriteRecords { .. } => "write_records",
            Op::WriteHole { .. } => "write_hole",
            Op::Meta { .. } => "meta",
            Op::PutMeta { .. } => "put_meta",
            Op::ScanBlocks { .. } => "scan_blocks",
            Op::ScanVenues { .. } => "scan_venues",
            Op::Cursor { .. } => "cursor",
            Op::CommitChunk { .. } => "commit_chunk",
            Op::HitScan { .. } => "hit_scan",
            Op::HitsFrom { .. } => "hits_from",
            Op::CommitHits { .. } => "commit_hits",
            Op::ReadBodies { .. } => "read_bodies",
            Op::PutBodies { .. } => "put_bodies",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    HeaderCursor(u64),
    Parent(B256),
    Appended { next: u64, hash: B256 },
    Headers(Vec<HeaderRecord>),
    Block(Option<BlockRecord>),
    Wrote(u64),
    Empty,
    Meta(Option<u64>),
    Cursor(Option<u64>),
    Scan(Vec<(u64, Vec<u8>)>),
    Venues(Vec<VenueRow>),
    HitScan(Option<u64>),
    Hits(Vec<u64>),
    Bodies(Vec<Option<ReceiptBody>>),
}

/// How many raw bytes follow the JSON line. Absent means none.
pub fn payload_len(value: &Value) -> eyre::Result<usize> {
    let Some(len) = value.get("bytes") else {
        return Ok(0);
    };
    let len = len.as_u64().ok_or_else(|| eyre!("bytes"))?;
    let len = usize::try_from(len).map_err(|_| eyre!("bytes"))?;
    if len > MAX_BODY {
        return Err(eyre!("body is {len} bytes"));
    }
    Ok(len)
}

pub fn encode_request(op: &Op) -> eyre::Result<(Value, Vec<u8>)> {
    let encoded = match op {
        Op::HeaderCursor => (json!({"op": "header_cursor"}), Vec::new()),
        Op::HeaderParent { next } => (json!({"op": "header_parent", "next": next}), Vec::new()),
        Op::AppendHeaders { records } => {
            let body = encode_header_records(records);
            let bytes = body.len();
            (json!({"op": "append_headers", "bytes": bytes}), body)
        }
        Op::ReadHeaders { from, limit } => (
            json!({"op": "read_headers", "from": from, "limit": limit}),
            Vec::new(),
        ),
        Op::ReadBlock { number } => (json!({"op": "read_block", "number": number}), Vec::new()),
        Op::PutBlocks { records } => with_body("put_blocks", json!({}), encode_blocks(records)),
        Op::WriteRecords {
            cursor_key,
            records,
        } => with_body(
            "write_records",
            json!({"cursor_key": cursor_key}),
            encode_blocks(records),
        ),
        Op::WriteHole { number } => (json!({"op": "write_hole", "number": number}), Vec::new()),
        Op::Meta { key } => (json!({"op": "meta", "key": key}), Vec::new()),
        Op::PutMeta { key, value } => (
            json!({"op": "put_meta", "key": key, "value": value}),
            Vec::new(),
        ),
        Op::ScanBlocks { after, limit } => {
            let mut value = json!({"op": "scan_blocks", "limit": limit});
            if let Some(after) = after {
                value["after"] = json!(after);
            }
            (value, Vec::new())
        }
        Op::ScanVenues { kind, after, limit } => {
            let mut value = json!({"op": "scan_venues", "kind": kind, "limit": limit});
            if let Some(after) = after {
                value["after"] = json!(after);
            }
            (value, Vec::new())
        }
        Op::Cursor { job } => (json!({"op": "cursor", "job": job}), Vec::new()),
        Op::CommitChunk {
            job,
            next,
            rows,
            put,
        } => with_body(
            "commit_chunk",
            json!({"job": job, "next": next, "put": put_name(*put)}),
            encode_rows(rows)?,
        ),
        Op::HitScan { job } => (json!({"op": "hit_scan", "job": job}), Vec::new()),
        Op::HitsFrom { job, from, to } => (
            json!({"op": "hits_from", "job": job, "from": from, "to": to}),
            Vec::new(),
        ),
        Op::CommitHits {
            job,
            scanned_to,
            hits,
        } => with_body(
            "commit_hits",
            json!({"job": job, "scanned_to": scanned_to}),
            encode_u64s(hits),
        ),
        Op::ReadBodies { numbers } => with_body("read_bodies", json!({}), encode_u64s(numbers)),
        Op::PutBodies { bodies } => with_body("put_bodies", json!({}), encode_put_bodies(bodies)?),
    };
    Ok(encoded)
}

pub fn decode_request(value: &Value, body: &[u8]) -> eyre::Result<Op> {
    let name = value
        .get("op")
        .and_then(|op| op.as_str())
        .ok_or_else(|| eyre!("missing op"))?;
    if name == "put_meta" && !body.is_empty() {
        return Err(eyre!("put_meta has no body"));
    }
    let op = match name {
        "header_cursor" => plain(body, Op::HeaderCursor)?,
        "header_parent" => plain(
            body,
            Op::HeaderParent {
                next: field_u64(value, "next")?,
            },
        )?,
        "append_headers" => Op::AppendHeaders {
            records: decode_header_records(body).map_err(|err| eyre!("{err}"))?,
        },
        "read_headers" => plain(
            body,
            Op::ReadHeaders {
                from: field_u64(value, "from")?,
                limit: field_u64(value, "limit")?.min(READ_CAP),
            },
        )?,
        "read_block" => plain(
            body,
            Op::ReadBlock {
                number: field_u64(value, "number")?,
            },
        )?,
        "put_blocks" => Op::PutBlocks {
            records: decode_block_records(body).map_err(|err| eyre!("{err}"))?,
        },
        "write_records" => Op::WriteRecords {
            cursor_key: field_str(value, "cursor_key")?,
            records: decode_block_records(body).map_err(|err| eyre!("{err}"))?,
        },
        "write_hole" => plain(
            body,
            Op::WriteHole {
                number: field_u64(value, "number")?,
            },
        )?,
        "meta" => plain(
            body,
            Op::Meta {
                key: field_str(value, "key")?,
            },
        )?,
        "put_meta" => Op::PutMeta {
            key: field_str(value, "key")?,
            value: field_u64(value, "value")?,
        },
        "scan_blocks" => {
            let after = match value.get("after") {
                None => None,
                Some(after) => Some(after.as_u64().ok_or_else(|| eyre!("after"))?),
            };
            let limit = match value.get("limit") {
                None => SCAN_PAGE,
                Some(limit) => limit.as_u64().ok_or_else(|| eyre!("limit"))?.min(SCAN_PAGE),
            };
            plain(body, Op::ScanBlocks { after, limit })?
        }
        "scan_venues" => {
            let kind = u8::try_from(field_u64(value, "kind")?).map_err(|_| eyre!("kind"))?;
            let after = match value.get("after") {
                None => None,
                Some(after) => {
                    let text = after.as_str().ok_or_else(|| eyre!("after"))?;
                    Some(text.parse().map_err(|_| eyre!("after"))?)
                }
            };
            let limit = match value.get("limit") {
                None => SCAN_PAGE,
                Some(limit) => limit.as_u64().ok_or_else(|| eyre!("limit"))?.min(SCAN_PAGE),
            };
            plain(body, Op::ScanVenues { kind, after, limit })?
        }
        "cursor" => plain(
            body,
            Op::Cursor {
                job: field_str(value, "job")?,
            },
        )?,
        "commit_chunk" => Op::CommitChunk {
            job: field_str(value, "job")?,
            next: field_u64(value, "next")?,
            rows: decode_rows(body)?,
            put: decode_put(value)?,
        },
        "hit_scan" => plain(
            body,
            Op::HitScan {
                job: field_str(value, "job")?,
            },
        )?,
        "hits_from" => plain(
            body,
            Op::HitsFrom {
                job: field_str(value, "job")?,
                from: field_u64(value, "from")?,
                to: field_u64(value, "to")?,
            },
        )?,
        "commit_hits" => Op::CommitHits {
            job: field_str(value, "job")?,
            scanned_to: field_u64(value, "scanned_to")?,
            hits: decode_u64s(body)?,
        },
        "read_bodies" => {
            let numbers = decode_u64s(body)?;
            if numbers.len() > READ_CAP as usize {
                return Err(eyre!("read_bodies cap is {READ_CAP}"));
            }
            Op::ReadBodies { numbers }
        }
        "put_bodies" => Op::PutBodies {
            bodies: decode_put_bodies(body)?,
        },
        other => return Err(eyre!("unknown op {other}")),
    };
    Ok(op)
}

pub fn encode_response(reply: &Reply) -> eyre::Result<(Value, Vec<u8>)> {
    let encoded = match reply {
        Reply::HeaderCursor(cursor) => (json!({"ok": true, "cursor": cursor}), Vec::new()),
        Reply::Parent(hash) => (json!({"ok": true, "hash": hash.to_string()}), Vec::new()),
        Reply::Appended { next, hash } => (
            json!({"ok": true, "next": next, "hash": hash.to_string()}),
            Vec::new(),
        ),
        Reply::Headers(records) => {
            let body = encode_header_records(records);
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
        Reply::Block(None) => (json!({"ok": true}), Vec::new()),
        Reply::Block(Some(record)) => {
            let body = encode_block_record(record);
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
        Reply::Wrote(n) => (json!({"ok": true, "wrote": n}), Vec::new()),
        Reply::Empty => (json!({"ok": true}), Vec::new()),
        Reply::Meta(None) | Reply::Cursor(None) | Reply::HitScan(None) => {
            (json!({"ok": true}), Vec::new())
        }
        Reply::Meta(Some(value)) => (json!({"ok": true, "value": value}), Vec::new()),
        Reply::Cursor(Some(cursor)) => (json!({"ok": true, "cursor": cursor}), Vec::new()),
        Reply::HitScan(Some(cursor)) => (json!({"ok": true, "cursor": cursor}), Vec::new()),
        Reply::Scan(rows) => {
            let body = encode_scan(rows)?;
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
        Reply::Venues(rows) => {
            let body = encode_rows(rows)?;
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
        Reply::Hits(hits) => {
            let body = encode_u64s(hits);
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
        Reply::Bodies(bodies) => {
            let body = encode_stored_bodies(bodies)?;
            let bytes = body.len();
            (json!({"ok": true, "bytes": bytes}), body)
        }
    };
    Ok(encoded)
}

pub fn encode_error(message: &str) -> Value {
    json!({"ok": false, "error": message})
}

pub fn decode_response(op: &str, value: &Value, body: &[u8]) -> eyre::Result<Reply> {
    match value.get("ok").and_then(|ok| ok.as_bool()) {
        Some(true) => {}
        Some(false) => {
            let message = value
                .get("error")
                .and_then(|err| err.as_str())
                .unwrap_or("request failed");
            return Err(eyre!("{message}"));
        }
        None => return Err(eyre!("response has no ok")),
    }
    if value.get("bytes").is_some() {
        if payload_len(value)? != body.len() {
            return Err(eyre!("response bytes"));
        }
    } else if !body.is_empty() {
        return Err(eyre!("unexpected body"));
    }
    match op {
        "header_cursor" => {
            no_body(body)?;
            Ok(Reply::HeaderCursor(field_u64(value, "cursor")?))
        }
        "header_parent" => {
            no_body(body)?;
            Ok(Reply::Parent(parse_hash(value)?))
        }
        "append_headers" => {
            no_body(body)?;
            Ok(Reply::Appended {
                next: field_u64(value, "next")?,
                hash: parse_hash(value)?,
            })
        }
        "read_headers" => {
            let records = decode_header_records(body).map_err(|err| eyre!("{err}"))?;
            if records.len() > READ_CAP as usize {
                return Err(eyre!("read_headers cap is {READ_CAP}"));
            }
            Ok(Reply::Headers(records))
        }
        "read_block" => {
            if body.is_empty() {
                return Ok(Reply::Block(None));
            }
            let records = decode_block_records(body).map_err(|err| eyre!("{err}"))?;
            let mut records = records.into_iter();
            let Some(record) = records.next() else {
                return Ok(Reply::Block(None));
            };
            if records.next().is_some() {
                return Err(eyre!("read_block returned more than one record"));
            }
            Ok(Reply::Block(Some(record)))
        }
        "put_blocks" | "write_records" | "commit_chunk" => {
            no_body(body)?;
            Ok(Reply::Wrote(field_u64(value, "wrote")?))
        }
        "write_hole" | "put_meta" | "put_bodies" | "commit_hits" => {
            no_body(body)?;
            Ok(Reply::Empty)
        }
        "meta" => {
            no_body(body)?;
            Ok(Reply::Meta(optional_u64(value, "value")?))
        }
        "cursor" => {
            no_body(body)?;
            Ok(Reply::Cursor(optional_u64(value, "cursor")?))
        }
        "hit_scan" => {
            no_body(body)?;
            Ok(Reply::HitScan(optional_u64(value, "cursor")?))
        }
        "scan_blocks" => {
            let rows = decode_scan(body)?;
            if rows.len() > SCAN_PAGE as usize {
                return Err(eyre!("scan page is larger than {SCAN_PAGE}"));
            }
            Ok(Reply::Scan(rows))
        }
        "scan_venues" => {
            let rows = decode_rows(body)?;
            if rows.len() > SCAN_PAGE as usize {
                return Err(eyre!("scan page is larger than {SCAN_PAGE}"));
            }
            Ok(Reply::Venues(rows))
        }
        "hits_from" => {
            let hits = decode_u64s(body)?;
            if hits.len() > READ_CAP as usize {
                return Err(eyre!("hits cap is {READ_CAP}"));
            }
            Ok(Reply::Hits(hits))
        }
        "read_bodies" => Ok(Reply::Bodies(decode_stored_bodies(body)?)),
        other => Err(eyre!("unexpected reply for {other}")),
    }
}

fn with_body(op: &str, mut fields: Value, body: Vec<u8>) -> (Value, Vec<u8>) {
    fields["op"] = json!(op);
    fields["bytes"] = json!(body.len());
    (fields, body)
}

fn plain(body: &[u8], op: Op) -> eyre::Result<Op> {
    if body.is_empty() {
        Ok(op)
    } else {
        Err(eyre!("{} has no body", op.name()))
    }
}

fn no_body(body: &[u8]) -> eyre::Result<()> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(eyre!("unexpected body"))
    }
}

fn field_u64(value: &Value, field: &str) -> eyre::Result<u64> {
    value
        .get(field)
        .and_then(|field| field.as_u64())
        .ok_or_else(|| eyre!("missing {field}"))
}

fn optional_u64(value: &Value, field: &str) -> eyre::Result<Option<u64>> {
    match value.get(field) {
        None => Ok(None),
        Some(field) => field
            .as_u64()
            .map(Some)
            .ok_or_else(|| eyre!("missing number")),
    }
}

fn field_str(value: &Value, field: &str) -> eyre::Result<String> {
    value
        .get(field)
        .and_then(|field| field.as_str())
        .map(str::to_string)
        .ok_or_else(|| eyre!("missing {field}"))
}

fn parse_hash(value: &Value) -> eyre::Result<B256> {
    let text = value
        .get("hash")
        .and_then(|hash| hash.as_str())
        .ok_or_else(|| eyre!("hash"))?;
    text.parse().map_err(|_| eyre!("hash {text}"))
}

fn put_name(put: Put) -> &'static str {
    match put {
        Put::Merge => "merge",
        Put::IfAbsent => "if_absent",
    }
}

fn decode_put(value: &Value) -> eyre::Result<Put> {
    match value.get("put").and_then(|put| put.as_str()) {
        Some("merge") => Ok(Put::Merge),
        Some("if_absent") => Ok(Put::IfAbsent),
        _ => Err(eyre!("commit_chunk put")),
    }
}

fn encode_blocks(records: &[BlockRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&encode_block_record(record));
    }
    out
}

fn encode_rows(rows: &[VenueRow]) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    for row in rows {
        let encoded = encode_row(row)?;
        let len = u32::try_from(encoded.len()).map_err(|_| eyre!("venue row"))?;
        out.push(row.kind);
        out.extend_from_slice(row.id.as_slice());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&encoded);
    }
    Ok(out)
}

fn decode_rows(mut bytes: &[u8]) -> eyre::Result<Vec<VenueRow>> {
    let mut rows = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < ROW_HEAD {
            return Err(eyre!("venue row truncated"));
        }
        let kind = bytes[0];
        let id = B256::from_slice(&bytes[1..33]);
        let len = u32::from_le_bytes(bytes[33..37].try_into().unwrap()) as usize;
        let end = ROW_HEAD + len;
        if bytes.len() < end {
            return Err(eyre!("venue row truncated"));
        }
        rows.push(decode_row(kind, id, &bytes[ROW_HEAD..end])?);
        bytes = &bytes[end..];
    }
    Ok(rows)
}

fn encode_u64s(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn decode_u64s(bytes: &[u8]) -> eyre::Result<Vec<u64>> {
    let (words, rest) = bytes.as_chunks::<8>();
    if !rest.is_empty() {
        return Err(eyre!("u64 list"));
    }
    Ok(words.iter().map(|word| u64::from_le_bytes(*word)).collect())
}

fn encode_put_bodies(bodies: &[(u64, ReceiptBody)]) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    for (number, body) in bodies {
        let encoded = encode_body(body);
        let len = u32::try_from(encoded.len()).map_err(|_| eyre!("receipt body"))?;
        out.extend_from_slice(&number.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&encoded);
    }
    Ok(out)
}

fn decode_put_bodies(mut bytes: &[u8]) -> eyre::Result<Vec<(u64, ReceiptBody)>> {
    let mut bodies = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 12 {
            return Err(eyre!("receipt body truncated"));
        }
        let number = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let end = 12 + len;
        if bytes.len() < end {
            return Err(eyre!("receipt body truncated"));
        }
        bodies.push((number, decode_body(&bytes[12..end])?));
        bytes = &bytes[end..];
    }
    Ok(bodies)
}

fn encode_stored_bodies(bodies: &[Option<ReceiptBody>]) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    for body in bodies {
        match body {
            None => out.push(0),
            Some(body) => {
                out.push(1);
                let encoded = encode_body(body);
                let len = u32::try_from(encoded.len()).map_err(|_| eyre!("receipt body"))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&encoded);
            }
        }
    }
    Ok(out)
}

fn decode_stored_bodies(mut bytes: &[u8]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
    let mut bodies = Vec::new();
    while !bytes.is_empty() {
        match bytes[0] {
            0 => {
                bodies.push(None);
                bytes = &bytes[1..];
            }
            1 => {
                if bytes.len() < 5 {
                    return Err(eyre!("receipt body truncated"));
                }
                let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
                let end = 5 + len;
                if bytes.len() < end {
                    return Err(eyre!("receipt body truncated"));
                }
                bodies.push(Some(decode_body(&bytes[5..end])?));
                bytes = &bytes[end..];
            }
            _ => return Err(eyre!("receipt body tag")),
        }
    }
    Ok(bodies)
}

/// `u64` LE block number, `u32` LE length, then that many bytes. Length 0 is a hole.
fn encode_scan(rows: &[(u64, Vec<u8>)]) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    for (number, bytes) in rows {
        let len = u32::try_from(bytes.len()).map_err(|_| eyre!("scan record"))?;
        out.extend_from_slice(&number.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

fn decode_scan(mut bytes: &[u8]) -> eyre::Result<Vec<(u64, Vec<u8>)>> {
    let mut rows = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < SCAN_HEAD {
            return Err(eyre!("scan page truncated"));
        }
        let number = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let end = SCAN_HEAD + len;
        if bytes.len() < end {
            return Err(eyre!("scan page truncated"));
        }
        rows.push((number, bytes[SCAN_HEAD..end].to_vec()));
        bytes = &bytes[end..];
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, Address};

    fn row() -> VenueRow {
        VenueRow {
            kind: 1,
            id: VenueRow::address_id(address!("1111111111111111111111111111111111111111")),
            factory: Address::ZERO,
            tokens: Vec::new(),
            fee: None,
            tick_spacing: None,
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        }
    }

    #[test]
    fn put_meta_is_json_only() {
        let op = Op::PutMeta {
            key: "cursor-univ3-launch".to_string(),
            value: 12_369_621,
        };
        let (value, body) = encode_request(&op).unwrap();
        assert!(body.is_empty());
        assert!(value.get("bytes").is_none());
        assert_eq!(decode_request(&value, &body).unwrap(), op);
        let err = decode_request(&value, &[1]).unwrap_err();
        assert!(err.to_string().contains("no body"));
    }

    #[test]
    fn scan_page_keeps_a_hole_and_caps_the_request() {
        let rows = vec![(4_u64, Vec::new()), (7, vec![9, 8, 7])];
        let (value, body) = encode_response(&Reply::Scan(rows.clone())).unwrap();
        let decoded = decode_response("scan_blocks", &value, &body).unwrap();
        assert_eq!(decoded, Reply::Scan(rows));

        let request = json!({"op": "scan_blocks", "limit": 1000});
        let op = decode_request(&request, &[]).unwrap();
        assert_eq!(
            op,
            Op::ScanBlocks {
                after: None,
                limit: SCAN_PAGE
            }
        );
        let request = json!({"op": "read_headers", "from": 3, "limit": 90_000});
        let op = decode_request(&request, &[]).unwrap();
        assert_eq!(
            op,
            Op::ReadHeaders {
                from: 3,
                limit: READ_CAP
            }
        );
    }

    #[test]
    fn commit_chunk_row_round_trip() {
        let op = Op::CommitChunk {
            job: "list".to_string(),
            next: 10,
            rows: vec![row()],
            put: Put::Merge,
        };
        let (value, body) = encode_request(&op).unwrap();
        assert_eq!(decode_request(&value, &body).unwrap(), op);
    }

    #[test]
    fn scan_venues_uses_the_commit_chunk_row_frame() {
        let mut row = row();
        row.kind = 5;
        row.block_hash = Some(b256!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        row.tx_index = Some(2);
        row.log_index = Some(5);
        let id = row.id;
        let (value, body) = encode_response(&Reply::Venues(vec![row.clone()])).unwrap();
        let decoded = decode_response("scan_venues", &value, &body).unwrap();
        assert_eq!(decoded, Reply::Venues(vec![row]));
        assert_eq!(body[ROW_HEAD], 2);
        let request = json!({"op": "scan_venues", "kind": 5});
        assert_eq!(
            decode_request(&request, &[]).unwrap(),
            Op::ScanVenues {
                kind: 5,
                after: None,
                limit: SCAN_PAGE
            }
        );
        let request = json!({"op": "scan_venues", "kind": 5, "limit": 1000, "after": id});
        assert_eq!(
            decode_request(&request, &[]).unwrap(),
            Op::ScanVenues {
                kind: 5,
                after: Some(id),
                limit: SCAN_PAGE
            }
        );
    }

    #[test]
    fn failure_is_the_error_string() {
        let value = encode_error("header 1 parent does not match the stored chain");
        let err = decode_response("append_headers", &value, &[]).unwrap_err();
        assert!(err.to_string().contains("parent does not match"));
    }
}
