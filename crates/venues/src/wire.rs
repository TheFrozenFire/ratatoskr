//! JSON body for the pool-state socket call.
//!
//! The request line stays `pool_state`. These bytes are the arguments and the
//! reply, so a tick page does not have to grow a new opcode.

use crate::detail::TickWord;
use crate::state::{
    CurveRecord, Dirty, ImmutablePut, ImmutableRow, MutableStored, MutableView, MutableWrite,
    PoolState, QuoteRecord, TickPage, WatchHit, WorkFail, WorkJob,
};
use crate::{decode_row, encode_row, VenueRow};
use alloy_primitives::B256;
use eyre::eyre;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateCall {
    ReadVenue {
        kind: u8,
        id: B256,
    },
    Reconcile {
        now: u64,
    },
    ClaimWork {
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    },
    FinishWork {
        done: Vec<(u8, u8, B256)>,
        failed: Vec<WorkFail>,
    },
    EnqueueWork {
        jobs: Vec<WorkJob>,
        now: u64,
    },
    PutImmutable {
        rows: Vec<ImmutableRow>,
    },
    PutMutable {
        row: MutableWrite,
    },
    PutTickPages {
        pages: Vec<TickPage>,
    },
    PutCurve {
        curve: CurveRecord,
    },
    PutQuote {
        quote: QuoteRecord,
    },
    ReadState {
        kind: u8,
        id: B256,
    },
    ApplyWatch {
        through: u64,
        hits: Vec<WatchHit>,
    },
    WatchThrough,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateReply {
    Venue(Option<VenueRow>),
    Wrote(u64),
    Jobs(Vec<WorkJob>),
    Empty,
    Puts(Vec<ImmutablePut>),
    Stored(MutableStored),
    State(Box<PoolState>),
    Cursor(Option<u64>),
}

pub fn encode_state_call(call: &StateCall) -> eyre::Result<Vec<u8>> {
    let value = match call {
        StateCall::ReadVenue { kind, id } => json!({"call": "read_venue", "kind": kind, "id": id}),
        StateCall::Reconcile { now } => json!({"call": "reconcile", "now": now}),
        StateCall::ClaimWork {
            stage,
            limit,
            now,
            lease_for,
        } => json!({
            "call": "claim_work",
            "stage": stage,
            "limit": limit,
            "now": now,
            "lease_for": lease_for,
        }),
        StateCall::FinishWork { done, failed } => json!({
            "call": "finish_work",
            "done": done.iter().map(|(stage, kind, id)| json!({
                "stage": stage,
                "kind": kind,
                "id": id,
            })).collect::<Vec<_>>(),
            "failed": failed.iter().map(fail_value).collect::<Vec<_>>(),
        }),
        StateCall::EnqueueWork { jobs, now } => json!({
            "call": "enqueue_work",
            "now": now,
            "jobs": jobs.iter().map(job_value).collect::<Vec<_>>(),
        }),
        StateCall::PutImmutable { rows } => json!({
            "call": "put_immutable",
            "rows": rows.iter().map(immutable_value).collect::<Vec<_>>(),
        }),
        StateCall::PutMutable { row } => json!({"call": "put_mutable", "row": mutable_value(row)}),
        StateCall::PutTickPages { pages } => json!({
            "call": "put_tick_pages",
            "pages": pages.iter().map(page_value).collect::<Vec<_>>(),
        }),
        StateCall::PutCurve { curve } => json!({"call": "put_curve", "curve": curve_value(curve)}),
        StateCall::PutQuote { quote } => json!({"call": "put_quote", "quote": quote_value(quote)}),
        StateCall::ReadState { kind, id } => json!({"call": "read_state", "kind": kind, "id": id}),
        StateCall::ApplyWatch { through, hits } => json!({
            "call": "apply_watch",
            "through": through,
            "hits": hits.iter().map(hit_value).collect::<Vec<_>>(),
        }),
        StateCall::WatchThrough => json!({"call": "watch_through"}),
    };
    Ok(serde_json::to_vec(&value)?)
}

pub fn decode_state_call(bytes: &[u8]) -> eyre::Result<StateCall> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| eyre!("state call: {err}"))?;
    let name = value
        .get("call")
        .and_then(|call| call.as_str())
        .ok_or_else(|| eyre!("state call"))?;
    let call = match name {
        "read_venue" => StateCall::ReadVenue {
            kind: field_u8(&value, "kind")?,
            id: field_b256(&value, "id")?,
        },
        "reconcile" => StateCall::Reconcile {
            now: field_u64(&value, "now")?,
        },
        "claim_work" => StateCall::ClaimWork {
            stage: field_u8(&value, "stage")?,
            limit: u32::try_from(field_u64(&value, "limit")?).map_err(|_| eyre!("limit"))?,
            now: field_u64(&value, "now")?,
            lease_for: field_u64(&value, "lease_for")?,
        },
        "finish_work" => StateCall::FinishWork {
            done: array(&value, "done")?
                .iter()
                .map(done_item)
                .collect::<eyre::Result<_>>()?,
            failed: array(&value, "failed")?
                .iter()
                .map(fail_from)
                .collect::<eyre::Result<_>>()?,
        },
        "enqueue_work" => StateCall::EnqueueWork {
            now: field_u64(&value, "now")?,
            jobs: array(&value, "jobs")?
                .iter()
                .map(job_from)
                .collect::<eyre::Result<_>>()?,
        },
        "put_immutable" => StateCall::PutImmutable {
            rows: array(&value, "rows")?
                .iter()
                .map(immutable_from)
                .collect::<eyre::Result<_>>()?,
        },
        "put_mutable" => StateCall::PutMutable {
            row: mutable_from(field(&value, "row")?)?,
        },
        "put_tick_pages" => StateCall::PutTickPages {
            pages: array(&value, "pages")?
                .iter()
                .map(page_from)
                .collect::<eyre::Result<_>>()?,
        },
        "put_curve" => StateCall::PutCurve {
            curve: curve_from(field(&value, "curve")?)?,
        },
        "put_quote" => StateCall::PutQuote {
            quote: quote_from(field(&value, "quote")?)?,
        },
        "read_state" => StateCall::ReadState {
            kind: field_u8(&value, "kind")?,
            id: field_b256(&value, "id")?,
        },
        "apply_watch" => StateCall::ApplyWatch {
            through: field_u64(&value, "through")?,
            hits: array(&value, "hits")?
                .iter()
                .map(hit_from)
                .collect::<eyre::Result<_>>()?,
        },
        "watch_through" => StateCall::WatchThrough,
        other => return Err(eyre!("unknown state call {other}")),
    };
    Ok(call)
}

