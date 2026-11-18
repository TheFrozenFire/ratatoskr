//! Pool snapshot, curve, and work records.

use super::tick::TickWord;
use super::venue::VenueRow;
use alloy_primitives::{B256, U256};
use std::collections::BTreeMap;

/// Fit-document generation stored on a curve. A bump rebuilds every curve.
/// Payload schema. A version 3 curve is chosen from local quotes of the
/// ingested snapshot. Older rows were fit to quoter samples or left empty.
pub const CURVE_VERSION: u32 = 3;
pub const TICK_PAGE: u32 = 512;
pub const STAGE_IMMUTABLE: u8 = 1;
pub const STAGE_MUTABLE: u8 = 2;
pub const STAGE_TICKS: u8 = 3;
pub const STAGE_CURVE: u8 = 4;
/// Retired work-queue stage. Reconcile and identity deletion still clear these rows.
pub const STAGE_QUOTE: u8 = 5;
/// Pair conduit. Claimed after the curve batch, from `conduit_jobs`.
pub const STAGE_CONDUIT: u8 = 6;
pub const CLASS_NONE: u8 = 0;
pub const CLASS_HEAD: u8 = 1;
pub const CLASS_TICKS: u8 = 2;

/// Watch hits sort ahead of every quiet pool. The low 63 bits are `max_in`.
pub const DIRTY_BAND: u64 = 1 << 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmutablePut {
    Written,
    Present,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableRow {
    pub kind: u8,
    pub id: B256,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableWrite {
    pub kind: u8,
    pub id: B256,
    pub block: u64,
    pub class: u8,
    pub bitmap: B256,
    pub tick_count: u32,
    pub truncated: bool,
    pub next_scan: Option<i32>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableStored {
    pub generation: u64,
    pub dirty_cleared: bool,
    pub kept_ticks: bool,
    pub bitmap_changed: bool,
    /// Curve removed by this snapshot. The caller retargets conduit members.
    pub previous_curve: Option<CurveRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableView {
    pub block: u64,
    pub generation: u64,
    /// Generation of the tick pages still stored for this bitmap.
    ///
    /// A head refresh keeps the pages and leaves this where it was. A new
    /// bitmap starts a new book at `generation`.
    pub book: u64,
    pub bitmap: B256,
    pub tick_count: u32,
    pub truncated: bool,
    pub next_scan: Option<i32>,
    pub body: Vec<u8>,
}

/// Generation assigned to one mutable snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotStamp {
    pub generation: u64,
    pub book: u64,
    pub kept_ticks: bool,
    pub bitmap_changed: bool,
}

/// The generation a snapshot write will store.
///
/// An unchanged head bitmap keeps the tick pages, so `book` stays on the
/// generation those pages were written with.
pub fn snapshot_stamp(previous: Option<&MutableView>, row: &MutableWrite) -> SnapshotStamp {
    let generation = previous
        .map(|view| view.generation.saturating_add(1))
        .unwrap_or(1);
    let bitmap_changed = previous
        .map(|view| view.bitmap != row.bitmap)
        .unwrap_or(true);
    let kept_ticks = !bitmap_changed && row.class == CLASS_HEAD && !row.truncated;
    let book = if kept_ticks {
        previous.map(|view| view.book).unwrap_or(generation)
    } else {
        generation
    };
    SnapshotStamp {
        generation,
        book,
        kept_ticks,
        bitmap_changed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickPage {
    pub kind: u8,
    pub id: B256,
    pub page: u32,
    pub generation: u64,
    pub ticks: Vec<TickWord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveRecord {
    pub kind: u8,
    pub id: B256,
    pub generation: u64,
    pub version: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirty {
    pub block: u64,
    pub class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchHit {
    pub kind: u8,
    pub id: B256,
    pub block: u64,
    pub class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkJob {
    pub stage: u8,
    pub kind: u8,
    pub id: B256,
    pub lease_until: u64,
    pub attempt: u32,
    pub not_before: u64,
    pub generation: u64,
    pub tick_cursor: i32,
    pub class: u8,
    /// Higher is claimed first. Bit 63 is a watch hit. The low bits grow with `max_in`.
    pub priority: u64,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkFail {
    pub stage: u8,
    pub kind: u8,
    pub id: B256,
    pub not_before: u64,
    pub error: String,
}

/// Counts for a sample catalog after a state pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateSummary {
    pub identities: u64,
    pub immutable: u64,
    pub mutable: u64,
    pub curves: u64,
    pub conduits: u64,
    pub tick_pages: u64,
    pub mismatches: u64,
    pub tokens: u64,
    pub jobs: u64,
    pub jobs_by_stage: BTreeMap<u8, u64>,
    pub errors: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub venue: Option<VenueRow>,
    pub immutable: Option<Vec<u8>>,
    pub mutable: Option<MutableView>,
    pub ticks: Vec<TickWord>,
    pub curve: Option<CurveRecord>,
    pub dirty: Option<Dirty>,
}

pub fn priority_of(dirty: bool, top: U256) -> u64 {
    let mag = magnitude(top);
    if dirty {
        DIRTY_BAND | mag
    } else {
        mag
    }
}

fn magnitude(value: U256) -> u64 {
    let bits = u256_bit_len(&value);
    if bits == 0 {
        return 0;
    }
    let shift = bits.saturating_sub(54);
    let shifted = if shift == 0 { value } else { value >> shift };
    let bytes = shifted.to_be_bytes::<32>();
    let mant = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
    let mant = mant & ((1_u64 << 54) - 1);
    ((bits as u64) << 54) | mant
}

fn u256_bit_len(value: &U256) -> u32 {
    let bytes = value.to_be_bytes::<32>();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != 0 {
            return (32 - index as u32) * 8 - byte.leading_zeros();
        }
    }
    0
}

fn clip_error(error: &str) -> String {
    error.chars().take(160).collect()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn rest(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.rest() < len {
            return Err("short".into());
        }
        let start = self.at;
        self.at += len;
        Ok(&self.bytes[start..self.at])
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u128(&mut self) -> Result<u128, String> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn i128(&mut self) -> Result<i128, String> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn hash(&mut self) -> Result<B256, String> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn finish(&self) -> Result<(), String> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing".into())
        }
    }
}

fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(out: &mut Vec<u8>, value: u128) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(bytes);
}

pub fn encode_mutable(row: &MutableWrite, generation: u64, book: u64) -> Vec<u8> {
    let mut out = vec![2];
    push_u64(&mut out, row.block);
    push_u64(&mut out, generation);
    push_u64(&mut out, book);
    out.extend_from_slice(row.bitmap.as_slice());
    push_u32(&mut out, row.tick_count);
    push_u8(&mut out, u8::from(row.truncated));
    match row.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_bytes(&mut out, &row.body);
    out
}

pub fn decode_mutable(bytes: &[u8]) -> eyre::Result<MutableView> {
    let view = decode_mutable_parts(bytes).map_err(|err| eyre::eyre!(err))?;
    Ok(view)
}

pub fn decode_mutable_parts(bytes: &[u8]) -> Result<MutableView, String> {
    let mut reader = Reader::new(bytes);
    let version = reader.u8()?;
    if version != 1 && version != 2 {
        return Err("mutable".into());
    }
    let block = reader.u64()?;
    let generation = reader.u64()?;
    // A version 1 row was written when every kept page was restamped to the
    // row generation, so that generation is the book.
    let book = if version == 2 {
        reader.u64()?
    } else {
        generation
    };
    let bitmap = reader.hash()?;
    let tick_count = reader.u32()?;
    let truncated = reader.u8()? != 0;
    let next_scan = if reader.u8()? == 1 {
        Some(reader.i32()?)
    } else {
        None
    };
    let body = reader.bytes()?;
    reader.finish()?;
    Ok(MutableView {
        block,
        generation,
        book,
        bitmap,
        tick_count,
        truncated,
        next_scan,
        body,
    })
}

pub fn encode_dirty(block: u64, class: u8) -> Vec<u8> {
    let mut out = vec![1];
    push_u64(&mut out, block);
    push_u8(&mut out, class);
    out
}

pub fn decode_dirty(bytes: &[u8]) -> eyre::Result<(u64, u8)> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("dirty"));
    }
    let block = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let class = reader.u8().map_err(|err| eyre::eyre!(err))?;
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok((block, class))
}

pub fn encode_tick_page(page: &TickPage) -> eyre::Result<Vec<u8>> {
    let mut out = vec![1];
    push_u64(&mut out, page.generation);
    push_u32(&mut out, page.ticks.len() as u32);
    for tick in &page.ticks {
        push_i32(&mut out, tick.tick);
        push_u128(&mut out, tick.liquidity_gross);
        out.extend_from_slice(&tick.liquidity_net.to_le_bytes());
    }
    Ok(out)
}

pub fn tick_page_generation(bytes: &[u8]) -> eyre::Result<u64> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("tick page"));
    }
    reader.u64().map_err(|err| eyre::eyre!(err))
}

