//! Multicall3 `aggregate3` call and the words a proxy returns.
//!
//! A dynamic offset in the array payload is counted from the first element
//! offset, the word after the array length. A tuple offset is counted from
//! the first word of that tuple.

use alloy_primitives::{Address, I256, U256};
pub use store_model::ProxyHead;

pub const MULTICALL: Address =
    alloy_primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");
pub const CALL_GAS: u64 = 50_000_000;
/// Feeds in one `aggregate3`. Five proxy calls each stay under [`CALL_GAS`].
pub const FEEDS_PER_CALL: usize = 20;

const AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
const DESCRIPTION: [u8; 4] = [0x72, 0x84, 0xe4, 0x16];
const VERSION: [u8; 4] = [0x54, 0xfd, 0x4d, 0x50];
const AGGREGATOR: [u8; 4] = [0x24, 0x5a, 0x7b, 0xfc];
const PHASE_ID: [u8; 4] = [0x58, 0x30, 0x3b, 0x10];
const MIN_ANSWER: [u8; 4] = [0x22, 0xad, 0xbc, 0x78];
const MAX_ANSWER: [u8; 4] = [0x70, 0xda, 0x2f, 0x67];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub success: bool,
    pub body: Vec<u8>,
}

pub fn proxy_calls(proxy: Address) -> [(Address, [u8; 4]); 5] {
    [
        (proxy, DECIMALS),
        (proxy, DESCRIPTION),
        (proxy, VERSION),
        (proxy, AGGREGATOR),
        (proxy, PHASE_ID),
    ]
}

pub fn bound_calls(aggregator: Address) -> [(Address, [u8; 4]); 2] {
    [(aggregator, MIN_ANSWER), (aggregator, MAX_ANSWER)]
}

pub fn encode_aggregate3(calls: &[(Address, &[u8])]) -> Vec<u8> {
    let mut elements = Vec::with_capacity(calls.len());
    for (target, data) in calls {
        let mut element = Vec::new();
        push_word(&mut element, address_word(*target));
        push_word(&mut element, U256::from(1u8));
        push_word(&mut element, U256::from(0x60u64));
        push_word(&mut element, U256::from(data.len()));
        element.extend_from_slice(data);
        let pad = (32 - (data.len() % 32)) % 32;
        element.extend(std::iter::repeat(0).take(pad));
        elements.push(element);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&AGGREGATE3);
    push_word(&mut out, U256::from(0x20u64));
    push_word(&mut out, U256::from(calls.len()));
    let mut cursor = calls.len() * 32;
    let mut tails = Vec::new();
    for element in &elements {
        push_word(&mut out, U256::from(cursor));
        cursor += element.len();
        tails.extend_from_slice(element);
    }
    out.extend_from_slice(&tails);
    out
}

/// `aggregate3` return data. The outer offset and each element offset follow
/// the ABI rule stated at the top of this module.
pub fn decode_aggregate3(data: &[u8]) -> eyre::Result<Vec<CallResult>> {
    let payload = word_at(data, 0)?;
    let base = usize_word(payload).ok_or_else(|| eyre::eyre!("aggregate offset"))?;
    let len = usize_word(word_at(data, base)?).ok_or_else(|| eyre::eyre!("aggregate length"))?;
    let head = base + 32;
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let rel = usize_word(word_at(data, head + index * 32)?)
            .ok_or_else(|| eyre::eyre!("result offset"))?;
        let element = head + rel;
        let success = word_at(data, element)? != U256::ZERO;
        let bytes_rel = usize_word(word_at(data, element + 32)?)
            .ok_or_else(|| eyre::eyre!("result bytes offset"))?;
        let bytes_at = element + bytes_rel;
        let blen = usize_word(word_at(data, bytes_at)?)
            .ok_or_else(|| eyre::eyre!("result bytes length"))?;
        let start = bytes_at + 32;
        let end = start + blen;
        if end > data.len() {
            return Err(eyre::eyre!("result bytes are short"));
        }
        out.push(CallResult {
            success,
            body: data[start..end].to_vec(),
        });
    }
    Ok(out)
}

