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