pub fn encode_curve(curve: &CurveRecord) -> Vec<u8> {
    let mut out = vec![1];
    push_u32(&mut out, curve.version);
    push_u64(&mut out, curve.generation);
    push_bytes(&mut out, &curve.payload);
    out
}

pub fn decode_curve_record(kind: u8, id: B256, bytes: &[u8]) -> eyre::Result<CurveRecord> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("curve"));
    }
    let version = reader.u32().map_err(|err| eyre::eyre!(err))?;
    let generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let payload = reader.bytes().map_err(|err| eyre::eyre!(err))?;
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok(CurveRecord {
        kind,
        id,
        generation,
        version,
        payload,
    })
}

pub fn encode_job(job: &WorkJob) -> Vec<u8> {
    let error = clip_error(&job.error);
    let mut out = vec![2];
    push_u64(&mut out, job.lease_until);
    push_u32(&mut out, job.attempt);
    push_u64(&mut out, job.not_before);
    push_u64(&mut out, job.generation);
    push_i32(&mut out, job.tick_cursor);
    push_u8(&mut out, job.class);
    push_u64(&mut out, job.priority);
    let bytes = error.as_bytes();
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
    out
}

pub fn decode_job(key: &[u8], bytes: &[u8]) -> eyre::Result<WorkJob> {
    if key.len() != 34 {
        return Err(eyre::eyre!("work key"));
    }
    let mut reader = Reader::new(bytes);
    let tag = reader.u8().map_err(|err| eyre::eyre!(err))?;
    if tag != 1 && tag != 2 {
        return Err(eyre::eyre!("work"));
    }
    let lease_until = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let attempt = reader.u32().map_err(|err| eyre::eyre!(err))?;
    let not_before = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let tick_cursor = reader.i32().map_err(|err| eyre::eyre!(err))?;
    let class = reader.u8().map_err(|err| eyre::eyre!(err))?;
    let priority = if tag == 2 {
        reader.u64().map_err(|err| eyre::eyre!(err))?
    } else {
        0
    };
    let len = reader.u16().map_err(|err| eyre::eyre!(err))? as usize;
    let error = reader.take(len).map_err(|err| eyre::eyre!(err))?.to_vec();
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok(WorkJob {
        stage: key[0],
        kind: key[1],
        id: B256::from_slice(&key[2..]),
        lease_until,
        attempt,
        not_before,
        generation,
        tick_cursor,
        class,
        priority,
        error: String::from_utf8_lossy(&error).into_owned(),
    })
}

