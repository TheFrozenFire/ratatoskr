//! Pair-conduit work records.

use alloy_primitives::Address;

/// One leased rebuild of an ordered pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitJob {
    pub token_in: Address,
    pub token_out: Address,
    pub lease_until: u64,
    pub attempt: u32,
    pub not_before: u64,
    /// A curve write arrived while this job was leased. The next finish keeps it.
    pub rebuild: bool,
    pub error: String,
}

/// A failed rebuild. The job stays and waits until `not_before`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitFail {
    pub token_in: Address,
    pub token_out: Address,
    pub not_before: u64,
    pub error: String,
}

pub fn encode_conduit_job(job: &ConduitJob) -> Vec<u8> {
    let error = clip_error(&job.error);
    let mut out = vec![1];
    out.extend_from_slice(&job.lease_until.to_le_bytes());
    out.extend_from_slice(&job.attempt.to_le_bytes());
    out.extend_from_slice(&job.not_before.to_le_bytes());
    out.push(u8::from(job.rebuild));
    let bytes = error.as_bytes();
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
    out
}

pub fn decode_conduit_job(key: &[u8], bytes: &[u8]) -> eyre::Result<ConduitJob> {
    if key.len() != 40 {
        return Err(eyre::eyre!("conduit job"));
    }
    let mut reader = Reader::new(bytes);
    if reader.u8()? != 1 {
        return Err(eyre::eyre!("conduit job"));
    }
    let lease_until = reader.u64()?;
    let attempt = reader.u32()?;
    let not_before = reader.u64()?;
    let rebuild = reader.u8()? == 1;
    let len = reader.u16()? as usize;
    let error = reader.take(len)?.to_vec();
    reader.finish()?;
    Ok(ConduitJob {
        token_in: Address::from_slice(&key[..20]),
        token_out: Address::from_slice(&key[20..]),
        lease_until,
        attempt,
        not_before,
        rebuild,
        error: String::from_utf8_lossy(&error).into_owned(),
    })
}

fn clip_error(error: &str) -> String {
    let mut end = error.len().min(180);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
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

    fn u16(&mut self) -> eyre::Result<u16> {
        let mut word = [0_u8; 2];
        word.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(word))
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

    fn finish(self) -> eyre::Result<()> {
        if self.at != self.bytes.len() {
            return Err(eyre::eyre!("conduit tail"));
        }
        Ok(())
    }
}
