//! Conduit row bytes.

use num_bigint::{BigInt, Sign};
use routing::{Action, SlopeSegment};

use super::*;

pub(super) fn encode_conduit(
    block: u64,
    generation: u64,
    members: u32,
    segments: &[SlopeSegment],
) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.push(1);
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&members.to_le_bytes());
    let count = u32::try_from(segments.len()).map_err(|_| eyre::eyre!("conduit segments"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for segment in segments {
        push_be32(&mut out, &segment.width)?;
        push_be32(&mut out, &segment.output)?;
        out.push(segment.action.kind);
        out.extend_from_slice(&segment.action.pool);
    }
    Ok(out)
}

pub(super) fn decode_conduit(bytes: &[u8]) -> eyre::Result<StoredConduit> {
    let mut reader = Reader::new(bytes);
    if reader.u8()? != 1 {
        return Err(eyre::eyre!("conduit"));
    }
    let block = reader.u64()?;
    let generation = reader.u64()?;
    let members = reader.u32()?;
    let count = reader.u32()? as usize;
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        let width = reader.be32()?;
        let output = reader.be32()?;
        let kind = reader.u8()?;
        let mut pool = [0_u8; 32];
        pool.copy_from_slice(reader.take(32)?);
        segments.push(SlopeSegment {
            width,
            output,
            action: Action { kind, pool },
        });
    }
    reader.finish()?;
    Ok(StoredConduit {
        block,
        generation,
        members,
        segments,
    })
}

pub(crate) fn meta_of(segments: &[SlopeSegment]) -> ConduitMeta {
    let mut kinds = 0_u32;
    let mut capacity = 0_u128;
    for segment in segments {
        kinds |= kind_bit(segment.action.kind);
        capacity = capacity.saturating_add(as_u128(&segment.width));
    }
    ConduitMeta {
        kinds,
        capacity,
        segments: u32::try_from(segments.len()).unwrap_or(u32::MAX),
    }
}

pub(super) fn meta_of_row(bytes: &[u8]) -> eyre::Result<ConduitMeta> {
    let mut reader = Reader::new(bytes);
    if reader.u8()? != 1 {
        return Err(eyre::eyre!("conduit"));
    }
    let _block = reader.u64()?;
    let _generation = reader.u64()?;
    let _members = reader.u32()?;
    let count = reader.u32()?;
    let mut kinds = 0_u32;
    let mut capacity = 0_u128;
    for _ in 0..count {
        let width = width_word(reader.take(32)?);
        let _output = reader.take(32)?;
        let kind = reader.u8()?;
        let _pool = reader.take(32)?;
        kinds |= kind_bit(kind);
        capacity = capacity.saturating_add(width);
    }
    reader.finish()?;
    Ok(ConduitMeta {
        kinds,
        capacity,
        segments: count,
    })
}

pub(super) fn encode_meta(meta: &ConduitMeta) -> Vec<u8> {
    let mut out = Vec::with_capacity(25);
    out.push(1);
    out.extend_from_slice(&meta.kinds.to_le_bytes());
    out.extend_from_slice(&meta.capacity.to_le_bytes());
    out.extend_from_slice(&meta.segments.to_le_bytes());
    out
}

pub(super) fn decode_meta(bytes: &[u8]) -> eyre::Result<ConduitMeta> {
    if bytes.len() != 25 || bytes[0] != 1 {
        return Err(eyre::eyre!("conduit meta"));
    }
    let mut kinds = [0_u8; 4];
    kinds.copy_from_slice(&bytes[1..5]);
    let mut capacity = [0_u8; 16];
    capacity.copy_from_slice(&bytes[5..21]);
    let mut segments = [0_u8; 4];
    segments.copy_from_slice(&bytes[21..25]);
    Ok(ConduitMeta {
        kinds: u32::from_le_bytes(kinds),
        capacity: u128::from_le_bytes(capacity),
        segments: u32::from_le_bytes(segments),
    })
}

fn kind_bit(kind: u8) -> u32 {
    if kind == 0 || kind >= 32 {
        return 0;
    }
    1_u32 << (kind - 1)
}

fn as_u128(value: &BigInt) -> u128 {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 16 {
        return if sign == Sign::Minus { 0 } else { u128::MAX };
    }
    let mut buf = [0_u8; 16];
    buf[16 - bytes.len()..].copy_from_slice(&bytes);
    u128::from_be_bytes(buf)
}

fn width_word(bytes: &[u8]) -> u128 {
    if bytes.len() != 32 || bytes.iter().take(16).any(|byte| *byte != 0) {
        return u128::MAX;
    }
    let mut buf = [0_u8; 16];
    buf.copy_from_slice(&bytes[16..]);
    u128::from_be_bytes(buf)
}

fn push_be32(out: &mut Vec<u8>, value: &BigInt) -> eyre::Result<()> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 32 {
        return Err(eyre::eyre!("segment magnitude"));
    }
    let mut word = [0_u8; 32];
    word[32 - bytes.len()..].copy_from_slice(&bytes);
    out.extend_from_slice(&word);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> eyre::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| eyre::eyre!("conduit short"))?;
        if end > self.bytes.len() {
            return Err(eyre::eyre!("conduit short"));
        }
        let start = self.at;
        self.at = end;
        Ok(&self.bytes[start..end])
    }

    fn u8(&mut self) -> eyre::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> eyre::Result<u32> {
        let mut word = [0_u8; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(word))
    }

    fn u64(&mut self) -> eyre::Result<u64> {
        let mut word = [0_u8; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(word))
    }

    fn be32(&mut self) -> eyre::Result<BigInt> {
        Ok(BigInt::from_bytes_be(Sign::Plus, self.take(32)?))
    }

    fn finish(self) -> eyre::Result<()> {
        if self.at != self.bytes.len() {
            return Err(eyre::eyre!("conduit tail"));
        }
        Ok(())
    }
}