pub fn encode_state_reply(reply: &StateReply) -> eyre::Result<Vec<u8>> {
    let value = match reply {
        StateReply::Venue(row) => json!({
            "reply": "venue",
            "venue": match row {
                Some(row) => venue_value(row)?,
                None => Value::Null,
            },
        }),
        StateReply::Wrote(n) => json!({"reply": "wrote", "wrote": n}),
        StateReply::Jobs(jobs) => json!({
            "reply": "jobs",
            "jobs": jobs.iter().map(job_value).collect::<Vec<_>>(),
        }),
        StateReply::Empty => json!({"reply": "empty"}),
        StateReply::Puts(puts) => json!({
            "reply": "puts",
            "puts": puts.iter().map(put_name).collect::<Vec<_>>(),
        }),
        StateReply::Stored(stored) => json!({
            "reply": "stored",
            "generation": stored.generation,
            "dirty_cleared": stored.dirty_cleared,
            "kept_ticks": stored.kept_ticks,
            "bitmap_changed": stored.bitmap_changed,
        }),
        StateReply::State(state) => json!({
            "reply": "state",
            "venue": match &state.venue {
                Some(row) => venue_value(row)?,
                None => Value::Null,
            },
            "immutable": match &state.immutable {
                Some(bytes) => Value::String(hex_bytes(bytes)),
                None => Value::Null,
            },
            "mutable": match &state.mutable {
                Some(row) => view_value(row),
                None => Value::Null,
            },
            "ticks": state.ticks.iter().map(tick_value).collect::<Vec<_>>(),
            "curve": match &state.curve {
                Some(curve) => curve_value(curve),
                None => Value::Null,
            },
            "quotes": match &state.quotes {
                Some(quote) => quote_value(quote),
                None => Value::Null,
            },
            "dirty": match &state.dirty {
                Some(dirty) => json!({"block": dirty.block, "class": dirty.class}),
                None => Value::Null,
            },
        }),
        StateReply::Cursor(cursor) => json!({
            "reply": "cursor",
            "cursor": cursor,
        }),
    };
    Ok(serde_json::to_vec(&value)?)
}

