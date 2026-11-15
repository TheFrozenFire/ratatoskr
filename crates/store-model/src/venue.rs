//! Venue identity, token, and receipt-body records.

use alloy_primitives::{Address, B256};

pub const RECORD_VERSION: u8 = 1;
pub const RECORD_VERSION_OBS: u8 = 2;
const FLAG_FEE: u8 = 1;
const FLAG_TICK: u8 = 2;
const FLAG_BLOCK: u8 = 4;
const FLAG_EXTRA: u8 = 8;
pub const FLAG_OBS: u8 = 16;

/// One token the catalog has read. `decimals` is absent until `decimals()` answers.
/// `wraps` is the other side of a wrapper pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub address: Address,
    pub symbol: String,
    pub decimals: Option<u8>,
    pub wraps: Option<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// Fill gaps in an existing row. A later list does not erase a block number.
    Merge,
    /// Leave an existing row alone. Used when a registry repeats a factory pool.
    IfAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueRow {
    pub kind: u8,
    pub id: B256,
    pub factory: Address,
    pub tokens: Vec<Address>,
    pub fee: Option<u32>,
    pub tick_spacing: Option<i32>,
    pub block_number: Option<u64>,
    pub extra: Option<Address>,
    pub block_hash: Option<B256>,
    pub tx_index: Option<u64>,
    pub log_index: Option<u64>,
}

impl VenueRow {
    pub fn address_id(address: Address) -> B256 {
        let mut id = B256::ZERO;
        id[12..].copy_from_slice(address.as_slice());
        id
    }

    pub fn key(&self) -> [u8; 33] {
        entity_key(self.kind, &self.id)
    }
}

pub fn entity_key(kind: u8, id: &B256) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[0] = kind;
    out[1..].copy_from_slice(id.as_slice());
    out
}

pub const UNI_V2: u8 = 1;
pub const SUSHI_V2: u8 = 2;
pub const PANCAKE_V2: u8 = 3;
pub const V2_FORK: u8 = 4;
pub const UNI_V3: u8 = 5;
pub const PANCAKE_V3: u8 = 6;
pub const SUSHI_V3: u8 = 7;
pub const V3_FORK: u8 = 8;
pub const AERO: u8 = 9;
pub const UNI_V4: u8 = 10;
pub const BALANCER_V2: u8 = 11;
pub const BALANCER_V3: u8 = 12;
pub const CURVE: u8 = 13;
pub const METRIC: u8 = 14;

pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "uniswap_v2",
        2 => "sushi_v2",
        3 => "pancake_v2",
        4 => "v2_fork",
        5 => "uniswap_v3",
        6 => "pancake_v3",
        7 => "sushi_v3",
        8 => "v3_fork",
        9 => "aero",
        10 => "uniswap_v4",
        11 => "balancer_v2",
        12 => "balancer_v3",
        13 => "curve",
        14 => "metric",
        _ => "other",
    }
}

pub fn encode_token(row: &TokenRecord) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1);
    out.push(row.decimals.unwrap_or(255));
    let bytes = row.symbol.as_bytes();
    let len = bytes.len().min(255);
    out.push(len as u8);
    out.extend_from_slice(&bytes[..len]);
    match row.wraps {
        Some(address) => {
            out.push(1);
            out.extend_from_slice(address.as_slice());
        }
        None => out.push(0),
    }
    out
}

pub fn decode_token(address: Address, bytes: &[u8]) -> eyre::Result<TokenRecord> {
    if bytes.first().copied() != Some(1) || bytes.len() < 4 {
        return Err(eyre::eyre!("token record"));
    }
    let decimals = match bytes[1] {
        255 => None,
        value => Some(value),
    };
    let len = bytes[2] as usize;
    if bytes.len() < 3 + len + 1 {
        return Err(eyre::eyre!("token record"));
    }
    let symbol =
        String::from_utf8(bytes[3..3 + len].to_vec()).map_err(|_| eyre::eyre!("token symbol"))?;
    let wraps = if bytes[3 + len] == 1 {
        let start = 4 + len;
        if bytes.len() < start + 20 {
            return Err(eyre::eyre!("token record"));
        }
        Some(Address::from_slice(&bytes[start..start + 20]))
    } else {
        None
    };
    Ok(TokenRecord {
        address,
        symbol,
        decimals,
        wraps,
    })
}

