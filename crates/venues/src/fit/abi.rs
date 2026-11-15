//! Calldata for a venue quote, and the word the execution lens returns.
//!
//! The live path encodes `swap` on the lens. The other selectors stay so those
//! encodings can still be checked.

use alloy_primitives::{Address, B256, U256};

pub(super) const SEL_AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
pub(super) const SEL_GET_AMOUNTS: [u8; 4] = [0xd0, 0x6c, 0xa6, 0x1f];
pub(super) const SEL_V3_QUOTE: [u8; 4] = [0xc6, 0xa5, 0x02, 0x6a];
pub(super) const SEL_V4_QUOTE: [u8; 4] = [0xaa, 0x9d, 0x21, 0xcb];
pub(super) const SEL_DY_INT: [u8; 4] = [0x5e, 0x0d, 0x44, 0x3f];
pub(super) const SEL_DY_UINT: [u8; 4] = [0x55, 0x6d, 0x6e, 0x9f];
pub(super) const SEL_BAL3: [u8; 4] = [0x3e, 0xbc, 0x54, 0xe5];
pub(super) const SEL_BAL2: [u8; 4] = [0xf8, 0x4d, 0x06, 0x6e];

pub(super) struct RawOut {
    pub(super) ok: bool,
    pub(super) bytes: Vec<u8>,
    pub(super) error: Option<String>,
}

#[derive(Clone)]
pub(super) struct ChainOut {
    pub(super) amount_out: Option<U256>,
    pub(super) error: Option<String>,
}

pub(super) fn interpret(method: &str, raw: RawOut) -> ChainOut {
    if let Some(error) = raw.error {
        return ChainOut {
            amount_out: None,
            error: Some(error),
        };
    }
    if !raw.ok {
        return ChainOut {
            amount_out: None,
            error: Some(revert_text(&raw.bytes)),
        };
    }
    let amount = match method {
        "getAmountsOut" => decode_uint_array(&raw.bytes).and_then(|words| words.get(1).copied()),
        "queryBatchSwap" => decode_negative(&raw.bytes, 1),
        // `(amountIn, amountOut)`. The first word repeats the input.
        "quoteLiveExactInSingle" => word_at(&raw.bytes, 1),
        _ => first_word(&raw.bytes),
    };
    match amount {
        Some(amount_out) => ChainOut {
            amount_out: Some(amount_out),
            error: None,
        },
        None => ChainOut {
            amount_out: None,
            error: Some("return".into()),
        },
    }
}

pub(super) fn first_word(bytes: &[u8]) -> Option<U256> {
    word_at(bytes, 0)
}

pub(super) fn word_at(bytes: &[u8], index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    let word = bytes.get(start..start.checked_add(32)?)?;
    Some(U256::from_be_slice(word))
}

pub(super) fn decode_uint_array(data: &[u8]) -> Option<Vec<U256>> {
    let offset = as_usize(read_word(data, 0)?)?;
    let len = as_usize(read_word(data, offset)?)?;
    let start = offset.checked_add(32)?;
    let mut words = Vec::with_capacity(len);
    for index in 0..len {
        let at = start.checked_add(index.checked_mul(32)?)?;
        words.push(read_word(data, at)?);
    }
    Some(words)
}

pub(super) fn decode_negative(data: &[u8], index: usize) -> Option<U256> {
    let words = decode_uint_array(data)?;
    let word = *words.get(index)?;
    let bytes = word.to_be_bytes::<32>();
    if bytes[0] & 0x80 == 0 {
        return None;
    }
    Some((!word).saturating_add(U256::from(1u8)))
}

pub(super) fn decode_aggregate(data: &[u8]) -> Option<Vec<(bool, Vec<u8>)>> {
    let base = as_usize(read_word(data, 0)?)?;
    let len = as_usize(read_word(data, base)?)?;
    let table = base.checked_add(32)?;
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let rel = as_usize(read_word(data, table + index * 32)?)?;
        let element = table.checked_add(rel)?;
        let success = !read_word(data, element)?.is_zero();
        let bytes_rel = as_usize(read_word(data, element + 32)?)?;
        let bytes_at = element.checked_add(bytes_rel)?;
        let n = as_usize(read_word(data, bytes_at)?)?;
        let start = bytes_at.checked_add(32)?;
        let end = start.checked_add(n)?;
        out.push((success, data.get(start..end)?.to_vec()));
    }
    Some(out)
}