pub fn decode_state_reply(bytes: &[u8]) -> eyre::Result<StateReply> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| eyre!("state reply: {err}"))?;
    let name = value
        .get("reply")
        .and_then(|reply| reply.as_str())
        .ok_or_else(|| eyre!("state reply"))?;
    let reply = match name {
        "venue" => StateReply::Venue(optional_venue(&value, "venue")?),
        "wrote" => StateReply::Wrote(field_u64(&value, "wrote")?),
        "jobs" => StateReply::Jobs(
            array(&value, "jobs")?
                .iter()
                .map(job_from)
                .collect::<eyre::Result<_>>()?,
        ),
        "empty" => StateReply::Empty,
        "puts" => StateReply::Puts(
            array(&value, "puts")?
                .iter()
                .map(|item| match item.as_str() {
                    Some("written") => Ok(ImmutablePut::Written),
                    Some("present") => Ok(ImmutablePut::Present),
                    Some("mismatch") => Ok(ImmutablePut::Mismatch),
                    _ => Err(eyre!("immutable put")),
                })
                .collect::<eyre::Result<_>>()?,
        ),
        "stored" => StateReply::Stored(MutableStored {
            generation: field_u64(&value, "generation")?,
            dirty_cleared: field_bool(&value, "dirty_cleared")?,
            kept_ticks: field_bool(&value, "kept_ticks")?,
            bitmap_changed: field_bool(&value, "bitmap_changed")?,
        }),
        "state" => StateReply::State(Box::new(PoolState {
            venue: optional_venue(&value, "venue")?,
            immutable: optional_bytes(&value, "immutable")?,
            mutable: match value.get("mutable") {
                None | Some(Value::Null) => None,
                Some(row) => Some(view_from(row)?),
            },
            ticks: array(&value, "ticks")?
                .iter()
                .map(tick_from)
                .collect::<eyre::Result<_>>()?,
            curve: match value.get("curve") {
                None | Some(Value::Null) => None,
                Some(curve) => Some(curve_from(curve)?),
            },
            quotes: match value.get("quotes") {
                None | Some(Value::Null) => None,
                Some(quote) => Some(quote_from(quote)?),
            },
            dirty: match value.get("dirty") {
                None | Some(Value::Null) => None,
                Some(dirty) => Some(Dirty {
                    block: field_u64(dirty, "block")?,
                    class: field_u8(dirty, "class")?,
                }),
            },
        })),
        "cursor" => StateReply::Cursor(match value.get("cursor") {
            None | Some(Value::Null) => None,
            Some(cursor) => Some(cursor.as_u64().ok_or_else(|| eyre!("cursor"))?),
        }),
        other => return Err(eyre!("unknown state reply {other}")),
    };
    Ok(reply)
}

fn venue_value(row: &VenueRow) -> eyre::Result<Value> {
    Ok(json!({
        "kind": row.kind,
        "id": row.id,
        "row": hex_bytes(&encode_row(row)?),
    }))
}

fn optional_venue(value: &Value, name: &str) -> eyre::Result<Option<VenueRow>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(row) => {
            let kind = field_u8(row, "kind")?;
            let id = field_b256(row, "id")?;
            let bytes = parse_hex(&field_str(row, "row")?)?;
            Ok(Some(decode_row(kind, id, &bytes)?))
        }
    }
}

fn job_value(job: &WorkJob) -> Value {
    json!({
        "stage": job.stage,
        "kind": job.kind,
        "id": job.id,
        "lease_until": job.lease_until,
        "attempt": job.attempt,
        "not_before": job.not_before,
        "generation": job.generation,
        "tick_cursor": job.tick_cursor,
        "class": job.class,
        "priority": job.priority,
        "error": job.error,
    })
}

fn job_from(value: &Value) -> eyre::Result<WorkJob> {
    Ok(WorkJob {
        stage: field_u8(value, "stage")?,
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        lease_until: field_u64(value, "lease_until")?,
        attempt: u32::try_from(field_u64(value, "attempt")?).map_err(|_| eyre!("attempt"))?,
        not_before: field_u64(value, "not_before")?,
        generation: field_u64(value, "generation")?,
        tick_cursor: field_i32(value, "tick_cursor")?,
        class: field_u8(value, "class")?,
        priority: value.get("priority").and_then(Value::as_u64).unwrap_or(0),
        error: field_str(value, "error")?,
    })
}

fn fail_value(fail: &WorkFail) -> Value {
    json!({
        "stage": fail.stage,
        "kind": fail.kind,
        "id": fail.id,
        "not_before": fail.not_before,
        "error": fail.error,
    })
}

