//! Creation-code lens.
//!
//! `eth_call` this init code with no `to`. The constructor reverts the rows,
//! and the client reads JSON-RPC error `data`. Nothing is deployed.

use alloy_primitives::{Address, I256, U256};
use std::sync::OnceLock;

/// keccak256("ratatoskr.lens.v1").
pub const MAGIC: [u8; 32] = [
    0xae, 0xe6, 0x4a, 0x0f, 0xcd, 0x28, 0x25, 0x2b, 0x5f, 0x74, 0x49, 0xae, 0x98, 0x33, 0xb0, 0xc8,
    0xc4, 0x78, 0xd8, 0x3a, 0x2e, 0x26, 0x6d, 0xbb, 0x86, 0x68, 0x40, 0x6d, 0x5e, 0x8d, 0x26, 0x67,
];

/// Gas passed on the `eth_call`. Inner staticcalls spend far less on success.
pub const CALL_GAS: u64 = 50_000_000;

const CREATION_HEX: &str = include_str!("../../../contracts/lens/IndexLens.bin");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LensHit {
    pub pool: Address,
    pub tokens: Vec<Address>,
    pub fee: Option<u32>,
    pub tick_spacing: Option<i32>,
}

pub fn creation_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        alloy_primitives::hex::decode(CREATION_HEX.trim())
            .expect("IndexLens.bin is hex creation code")
    })
}

/// Init code plus ABI constructor arguments.
///
/// `(address factory, bytes4 indexSel, uint256 start, uint256 count, uint8 mode)`.
pub fn call_data(
    factory: Address,
    index_sel: [u8; 4],
    start: u64,
    count: u64,
    mode: u8,
) -> Vec<u8> {
    let mut out = creation_code().to_vec();
    out.extend_from_slice(&word_address(factory));
    out.extend_from_slice(&word_selector(index_sel));
    out.extend_from_slice(&word_u64(start));
    out.extend_from_slice(&word_u64(count));
    out.extend_from_slice(&word_u64(u64::from(mode)));
    out
}

pub fn decode_rows(bytes: &[u8], mode: u8) -> Result<Vec<LensHit>, String> {
    let width = width(mode).ok_or_else(|| format!("lens mode {mode}"))?;
    if bytes.len() < 64 || bytes[..32] != MAGIC {
        return Err("lens payload has no magic".into());
    }
    let count = word_as_u64(&bytes[32..64]).ok_or_else(|| "lens count".to_string())?;
    if count > 4096 {
        return Err(format!("lens count {count}"));
    }
    let need = (2 + count as usize * width) * 32;
    if bytes.len() != need {
        return Err(format!(
            "lens payload {len} bytes, expected {need}",
            len = bytes.len()
        ));
    }
    let mut rows = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let row = &bytes[64 + index * width * 32..64 + (index + 1) * width * 32];
        rows.push(decode_row(row, mode, width)?);
    }
    Ok(rows)
}

fn decode_row(row: &[u8], mode: u8, width: usize) -> Result<LensHit, String> {
    let pool = address_word(&row[..32]);
    let mut tokens = Vec::new();
    let mut fee = None;
    let mut tick_spacing = None;
    if mode == 1 || mode == 2 {
        tokens.push(address_word(&row[32..64]));
        tokens.push(address_word(&row[64..96]));
    }
    if mode == 2 {
        fee = u32_word(&row[96..128]);
        tick_spacing = i32_word(&row[128..160]);
    }
    if mode == 3 {
        let n = word_as_u64(&row[32..64]).ok_or_else(|| "lens nCoins".to_string())?;
        if n > 8 {
            return Err(format!("lens nCoins {n}"));
        }
        for coin in 0..n as usize {
            let at = (2 + coin) * 32;
            tokens.push(address_word(&row[at..at + 32]));
        }
    }
    if mode == 0 && width != 1 {
        return Err("lens width".into());
    }
    Ok(LensHit {
        pool,
        tokens,
        fee,
        tick_spacing,
    })
}

fn width(mode: u8) -> Option<usize> {
    match mode {
        0 => Some(1),
        1 => Some(3),
        2 => Some(5),
        3 => Some(10),
        _ => None,
    }
}