/// Era1 receipt record kept so the same block is not fetched again.
///
/// `payload` is the snappy frame. An empty payload is a block with no receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBody {
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub receipts_root: B256,
    pub payload: Vec<u8>,
}

const BODY_VERSION: u8 = 1;
const BODY_FIXED: usize = 1 + 32 + 32 + 8 + 32;

pub fn encode_body(body: &ReceiptBody) -> Vec<u8> {
    let mut out = Vec::with_capacity(BODY_FIXED + body.payload.len());
    out.push(BODY_VERSION);
    out.extend_from_slice(body.hash.as_slice());
    out.extend_from_slice(body.parent_hash.as_slice());
    out.extend_from_slice(&body.timestamp.to_le_bytes());
    out.extend_from_slice(body.receipts_root.as_slice());
    out.extend_from_slice(&body.payload);
    out
}

pub fn decode_body(bytes: &[u8]) -> eyre::Result<ReceiptBody> {
    if bytes.len() < BODY_FIXED || bytes[0] != BODY_VERSION {
        return Err(eyre::eyre!("receipt body header"));
    }
    Ok(ReceiptBody {
        hash: B256::from_slice(&bytes[1..33]),
        parent_hash: B256::from_slice(&bytes[33..65]),
        timestamp: u64::from_le_bytes(bytes[65..73].try_into().unwrap()),
        receipts_root: B256::from_slice(&bytes[73..105]),
        payload: bytes[BODY_FIXED..].to_vec(),
    })
}

pub fn factory_of(bytes: &[u8]) -> eyre::Result<Address> {
    if bytes.len() < 22 || !matches!(bytes[0], RECORD_VERSION | RECORD_VERSION_OBS) {
        return Err(eyre::eyre!("venue row header"));
    }
    Ok(Address::from_slice(&bytes[2..22]))
}

pub fn encode_row(row: &VenueRow) -> eyre::Result<Vec<u8>> {
    let observation = match (row.block_hash, row.tx_index, row.log_index) {
        (None, None, None) => None,
        (Some(hash), Some(tx_index), Some(log_index)) => Some((hash, tx_index, log_index)),
        _ => return Err(eyre::eyre!("venue row observation")),
    };
    let mut flags = 0_u8;
    if row.fee.is_some() {
        flags |= FLAG_FEE;
    }
    if row.tick_spacing.is_some() {
        flags |= FLAG_TICK;
    }
    if row.block_number.is_some() {
        flags |= FLAG_BLOCK;
    }
    if row.extra.is_some() {
        flags |= FLAG_EXTRA;
    }
    if observation.is_some() {
        flags |= FLAG_OBS;
    }
    let tokens = row.tokens.len().min(16);
    let mut out = Vec::with_capacity(2 + 20 + 1 + tokens * 20 + 16 + 48);
    // No observation keeps version 1, so those rows stay the bytes they are today.
    out.push(if observation.is_some() {
        RECORD_VERSION_OBS
    } else {
        RECORD_VERSION
    });
    out.push(flags);
    out.extend_from_slice(row.factory.as_slice());
    out.push(tokens as u8);
    for token in row.tokens.iter().take(tokens) {
        out.extend_from_slice(token.as_slice());
    }
    if let Some(fee) = row.fee {
        out.extend_from_slice(&fee.to_le_bytes());
    }
    if let Some(tick) = row.tick_spacing {
        out.extend_from_slice(&tick.to_le_bytes());
    }
    if let Some(block) = row.block_number {
        out.extend_from_slice(&block.to_le_bytes());
    }
    if let Some(extra) = row.extra {
        out.extend_from_slice(extra.as_slice());
    }
    if let Some((hash, tx_index, log_index)) = observation {
        out.extend_from_slice(hash.as_slice());
        out.extend_from_slice(&tx_index.to_le_bytes());
        out.extend_from_slice(&log_index.to_le_bytes());
    }
    Ok(out)
}