fn fail_from(value: &Value) -> eyre::Result<WorkFail> {
    Ok(WorkFail {
        stage: field_u8(value, "stage")?,
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        not_before: field_u64(value, "not_before")?,
        error: field_str(value, "error")?,
    })
}

fn done_item(value: &Value) -> eyre::Result<(u8, u8, B256)> {
    Ok((
        field_u8(value, "stage")?,
        field_u8(value, "kind")?,
        field_b256(value, "id")?,
    ))
}

fn immutable_value(row: &ImmutableRow) -> Value {
    json!({
        "kind": row.kind,
        "id": row.id,
        "body": hex_bytes(&row.body),
    })
}

fn immutable_from(value: &Value) -> eyre::Result<ImmutableRow> {
    Ok(ImmutableRow {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        body: parse_hex(&field_str(value, "body")?)?,
    })
}

fn mutable_value(row: &MutableWrite) -> Value {
    json!({
        "kind": row.kind,
        "id": row.id,
        "block": row.block,
        "class": row.class,
        "bitmap": row.bitmap,
        "tick_count": row.tick_count,
        "truncated": row.truncated,
        "next_scan": row.next_scan,
        "body": hex_bytes(&row.body),
    })
}

fn mutable_from(value: &Value) -> eyre::Result<MutableWrite> {
    Ok(MutableWrite {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        block: field_u64(value, "block")?,
        class: field_u8(value, "class")?,
        bitmap: field_b256(value, "bitmap")?,
        tick_count: u32::try_from(field_u64(value, "tick_count")?)
            .map_err(|_| eyre!("tick_count"))?,
        truncated: field_bool(value, "truncated")?,
        next_scan: optional_i32(value, "next_scan")?,
        body: parse_hex(&field_str(value, "body")?)?,
    })
}

fn view_value(row: &MutableView) -> Value {
    json!({
        "block": row.block,
        "generation": row.generation,
        "bitmap": row.bitmap,
        "tick_count": row.tick_count,
        "truncated": row.truncated,
        "next_scan": row.next_scan,
        "body": hex_bytes(&row.body),
    })
}

fn view_from(value: &Value) -> eyre::Result<MutableView> {
    Ok(MutableView {
        block: field_u64(value, "block")?,
        generation: field_u64(value, "generation")?,
        bitmap: field_b256(value, "bitmap")?,
        tick_count: u32::try_from(field_u64(value, "tick_count")?)
            .map_err(|_| eyre!("tick_count"))?,
        truncated: field_bool(value, "truncated")?,
        next_scan: optional_i32(value, "next_scan")?,
        body: parse_hex(&field_str(value, "body")?)?,
    })
}

fn page_value(page: &TickPage) -> Value {
    json!({
        "kind": page.kind,
        "id": page.id,
        "page": page.page,
        "generation": page.generation,
        "ticks": page.ticks.iter().map(tick_value).collect::<Vec<_>>(),
    })
}

fn page_from(value: &Value) -> eyre::Result<TickPage> {
    Ok(TickPage {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        page: u32::try_from(field_u64(value, "page")?).map_err(|_| eyre!("page"))?,
        generation: field_u64(value, "generation")?,
        ticks: array(value, "ticks")?
            .iter()
            .map(tick_from)
            .collect::<eyre::Result<_>>()?,
    })
}

fn tick_value(tick: &TickWord) -> Value {
    json!({
        "tick": tick.tick,
        "gross": tick.liquidity_gross.to_string(),
        "net": tick.liquidity_net.to_string(),
    })
}

fn tick_from(value: &Value) -> eyre::Result<TickWord> {
    Ok(TickWord {
        tick: field_i32(value, "tick")?,
        liquidity_gross: field_str(value, "gross")?
            .parse()
            .map_err(|_| eyre!("gross"))?,
        liquidity_net: field_str(value, "net")?.parse().map_err(|_| eyre!("net"))?,
    })
}

fn curve_value(curve: &CurveRecord) -> Value {
    json!({
        "kind": curve.kind,
        "id": curve.id,
        "generation": curve.generation,
        "version": curve.version,
        "payload": hex_bytes(&curve.payload),
    })
}

fn quote_value(quote: &QuoteRecord) -> Value {
    json!({
        "kind": quote.kind,
        "id": quote.id,
        "generation": quote.generation,
        "version": quote.version,
        "block": quote.block,
        "payload": hex_bytes(&quote.payload),
    })
}