pub(super) fn read_word(data: &[u8], at: usize) -> Option<U256> {
    let end = at.checked_add(32)?;
    Some(U256::from_be_slice(data.get(at..end)?))
}

pub(super) fn as_usize(value: U256) -> Option<usize> {
    if value > U256::from(usize::MAX) {
        return None;
    }
    Some(value.to::<usize>())
}

pub(super) fn revert_text(data: &[u8]) -> String {
    if data.len() >= 68 && data.starts_with(&[0x08, 0xc3, 0x79, 0xa0]) {
        if let Some(len) = as_usize(read_word(data, 36).unwrap_or(U256::ZERO)) {
            let start = 68usize;
            let end = start.saturating_add(len).min(data.len());
            if let Ok(text) = std::str::from_utf8(&data[start..end]) {
                return clip(text);
            }
        }
    }
    if data.is_empty() {
        return "reverted".into();
    }
    clip(&format!(
        "reverted {}",
        hex_prefix(&data[..data.len().min(32)])
    ))
}

pub(super) fn clip(text: &str) -> String {
    let text = if text.contains("/v2/") {
        "rpc error"
    } else {
        text
    };
    text.chars().take(160).collect()
}

pub(super) fn push_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

pub(super) fn push_addr(out: &mut Vec<u8>, value: Address) {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_slice());
    out.extend_from_slice(&word);
}

pub(super) fn push_bool(out: &mut Vec<u8>, value: bool) {
    push_u256(out, U256::from(u8::from(value)));
}

pub(super) fn push_i32_word(out: &mut Vec<u8>, value: i32) {
    let mut word = if value < 0 { [0xffu8; 32] } else { [0u8; 32] };
    word[28..].copy_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&word);
}

pub(super) fn push_bytes_tail(out: &mut Vec<u8>, data: &[u8]) {
    push_u256(out, U256::from(data.len()));
    out.extend_from_slice(data);
    let pad = (32 - (data.len() % 32)) % 32;
    out.extend(std::iter::repeat_n(0u8, pad));
}

pub(super) fn selector(signature: &str) -> [u8; 4] {
    let hash = alloy_primitives::keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

pub(super) fn encode_quote_v2(
    pair: Address,
    token_in: Address,
    token_out: Address,
    amount: U256,
    fee_pips: u32,
) -> Vec<u8> {
    let mut out = selector("quoteV2(address,address,address,uint256,uint256)").to_vec();
    push_addr(&mut out, pair);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(fee_pips));
    out
}

pub(super) fn encode_quote_v3(
    pool: Address,
    token_in: Address,
    token_out: Address,
    zero_for_one: bool,
    amount: U256,
) -> Vec<u8> {
    let mut out = selector("quoteV3(address,address,address,bool,int256)").to_vec();
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_bool(&mut out, zero_for_one);
    push_u256(&mut out, amount);
    out
}

pub(super) fn encode_quote_v4(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    zero_for_one: bool,
    amount: U256,
) -> Vec<u8> {
    let mut out = selector("quoteV4(address,address,uint24,int24,address,bool,int256)").to_vec();
    push_addr(&mut out, currency0);
    push_addr(&mut out, currency1);
    push_u256(&mut out, U256::from(fee));
    push_i32_word(&mut out, tick_spacing);
    push_addr(&mut out, hooks);
    push_bool(&mut out, zero_for_one);
    push_u256(&mut out, amount);
    out
}

pub(super) fn encode_quote_metric(
    pool: Address,
    token_in: Address,
    token_out: Address,
    zero_for_one: bool,
    amount: U256,
    limit: U256,
) -> Vec<u8> {
    let mut out = selector("quoteMetric(address,address,address,bool,int128,uint128)").to_vec();
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_bool(&mut out, zero_for_one);
    push_u256(&mut out, amount);
    push_u256(&mut out, limit);
    out
}

pub(super) fn encode_quote_curve(
    pool: Address,
    token_in: Address,
    token_out: Address,
    i: u64,
    j: u64,
    amount: U256,
    uint_index: bool,
) -> Vec<u8> {
    let mut out =
        selector("quoteCurve(address,address,address,uint256,uint256,uint256,bool)").to_vec();
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, U256::from(i));
    push_u256(&mut out, U256::from(j));
    push_u256(&mut out, amount);
    push_bool(&mut out, uint_index);
    out
}