pub fn decode_row(kind: u8, id: B256, bytes: &[u8]) -> eyre::Result<VenueRow> {
    if bytes.len() < 23 {
        return Err(eyre::eyre!("venue row header"));
    }
    let version = bytes[0];
    let flags = bytes[1];
    let observed = flags & FLAG_OBS != 0;
    match (version, observed) {
        (RECORD_VERSION, false) | (RECORD_VERSION_OBS, true) => {}
        (RECORD_VERSION, true) | (RECORD_VERSION_OBS, false) => {
            return Err(eyre::eyre!("venue row observation"));
        }
        _ => return Err(eyre::eyre!("venue row header")),
    }
    let factory = Address::from_slice(&bytes[2..22]);
    let n = bytes[22] as usize;
    let mut at = 23;
    if bytes.len() < at + n * 20 {
        return Err(eyre::eyre!("venue row tokens"));
    }
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(Address::from_slice(&bytes[at..at + 20]));
        at += 20;
    }
    let fee = if flags & FLAG_FEE != 0 {
        Some(read_u32(bytes, &mut at)?)
    } else {
        None
    };
    let tick_spacing = if flags & FLAG_TICK != 0 {
        Some(read_i32(bytes, &mut at)?)
    } else {
        None
    };
    let block_number = if flags & FLAG_BLOCK != 0 {
        Some(read_u64(bytes, &mut at)?)
    } else {
        None
    };
    let extra = if flags & FLAG_EXTRA != 0 {
        if bytes.len() < at + 20 {
            return Err(eyre::eyre!("venue row extra"));
        }
        let address = Address::from_slice(&bytes[at..at + 20]);
        at += 20;
        Some(address)
    } else {
        None
    };
    let (block_hash, tx_index, log_index) = if observed {
        if bytes.len() < at + 32 {
            return Err(eyre::eyre!("venue row observation"));
        }
        let hash = B256::from_slice(&bytes[at..at + 32]);
        at += 32;
        let tx_index = read_u64(bytes, &mut at)?;
        let log_index = read_u64(bytes, &mut at)?;
        (Some(hash), Some(tx_index), Some(log_index))
    } else {
        (None, None, None)
    };
    if at != bytes.len() {
        return Err(eyre::eyre!("venue row trailing bytes"));
    }
    Ok(VenueRow {
        kind,
        id,
        factory,
        tokens,
        fee,
        tick_spacing,
        block_number,
        extra,
        block_hash,
        tx_index,
        log_index,
    })
}

fn read_u32(bytes: &[u8], at: &mut usize) -> eyre::Result<u32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| eyre::eyre!("venue row fee"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row fee"))?;
    *at = end;
    Ok(u32::from_le_bytes(word.try_into().unwrap()))
}

fn read_i32(bytes: &[u8], at: &mut usize) -> eyre::Result<i32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| eyre::eyre!("venue row tick"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row tick"))?;
    *at = end;
    Ok(i32::from_le_bytes(word.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], at: &mut usize) -> eyre::Result<u64> {
    let end = at
        .checked_add(8)
        .ok_or_else(|| eyre::eyre!("venue row block"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row block"))?;
    *at = end;
    Ok(u64::from_le_bytes(word.try_into().unwrap()))
}

/// Kind is stored in the key. The tally has to read it from there.
pub fn tally_kind(key: &[u8]) -> Option<u8> {
    key.first().copied()
}
