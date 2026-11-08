//! Era1 records needed to jump to a header and then to its receipts.
//!
//! Offsets in the block index are signed distances from the first byte of the
//! index record to the compressed header. The body sits between that header
//! and the receipts, so the header read also takes the body's 8-byte tag and
//! then skips the body.

use alloy_consensus::{Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use eyre::{eyre, WrapErr};
use snap::read::FrameDecoder;
use std::io::Read;

pub const BLOCKS_PER_EPOCH: u64 = 8192;
pub const MAX_BLOCKS: usize = 8192;
/// Largest block-index record: 8-byte tag, start, 8192 offsets, count.
pub const INDEX_TAIL: u64 = 24 + MAX_BLOCKS as u64 * 8;
/// Probe used to measure how big a header record is before the bulk read.
pub const SAMPLE_WINDOW: u64 = 1536;

const HEADER_TYPE: [u8; 2] = [0x03, 0x00];
const BODY_TYPE: [u8; 2] = [0x04, 0x00];
const RECEIPTS_TYPE: [u8; 2] = [0x05, 0x00];
const INDEX_TYPE: [u8; 2] = [0x66, 0x32];
const MAX_HEADER_PAYLOAD: u32 = 16 * 1024;
const MAX_RECEIPT_PAYLOAD: u32 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct BlockIndex {
    pub starting_number: u64,
    /// File offset of the index record itself. Offsets are relative to this.
    pub record_at: u64,
    pub offsets: Vec<i64>,
}

impl BlockIndex {
    pub fn header_at(&self, block: u64) -> Option<u64> {
        let index = block.checked_sub(self.starting_number)? as usize;
        let relative = *self.offsets.get(index)?;
        self.record_at.checked_add_signed(relative)
    }

    pub fn last_block(&self) -> u64 {
        self.starting_number + self.offsets.len() as u64 - 1
    }
}

/// `tail` is the suffix of the file, ending on the block index.
pub fn parse_index(file_len: u64, tail: &[u8]) -> eyre::Result<BlockIndex> {
    if tail.len() < 32 || file_len < tail.len() as u64 {
        return Err(eyre!("era1 tail is shorter than a block index"));
    }
    let count = i64::from_le_bytes(tail[tail.len() - 8..].try_into().unwrap());
    if count < 1 || count as usize > MAX_BLOCKS {
        return Err(eyre!(
            "era1 block count {count} is outside 1..={MAX_BLOCKS}"
        ));
    }
    let count = count as usize;
    let record_len = 24 + count * 8;
    if tail.len() < record_len {
        return Err(eyre!("era1 tail does not hold the {record_len}-byte index"));
    }
    let record = &tail[tail.len() - record_len..];
    if record[0..2] != INDEX_TYPE {
        return Err(eyre!(
            "era1 tail type {:02x}{:02x} is not a block index",
            record[0],
            record[1]
        ));
    }
    let length = u32::from_le_bytes(record[2..6].try_into().unwrap());
    if record[6..8] != [0, 0] || length as usize != 16 + count * 8 {
        return Err(eyre!("era1 index header length {length}"));
    }
    let data = &record[8..8 + length as usize];
    let starting_number = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let start = 8 + i * 8;
        offsets.push(i64::from_le_bytes(
            data[start..start + 8].try_into().unwrap(),
        ));
    }
    let recorded = i64::from_le_bytes(data[data.len() - 8..].try_into().unwrap());
    if recorded != count as i64 {
        return Err(eyre!("era1 index count {recorded} does not match the tail"));
    }
    Ok(BlockIndex {
        starting_number,
        record_at: file_len - record_len as u64,
        offsets,
    })
}

/// What a header-window read contains.
#[derive(Debug)]
pub enum HeaderCut<'a> {
    /// Header payload, and the body length from the following tag.
    Ready {
        payload: &'a [u8],
        entry_len: u64,
        body_len: u64,
    },
    /// `buf` holds `have` bytes; the header tag plus the body tag need `need`.
    Short { have: u64, need: u64 },
}