pub fn decode_proxy_head(results: &[CallResult]) -> Result<ProxyHead, String> {
    if results.len() != 5 {
        return Err("proxy read is short".into());
    }
    let decimals = small_int(&results[0], "decimals", 8)? as u8;
    let description = if results[1].success {
        decode_string(&results[1].body).map_err(|_| "description".to_string())?
    } else {
        return Err("description reverted".into());
    };
    let version = word_body(&results[2], "version")?;
    let aggregator = address_body(&results[3], "aggregator")?;
    if aggregator == Address::ZERO {
        return Err("aggregator is empty".into());
    }
    let phase_id = small_int(&results[4], "phase", 16)? as u16;
    Ok(ProxyHead {
        decimals,
        description,
        version,
        aggregator,
        phase_id,
    })
}

pub fn decode_bound(result: &CallResult) -> Option<I256> {
    if !result.success || result.body.len() != 32 {
        return None;
    }
    let mut word = [0u8; 32];
    word.copy_from_slice(&result.body);
    Some(I256::from_be_bytes(word))
}

fn word_body(result: &CallResult, name: &str) -> Result<U256, String> {
    if !result.success {
        return Err(format!("{name} reverted"));
    }
    if result.body.len() != 32 {
        return Err(format!("{name} is short"));
    }
    Ok(U256::from_be_slice(&result.body))
}

fn address_body(result: &CallResult, name: &str) -> Result<Address, String> {
    let word = word_body(result, name)?;
    if word >> 160 != U256::ZERO {
        return Err(format!("{name} is not an address"));
    }
    Ok(Address::from_word(word.into()))
}

fn small_int(result: &CallResult, name: &str, bits: u32) -> Result<u64, String> {
    let word = word_body(result, name)?;
    let max = if bits == 64 {
        U256::from(u64::MAX)
    } else {
        (U256::from(1u8) << bits) - U256::from(1u8)
    };
    if word > max {
        return Err(format!("{name} does not fit"));
    }
    Ok(word.to::<u64>())
}

fn decode_string(body: &[u8]) -> eyre::Result<String> {
    if body.len() < 64 {
        return Err(eyre::eyre!("string is short"));
    }
    let offset =
        usize_word(U256::from_be_slice(&body[..32])).ok_or_else(|| eyre::eyre!("string offset"))?;
    let len = usize_word(word_at(body, offset)?).ok_or_else(|| eyre::eyre!("string length"))?;
    let start = offset + 32;
    let end = start + len;
    if end > body.len() {
        return Err(eyre::eyre!("string is short"));
    }
    let text =
        std::str::from_utf8(&body[start..end]).map_err(|_| eyre::eyre!("string is not utf-8"))?;
    Ok(text.to_string())
}

fn word_at(data: &[u8], at: usize) -> eyre::Result<U256> {
    let end = at + 32;
    if end > data.len() {
        return Err(eyre::eyre!("word is short"));
    }
    Ok(U256::from_be_slice(&data[at..end]))
}

fn usize_word(word: U256) -> Option<usize> {
    word.try_into().ok()
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn push_word(out: &mut Vec<u8>, word: U256) {
    out.extend_from_slice(&word.to_be_bytes::<32>());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_decimals_call_matches_the_multicall_encoding() {
        let proxy: Address = "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"
            .parse()
            .unwrap();
        let data = encode_aggregate3(&[(proxy, &DECIMALS)]);
        let expect = alloy_primitives::hex::decode(concat!(
            "82ad56cb",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000005f4ec3df9cbd43714fe2740f5e3616155c5b8419",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000004",
            "313ce56700000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap();
        assert_eq!(data, expect);
    }

    #[test]
    fn two_results_follow_the_aggregate_return_layout() {
        let data = alloy_primitives::hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000002",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "00000000000000000000000000000000000000000000000000000000000000c0",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000007",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap();
        let results = decode_aggregate3(&data).unwrap();
        assert!(results[0].success);
        assert_eq!(results[0].body, {
            let mut word = [0u8; 32];
            word[31] = 7;
            word.to_vec()
        });
        assert!(!results[1].success);
        assert!(results[1].body.is_empty());
    }
}