pub fn decode_tick_words(bytes: &[u8]) -> eyre::Result<Vec<TickWord>> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("tick page"));
    }
    let _generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let n = reader.u32().map_err(|err| eyre::eyre!(err))? as usize;
    let mut ticks = Vec::with_capacity(n);
    for _ in 0..n {
        ticks.push(TickWord {
            tick: reader.i32().map_err(|err| eyre::eyre!(err))?,
            liquidity_gross: reader.u128().map_err(|err| eyre::eyre!(err))?,
            liquidity_net: reader.i128().map_err(|err| eyre::eyre!(err))?,
        });
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> MutableWrite {
        MutableWrite {
            kind: 1,
            id: B256::repeat_byte(3),
            block: 9,
            class: CLASS_HEAD,
            bitmap: B256::repeat_byte(4),
            tick_count: 2,
            truncated: false,
            next_scan: Some(8),
            body: b"pool".to_vec(),
        }
    }

    #[test]
    fn a_mutable_row_without_a_book_uses_its_generation() {
        let sample = row();
        let mut bytes = vec![1];
        push_u64(&mut bytes, sample.block);
        push_u64(&mut bytes, 6);
        bytes.extend_from_slice(sample.bitmap.as_slice());
        push_u32(&mut bytes, sample.tick_count);
        push_u8(&mut bytes, 0);
        push_u8(&mut bytes, 1);
        push_i32(&mut bytes, 8);
        push_bytes(&mut bytes, &sample.body);
        let view = decode_mutable(&bytes).unwrap();
        assert_eq!(view.generation, 6);
        assert_eq!(view.book, 6);
        assert_eq!(view.body, b"pool");

        let encoded = encode_mutable(&sample, 7, 6);
        let again = decode_mutable(&encoded).unwrap();
        assert_eq!(again.generation, 7);
        assert_eq!(again.book, 6);
        assert_eq!(again.block, sample.block);
        assert_eq!(again.body, sample.body);
    }

    #[test]
    fn an_unchanged_head_bitmap_keeps_the_book_generation() {
        let sample = row();
        let previous = MutableView {
            block: 8,
            generation: 4,
            book: 2,
            bitmap: sample.bitmap,
            tick_count: sample.tick_count,
            truncated: false,
            next_scan: None,
            body: Vec::new(),
        };
        let stamp = snapshot_stamp(Some(&previous), &sample);
        assert!(stamp.kept_ticks);
        assert!(!stamp.bitmap_changed);
        assert_eq!(stamp.generation, 5);
        assert_eq!(stamp.book, 2);

        let mut changed = sample.clone();
        changed.bitmap = B256::repeat_byte(9);
        let stamp = snapshot_stamp(Some(&previous), &changed);
        assert!(!stamp.kept_ticks);
        assert!(stamp.bitmap_changed);
        assert_eq!(stamp.book, stamp.generation);
    }
}