pub fn cut_header(buf: &[u8]) -> eyre::Result<HeaderCut<'_>> {
    if buf.len() < 8 {
        return Ok(HeaderCut::Short {
            have: buf.len() as u64,
            need: 8,
        });
    }
    if buf[0..2] != HEADER_TYPE {
        return Err(eyre!(
            "expected a compressed header, found {:02x}{:02x}",
            buf[0],
            buf[1]
        ));
    }
    if buf[6..8] != [0, 0] {
        return Err(eyre!("compressed header reserved bytes are set"));
    }
    let payload_len = u32::from_le_bytes(buf[2..6].try_into().unwrap());
    if payload_len > MAX_HEADER_PAYLOAD {
        return Err(eyre!("compressed header is {payload_len} bytes"));
    }
    let entry_len = 8 + payload_len as u64;
    let need = entry_len + 8;
    if (buf.len() as u64) < need {
        return Ok(HeaderCut::Short {
            have: buf.len() as u64,
            need,
        });
    }
    let entry = entry_len as usize;
    if buf[entry..entry + 2] != BODY_TYPE {
        return Err(eyre!(
            "expected a body after the header, found {:02x}{:02x}",
            buf[entry],
            buf[entry + 1]
        ));
    }
    if buf[entry + 6..entry + 8] != [0, 0] {
        return Err(eyre!("body tag reserved bytes are set"));
    }
    let body_len = u32::from_le_bytes(buf[entry + 2..entry + 6].try_into().unwrap()) as u64;
    Ok(HeaderCut::Ready {
        payload: &buf[8..entry],
        entry_len,
        body_len,
    })
}

pub fn receipt_at(header_at: u64, entry_len: u64, body_len: u64) -> Option<u64> {
    header_at
        .checked_add(entry_len)?
        .checked_add(8)?
        .checked_add(body_len)
}

/// Length of the compressed receipt payload. `head` is the 8-byte record tag.
pub fn receipt_payload_len(head: &[u8]) -> eyre::Result<u32> {
    if head.len() < 8 || head[0..2] != RECEIPTS_TYPE || head[6..8] != [0, 0] {
        return Err(eyre!("expected a receipt record tag"));
    }
    let length = u32::from_le_bytes(head[2..6].try_into().unwrap());
    if length > MAX_RECEIPT_PAYLOAD {
        return Err(eyre!("compressed receipts are {length} bytes"));
    }
    Ok(length)
}

pub fn decode_header(payload: &[u8]) -> eyre::Result<Header> {
    let raw = inflate(payload).wrap_err("header snappy")?;
    let mut slice = raw.as_slice();
    let header = Header::decode(&mut slice).wrap_err("header rlp")?;
    if !slice.is_empty() {
        return Err(eyre!("header rlp has {} trailing bytes", slice.len()));
    }
    Ok(header)
}

pub fn decode_receipts(payload: &[u8]) -> eyre::Result<Vec<ReceiptEnvelope>> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    let raw = inflate(payload).wrap_err("receipt snappy")?;
    decode_receipt_list(&raw)
}

fn inflate(payload: &[u8]) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    FrameDecoder::new(payload).read_to_end(&mut out)?;
    Ok(out)
}

fn decode_receipt_list(raw: &[u8]) -> eyre::Result<Vec<ReceiptEnvelope>> {
    let mut buf = raw;
    let header = alloy_rlp::Header::decode(&mut buf).wrap_err("receipt list")?;
    if !header.list {
        return Err(eyre!("receipt payload is not an rlp list"));
    }
    if buf.len() != header.payload_length {
        return Err(eyre!("receipt list length does not match its payload"));
    }
    let mut list = buf;
    let mut receipts = Vec::new();
    while !list.is_empty() {
        receipts.push(decode_one_receipt(&mut list)?);
    }
    Ok(receipts)
}

fn decode_one_receipt(buf: &mut &[u8]) -> eyre::Result<ReceiptEnvelope> {
    let peeked = alloy_rlp::Header::decode(&mut &buf[..]).wrap_err("receipt item")?;
    if peeked.list {
        let receipt = ReceiptWithBloom::<Receipt>::decode(buf).wrap_err("legacy receipt")?;
        return Ok(ReceiptEnvelope::Legacy(receipt));
    }
    let header = alloy_rlp::Header::decode(buf).wrap_err("typed receipt string")?;
    if buf.len() < header.payload_length {
        return Err(eyre!("typed receipt string is truncated"));
    }
    let (bytes, rest) = buf.split_at(header.payload_length);
    *buf = rest;
    if bytes.is_empty() {
        return Err(eyre!("empty typed receipt"));
    }
    let mut payload = &bytes[1..];
    let receipt = ReceiptWithBloom::<Receipt>::decode(&mut payload).wrap_err("typed receipt")?;
    if !payload.is_empty() {
        return Err(eyre!("typed receipt has trailing bytes"));
    }
    match bytes[0] {
        0x01 => Ok(ReceiptEnvelope::Eip2930(receipt)),
        0x02 => Ok(ReceiptEnvelope::Eip1559(receipt)),
        0x03 => Ok(ReceiptEnvelope::Eip4844(receipt)),
        0x04 => Ok(ReceiptEnvelope::Eip7702(receipt)),
        other => Err(eyre!("unknown receipt type {other:#x}")),
    }
}