fn word_u64(value: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    out
}

fn word_address(address: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(address.as_slice());
    out
}

fn word_selector(selector: [u8; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..4].copy_from_slice(&selector);
    out
}

fn address_word(word: &[u8]) -> Address {
    Address::from_slice(&word[12..32])
}

fn word_as_u64(word: &[u8]) -> Option<u64> {
    if word[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u64::from_be_bytes(word[24..32].try_into().ok()?))
}

/// `None` for `uint256` max, which is the lens sentinel for a reverted `fee()`.
fn u32_word(word: &[u8]) -> Option<u32> {
    if word.iter().all(|byte| *byte == 0xff) {
        return None;
    }
    if word[..28].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u32::from_be_bytes(word[28..32].try_into().ok()?))
}

/// `None` for `int256` max (`2^255 - 1`), the sentinel for a reverted `tickSpacing()`.
///
/// `uint256` max is `int256` −1, which is a real tick, so it is not the sentinel.
fn i32_word(word: &[u8]) -> Option<i32> {
    if word.len() != 32 {
        return None;
    }
    let signed = I256::from_be_bytes::<32>(word.try_into().ok()?);
    if signed == I256::MAX {
        return None;
    }
    i32::try_from(signed).ok()
}

/// Low 160 bits. Used when a direct `eth_call` checks one index.
pub fn address_from_u256(value: U256) -> Address {
    Address::from_slice(&value.to_be_bytes::<32>()[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};

    #[test]
    fn bytecode_carries_the_magic_push() {
        assert_eq!(keccak256("ratatoskr.lens.v1").0, MAGIC);
        let code = creation_code();
        assert!(code.windows(32).any(|window| window == MAGIC));
        assert!(code
            .windows(33)
            .any(|window| window[0] == 0x7f && window[1..] == MAGIC));
    }

    #[test]
    fn constructor_args_are_five_words() {
        let factory = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
        let data = call_data(factory, [0x1e, 0x3d, 0xd1, 0x8b], 0, 3, 1);
        assert_eq!(data.len(), creation_code().len() + 160);
        let args = &data[data.len() - 160..];
        assert_eq!(&args[12..32], factory.as_slice());
        assert_eq!(&args[32..36], &[0x1e, 0x3d, 0xd1, 0x8b]);
        assert_eq!(args[127], 3);
        assert_eq!(args[159], 1);
    }

    #[test]
    fn mode_two_sentinels_are_absent_and_a_real_tick_remains() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&MAGIC);
        payload.extend_from_slice(&word_u64(1));
        payload.extend_from_slice(&word_address(address!(
            "b4e16d0168e52d35cacd2c6185b44281ec28c9dc"
        )));
        payload.extend_from_slice(&word_address(address!(
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        )));
        payload.extend_from_slice(&word_address(address!(
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
        )));
        payload.extend_from_slice(&[0xff; 32]);
        let mut tick = [0xff_u8; 32];
        tick[0] = 0x7f;
        payload.extend_from_slice(&tick);
        let rows = decode_rows(&payload, 2).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].fee.is_none());
        assert!(rows[0].tick_spacing.is_none());
        assert_eq!(rows[0].tokens.len(), 2);

        let mut live = payload.clone();
        live[64 + 96..64 + 128].fill(0);
        live[64 + 96 + 30] = 0x00;
        live[64 + 96 + 31] = 150;
        live[64 + 128..].fill(0);
        live[64 + 128 + 31] = 50;
        let rows = decode_rows(&live, 2).unwrap();
        assert_eq!(rows[0].fee, Some(150));
        assert_eq!(rows[0].tick_spacing, Some(50));
    }

    #[test]
    fn negative_tick_is_not_the_sentinel() {
        let mut word = [0xff_u8; 32];
        word[31] = 0xf6; // -10
        assert_eq!(i32_word(&word), Some(-10));
        let mut sentinel = [0xff_u8; 32];
        sentinel[0] = 0x7f;
        assert_eq!(i32_word(&sentinel), None);
    }
}