pub(super) fn encode_quote_bal2(
    vault: Address,
    pool_id: B256,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Vec<u8> {
    let mut out = selector("quoteBalancerV2(address,bytes32,address,address,uint256)").to_vec();
    push_addr(&mut out, vault);
    out.extend_from_slice(pool_id.as_slice());
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    out
}

pub(super) fn encode_quote_bal3(
    vault: Address,
    pool: Address,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Vec<u8> {
    let mut out = selector("quoteBalancerV3(address,address,address,address,uint256)").to_vec();
    push_addr(&mut out, vault);
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    out
}

pub(super) fn encode_amounts_out(amount: U256, token_in: Address, token_out: Address) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_GET_AMOUNTS);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(0x40u64));
    push_u256(&mut out, U256::from(2u8));
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    out
}

pub(super) fn encode_v3_quote(
    token_in: Address,
    token_out: Address,
    amount: U256,
    fee: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_V3_QUOTE);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(fee));
    push_u256(&mut out, U256::ZERO);
    out
}

pub(super) fn encode_v4_quote(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    zero_for_one: bool,
    amount: U256,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_V4_QUOTE);
    push_u256(&mut out, U256::from(0x20u64));
    push_addr(&mut out, currency0);
    push_addr(&mut out, currency1);
    push_u256(&mut out, U256::from(fee));
    push_i32_word(&mut out, tick_spacing);
    push_addr(&mut out, hooks);
    push_bool(&mut out, zero_for_one);
    push_u256(&mut out, amount);
    push_u256(&mut out, U256::from(0x100u64));
    push_u256(&mut out, U256::ZERO);
    out
}

pub(super) fn encode_get_dy(uint_index: bool, i: u64, j: u64, amount: U256) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if uint_index {
        &SEL_DY_UINT
    } else {
        &SEL_DY_INT
    });
    push_u256(&mut out, U256::from(i));
    push_u256(&mut out, U256::from(j));
    push_u256(&mut out, amount);
    out
}

pub(super) fn encode_bal3(
    pool: Address,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_BAL3);
    push_addr(&mut out, pool);
    push_addr(&mut out, token_in);
    push_addr(&mut out, token_out);
    push_u256(&mut out, amount);
    push_addr(&mut out, Address::ZERO);
    push_u256(&mut out, U256::from(0xc0u64));
    push_u256(&mut out, U256::ZERO);
    out
}

pub(super) fn encode_bal2(
    pool_id: B256,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Vec<u8> {
    let mut step = Vec::new();
    step.extend_from_slice(pool_id.as_slice());
    push_u256(&mut step, U256::ZERO);
    push_u256(&mut step, U256::from(1u8));
    push_u256(&mut step, amount);
    push_u256(&mut step, U256::from(0xa0u64));
    push_u256(&mut step, U256::ZERO);
    let mut swaps = Vec::new();
    push_u256(&mut swaps, U256::from(1u8));
    push_u256(&mut swaps, U256::from(0x20u64));
    swaps.extend_from_slice(&step);
    let mut assets = Vec::new();
    push_u256(&mut assets, U256::from(2u8));
    push_addr(&mut assets, token_in);
    push_addr(&mut assets, token_out);
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_BAL2);
    push_u256(&mut out, U256::ZERO);
    push_u256(&mut out, U256::from(0xe0u64));
    push_u256(&mut out, U256::from(0xe0u64 + swaps.len() as u64));
    push_addr(&mut out, Address::ZERO);
    push_bool(&mut out, false);
    push_addr(&mut out, Address::ZERO);
    push_bool(&mut out, false);
    out.extend_from_slice(&swaps);
    out.extend_from_slice(&assets);
    out
}

pub(super) fn encode_aggregate3(calls: &[(Address, &[u8])]) -> Vec<u8> {
    let mut elements = Vec::with_capacity(calls.len());
    for (target, data) in calls {
        let mut element = Vec::new();
        push_addr(&mut element, *target);
        push_bool(&mut element, true);
        push_u256(&mut element, U256::from(0x60u64));
        push_bytes_tail(&mut element, data);
        elements.push(element);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&SEL_AGGREGATE3);
    push_u256(&mut out, U256::from(0x20u64));
    push_u256(&mut out, U256::from(calls.len()));
    let mut cursor = calls.len() * 32;
    let mut tails = Vec::new();
    for element in &elements {
        push_u256(&mut out, U256::from(cursor));
        cursor += element.len();
        tails.extend_from_slice(element);
    }
    out.extend_from_slice(&tails);
    out
}

pub(super) fn hex_prefix(bytes: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(bytes))
}