pub fn receipt_root(receipts: &[ReceiptEnvelope]) -> B256 {
    alloy_consensus::proofs::calculate_receipt_root(receipts)
}

/// Bytes to fetch at each header so one response usually includes the body tag.
///
/// `entry_lens` are compressed-header record sizes from a sample. The result
/// adds the 8-byte body tag and a little slack for a neighbor that is slightly
/// larger. A header that still sticks out is finished with one short follow-up
/// rather than a second full window.
pub fn fitted_window(entry_lens: &[u64]) -> u64 {
    let max_entry = entry_lens.iter().copied().max().unwrap_or(512);
    (max_entry + 8 + 48).clamp(576, 1280)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Eip658Value;
    use alloy_primitives::{address, b256, Bytes, Log, LogData};
    use alloy_rlp::Encodable;
    use snap::write::FrameEncoder;
    use std::io::Write;

    fn frame(rlp: &[u8]) -> Vec<u8> {
        let mut enc = FrameEncoder::new(Vec::new());
        enc.write_all(rlp).unwrap();
        enc.into_inner().unwrap()
    }

    #[test]
    fn index_offset_is_relative_to_the_record() {
        let mut data = Vec::new();
        data.extend_from_slice(&100u64.to_le_bytes());
        data.extend_from_slice(&(-50i64).to_le_bytes());
        data.extend_from_slice(&1i64.to_le_bytes());
        let mut record = vec![0x66, 0x32];
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&[0, 0]);
        record.extend_from_slice(&data);
        let index = parse_index(1000, &record).unwrap();
        assert_eq!(index.starting_number, 100);
        assert_eq!(index.record_at, 1000 - record.len() as u64);
        assert_eq!(index.header_at(100), Some(index.record_at - 50));
        assert_eq!(index.header_at(101), None);
    }

    #[test]
    fn header_window_skips_the_body_using_its_tag() {
        let header = Header {
            number: 7,
            ..Default::default()
        };
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        let payload = frame(&rlp);
        let mut buf = vec![0x03, 0x00];
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&payload);
        let entry_len = buf.len() as u64;
        buf.extend_from_slice(&[0x04, 0x00, 0x20, 0, 0, 0, 0, 0]);
        match cut_header(&buf).unwrap() {
            HeaderCut::Ready {
                payload: got,
                entry_len: len,
                body_len,
            } => {
                assert_eq!(len, entry_len);
                assert_eq!(body_len, 0x20);
                assert_eq!(decode_header(got).unwrap().number, 7);
                assert_eq!(receipt_at(1000, len, body_len), Some(1000 + len + 8 + 0x20));
            }
            HeaderCut::Short { .. } => panic!("window should hold the header and the body tag"),
        }
        assert!(matches!(
            cut_header(&buf[..8]).unwrap(),
            HeaderCut::Short { .. }
        ));
    }

    #[test]
    fn fitted_window_covers_the_sample_and_the_body_tag() {
        assert_eq!(fitted_window(&[560]), 616);
        assert_eq!(fitted_window(&[]), 576);
        assert_eq!(fitted_window(&[2000]), 1280);
    }

    fn sample_log() -> Log {
        Log {
            address: address!("1F98431c8aD98523631AE4a59f267346ea31F984"),
            data: LogData::new_unchecked(
                vec![b256!(
                    "783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118"
                )],
                Bytes::from(vec![1, 2, 3, 4]),
            ),
        }
    }

    fn receipt_with(log: Log) -> ReceiptWithBloom {
        let bloom = alloy_primitives::logs_bloom([&log]);
        ReceiptWithBloom::new(
            Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![log],
            },
            bloom,
        )
    }

    fn list_of(item: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        alloy_rlp::Header {
            list: true,
            payload_length: item.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(item);
        out
    }

    #[test]
    fn legacy_and_typed_receipts_roundtrip() {
        let log = sample_log();
        let receipt = receipt_with(log.clone());
        let mut legacy = Vec::new();
        receipt.encode(&mut legacy);
        let encoded = frame(&list_of(&legacy));
        let decoded = decode_receipts(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].logs()[0].address, log.address);
        assert_eq!(
            receipt_root(&decoded),
            alloy_consensus::proofs::calculate_receipt_root(&decoded)
        );

        let mut typed = vec![0x02];
        receipt.encode(&mut typed);
        let mut as_string = Vec::new();
        typed.as_slice().encode(&mut as_string);
        let decoded = decode_receipts(&frame(&list_of(&as_string))).unwrap();
        assert!(matches!(decoded[0], ReceiptEnvelope::Eip1559(_)));
        assert_eq!(decoded[0].logs()[0].data.data.as_ref(), &[1, 2, 3, 4]);
    }
}