fn quote_from(value: &Value) -> eyre::Result<QuoteRecord> {
    Ok(QuoteRecord {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        generation: field_u64(value, "generation")?,
        version: u32::try_from(field_u64(value, "version")?).map_err(|_| eyre!("version"))?,
        block: field_u64(value, "block")?,
        payload: parse_hex(&field_str(value, "payload")?)?,
    })
}

fn curve_from(value: &Value) -> eyre::Result<CurveRecord> {
    Ok(CurveRecord {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        generation: field_u64(value, "generation")?,
        version: u32::try_from(field_u64(value, "version")?).map_err(|_| eyre!("version"))?,
        payload: parse_hex(&field_str(value, "payload")?)?,
    })
}

fn hit_value(hit: &WatchHit) -> Value {
    json!({
        "kind": hit.kind,
        "id": hit.id,
        "block": hit.block,
        "class": hit.class,
    })
}

fn hit_from(value: &Value) -> eyre::Result<WatchHit> {
    Ok(WatchHit {
        kind: field_u8(value, "kind")?,
        id: field_b256(value, "id")?,
        block: field_u64(value, "block")?,
        class: field_u8(value, "class")?,
    })
}

fn put_name(put: &ImmutablePut) -> &'static str {
    match put {
        ImmutablePut::Written => "written",
        ImmutablePut::Present => "present",
        ImmutablePut::Mismatch => "mismatch",
    }
}

fn field<'a>(value: &'a Value, name: &str) -> eyre::Result<&'a Value> {
    value.get(name).ok_or_else(|| eyre!("missing {name}"))
}

fn field_u64(value: &Value, name: &str) -> eyre::Result<u64> {
    field(value, name)?
        .as_u64()
        .ok_or_else(|| eyre!("missing {name}"))
}

fn field_u8(value: &Value, name: &str) -> eyre::Result<u8> {
    u8::try_from(field_u64(value, name)?).map_err(|_| eyre!("bad {name}"))
}

fn field_i32(value: &Value, name: &str) -> eyre::Result<i32> {
    let number = field(value, name)?
        .as_i64()
        .ok_or_else(|| eyre!("missing {name}"))?;
    i32::try_from(number).map_err(|_| eyre!("bad {name}"))
}

fn optional_i32(value: &Value, name: &str) -> eyre::Result<Option<i32>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => Ok(Some(field_i32(value, name)?)),
    }
}

fn field_bool(value: &Value, name: &str) -> eyre::Result<bool> {
    field(value, name)?
        .as_bool()
        .ok_or_else(|| eyre!("missing {name}"))
}

fn field_str(value: &Value, name: &str) -> eyre::Result<String> {
    field(value, name)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| eyre!("missing {name}"))
}

fn field_b256(value: &Value, name: &str) -> eyre::Result<B256> {
    let text = field(value, name)?
        .as_str()
        .ok_or_else(|| eyre!("missing {name}"))?;
    text.parse().map_err(|_| eyre!("bad {name}"))
}

fn array<'a>(value: &'a Value, name: &str) -> eyre::Result<&'a Vec<Value>> {
    field(value, name)?
        .as_array()
        .ok_or_else(|| eyre!("missing {name}"))
}

fn optional_bytes(value: &Value, name: &str) -> eyre::Result<Option<Vec<u8>>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(text) => {
            let text = text.as_str().ok_or_else(|| eyre!("bad {name}"))?;
            Ok(Some(parse_hex(text)?))
        }
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn parse_hex(text: &str) -> eyre::Result<Vec<u8>> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return Err(eyre!("hex"));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_nibble(bytes[index])?;
        let low = hex_nibble(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> eyre::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(eyre!("hex")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn a_job_and_a_watch_call_round_trip() {
        let id = VenueRow::address_id(address!("1111111111111111111111111111111111111111"));
        let call = StateCall::EnqueueWork {
            now: 9,
            jobs: vec![WorkJob {
                stage: 4,
                kind: 1,
                id,
                lease_until: 0,
                attempt: 2,
                not_before: 3,
                generation: 7,
                tick_cursor: -1,
                class: 1,
                priority: 99,
                error: "later".into(),
            }],
        };
        let bytes = encode_state_call(&call).unwrap();
        assert_eq!(decode_state_call(&bytes).unwrap(), call);
        let reply = StateReply::Cursor(Some(12));
        let bytes = encode_state_reply(&reply).unwrap();
        assert_eq!(decode_state_reply(&bytes).unwrap(), reply);
    }
}
