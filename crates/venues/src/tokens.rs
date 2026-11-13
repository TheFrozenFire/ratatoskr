//! Token symbol and decimals.
//!
//! One creation-code `eth_call` of `contracts/lens/TokenLens.sol` reads a
//! batch. The constructor reverts the rows. Discovery runs this after pool
//! identities are written. A token the batch cannot read is read on its own.

use std::sync::OnceLock;

use alloy_primitives::Address;
use eyre::eyre;

use crate::catalog::TokenRecord;
use crate::lens::CALL_GAS;
use crate::rpc::Rpc;
use crate::seeds;
use crate::EntityStore;

/// keccak256("ratatoskr.token.v1").
const MAGIC: [u8; 32] = [
    0x4e, 0x3d, 0xa3, 0xdf, 0x77, 0x62, 0x6e, 0xed, 0x44, 0xd6, 0xbf, 0x65, 0xc4, 0x86, 0x7d, 0x11,
    0x13, 0x59, 0xd6, 0x5b, 0x6d, 0x3c, 0x5c, 0x7d, 0xae, 0xc4, 0x1b, 0xfb, 0x6f, 0xd1, 0x55, 0x83,
];
const CREATION_HEX: &str = include_str!("../../../contracts/lens/TokenLens.bin");
/// Tokens in one `eth_call`. Each token is two staticcalls with a 500_000 gas
/// stipend, so 32 stay under the 50_000_000 gas passed to `eth_call`.
const BATCH: usize = 32;
const SCAN: usize = 65_536;
const DIRECT_GAS: u64 = 1_000_000;
const SEL_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
const SEL_SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];

struct TokenRead {
    address: Address,
    symbol: String,
    decimals: Option<u8>,
}

enum Fetch {
    Shrink(String),
    Fatal(eyre::Report),
}

/// Wrapper links from the seed file. An empty symbol leaves a stored symbol.
pub(crate) async fn write_links<S: EntityStore>(store: &S) -> eyre::Result<()> {
    let mut rows = Vec::new();
    for wrap in &seeds::document().wraps {
        rows.push(TokenRecord {
            address: wrap.token,
            symbol: String::new(),
            decimals: None,
            wraps: Some(wrap.wrapper),
        });
        if wrap.wrapper != Address::ZERO {
            rows.push(TokenRecord {
                address: wrap.wrapper,
                symbol: String::new(),
                decimals: None,
                wraps: Some(wrap.token),
            });
        }
    }
    store.put_tokens(&rows).await
}

/// Read symbol and decimals for the wrapper and for tokens that have no record.
pub(crate) async fn walk<S: EntityStore>(
    rpc: &Rpc,
    store: &S,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    write_links(store).await?;
    let mut wrote = fill(rpc, store, &blank_wrappers(store).await?, stop).await?;
    loop {
        if *stop.borrow() {
            return Ok(wrote);
        }
        let missing = store.missing_tokens(SCAN).await?;
        if missing.is_empty() {
            break;
        }
        wrote += fill(rpc, store, &missing, stop).await?;
    }
    Ok(wrote)
}

async fn blank_wrappers<S: EntityStore>(store: &S) -> eyre::Result<Vec<Address>> {
    let mut out = Vec::new();
    for wrap in &seeds::document().wraps {
        if wrap.wrapper == Address::ZERO || out.contains(&wrap.wrapper) {
            continue;
        }
        match store.token(wrap.wrapper).await? {
            Some(row) if !row.symbol.is_empty() || row.decimals.is_some() => {}
            _ => out.push(wrap.wrapper),
        }
    }
    Ok(out)
}

async fn fill<S: EntityStore>(
    rpc: &Rpc,
    store: &S,
    tokens: &[Address],
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    let mut wrote = 0_u64;
    let mut at = 0;
    while at < tokens.len() {
        if crate::hold_for_pause(stop).await {
            return Ok(wrote);
        }
        let mut count = (tokens.len() - at).min(BATCH).max(1);
        loop {
            let batch = tokens[at..at + count].to_vec();
            let rpc_fetch = rpc.clone();
            let fetched =
                match tokio::task::spawn_blocking(move || read_batch(&rpc_fetch, &batch)).await {
                    Ok(fetched) => fetched,
                    Err(err) => Err(Fetch::Fatal(eyre!("token lens: {err}"))),
                };
            match fetched {
                Ok(rows) => {
                    let n = rows.len() as u64;
                    store.put_tokens(&rows).await?;
                    wrote += n;
                    tracing::info!(at, count, rows = n, "token lens");
                    at += count;
                    break;
                }
                Err(Fetch::Shrink(reason)) if count > 1 => {
                    let smaller = (count / 2).max(1);
                    tracing::warn!(at, count, smaller, %reason, "token lens shrink");
                    count = smaller;
                }
                Err(Fetch::Shrink(reason)) => {
                    let token = tokens[at];
                    tracing::warn!(%token, %reason, "token lens direct");
                    let rpc_one = rpc.clone();
                    let row = tokio::task::spawn_blocking(move || read_one(&rpc_one, token))
                        .await
                        .map_err(|err| eyre!("token {token}: {err}"))??;
                    store.put_tokens(&[row]).await?;
                    wrote += 1;
                    at += 1;
                    break;
                }
                Err(Fetch::Fatal(err)) => return Err(err),
            }
        }
    }
    Ok(wrote)
}

fn read_batch(rpc: &Rpc, tokens: &[Address]) -> Result<Vec<TokenRecord>, Fetch> {
    let call = rpc
        .eth_call(&call_data(tokens), None, CALL_GAS)
        .map_err(Fetch::Fatal)?;
    let rows = match decode(&call.bytes) {
        Ok(rows) => rows,
        Err(reason) => return Err(Fetch::Shrink(format!("{reason}; {}", call.message))),
    };
    if rows.len() != tokens.len() {
        return Err(Fetch::Shrink(format!("token count {}", rows.len())));
    }
    let mut out = Vec::with_capacity(rows.len());
    for (row, token) in rows.iter().zip(tokens) {
        if row.address != *token {
            return Err(Fetch::Shrink(format!("token address {token}")));
        }
        out.push(TokenRecord {
            address: *token,
            symbol: row.symbol.clone(),
            decimals: row.decimals,
            wraps: None,
        });
    }
    Ok(out)
}

fn read_one(rpc: &Rpc, token: Address) -> eyre::Result<TokenRecord> {
    let (symbol, decimals) = read_erc20(rpc, token)?;
    Ok(TokenRecord {
        address: token,
        symbol,
        decimals,
        wraps: None,
    })
}

fn read_erc20(rpc: &Rpc, token: Address) -> eyre::Result<(String, Option<u8>)> {
    let decimals_call = rpc.eth_call(&SEL_DECIMALS, Some(token), DIRECT_GAS)?;
    let symbol_call = rpc.eth_call(&SEL_SYMBOL, Some(token), DIRECT_GAS)?;
    let decimals = if !decimals_call.reverted && decimals_call.bytes.len() == 32 {
        decimals_of(&decimals_call.bytes)
    } else {
        None
    };
    let symbol = if symbol_call.reverted {
        String::new()
    } else {
        decode_symbol(&symbol_call.bytes)
    };
    Ok((symbol, decimals))
}

fn creation_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        alloy_primitives::hex::decode(CREATION_HEX.trim())
            .expect("TokenLens.bin is hex creation code")
    })
}

fn call_data(tokens: &[Address]) -> Vec<u8> {
    let mut out = creation_code().to_vec();
    out.extend_from_slice(&word_u64(0x20));
    out.extend_from_slice(&word_u64(tokens.len() as u64));
    for token in tokens {
        let mut word = [0_u8; 32];
        word[12..].copy_from_slice(token.as_slice());
        out.extend_from_slice(&word);
    }
    out
}

fn decode(bytes: &[u8]) -> Result<Vec<TokenRead>, String> {
    if bytes.len() < 64 || !bytes.len().is_multiple_of(32) || bytes[..32] != MAGIC {
        return Err("token payload has no magic".into());
    }
    let count = word_as_u64(&bytes[32..64]).ok_or_else(|| "token count".to_string())?;
    if count > 256 {
        return Err(format!("token count {count}"));
    }
    let need = (2 + count as usize * 3) * 32;
    if bytes.len() != need {
        return Err(format!(
            "token payload {len} bytes, expected {need}",
            len = bytes.len()
        ));
    }
    let mut rows = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let row = &bytes[64 + index * 96..64 + (index + 1) * 96];
        rows.push(TokenRead {
            address: Address::from_slice(&row[12..32]),
            decimals: decimals_of(&row[32..64]),
            symbol: symbol_word(&row[64..96]),
        });
    }
    Ok(rows)
}

/// `2^256-1` is the lens sentinel. 255 is the catalog's absent decimals byte.
fn decimals_of(word: &[u8]) -> Option<u8> {
    if word.len() != 32 || word.iter().all(|byte| *byte == 0xff) {
        return None;
    }
    if word[..31].iter().any(|byte| *byte != 0) || word[31] == 255 {
        return None;
    }
    Some(word[31])
}

fn symbol_word(word: &[u8]) -> String {
    text_of(word)
}

fn decode_symbol(bytes: &[u8]) -> String {
    if bytes.len() == 32 && (bytes[0] != 0 || bytes.iter().all(|byte| *byte == 0)) {
        return text_of(bytes);
    }
    if bytes.len() >= 64 {
        if let Some(offset) = word_usize(&bytes[..32]) {
            if offset + 32 <= bytes.len() {
                if let Some(len) = word_usize(&bytes[offset..offset + 32]) {
                    let end = offset + 32 + len;
                    if end <= bytes.len() {
                        return text_of(&bytes[offset + 32..end]);
                    }
                }
            }
        }
    }
    if bytes.len() == 32 {
        return text_of(bytes);
    }
    String::new()
}

fn text_of(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn word_u64(value: u64) -> [u8; 32] {
    let mut out = [0_u8; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    out
}

fn word_as_u64(word: &[u8]) -> Option<u64> {
    if word.len() < 32 || word[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u64::from_be_bytes(word[24..32].try_into().ok()?))
}

fn word_usize(word: &[u8]) -> Option<usize> {
    usize::try_from(word_as_u64(word)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    #[test]
    fn bytecode_carries_the_token_magic() {
        assert_eq!(keccak256("ratatoskr.token.v1").0, MAGIC);
        let code = creation_code();
        assert!(code.windows(32).any(|window| window == MAGIC));
        assert!(code
            .windows(33)
            .any(|window| window[0] == 0x7f && window[1..] == MAGIC));
    }

    #[test]
    fn call_data_appends_the_address_array() {
        let token = Address::from_slice(&[0x11; 20]);
        let data = call_data(&[token]);
        let code = creation_code();
        assert!(data.starts_with(code));
        let tail = &data[code.len()..];
        assert_eq!(tail.len(), 96);
        assert_eq!(&tail[..32], &word_u64(0x20));
        assert_eq!(&tail[32..64], &word_u64(1));
        assert_eq!(Address::from_slice(&tail[76..96]), token);
    }

    #[test]
    fn a_row_keeps_symbol_decimals_and_a_silent_token() {
        let token = Address::from_slice(&[0x22; 20]);
        let silent = Address::from_slice(&[0x33; 20]);
        let mut payload = Vec::new();
        payload.extend_from_slice(&MAGIC);
        payload.extend_from_slice(&word_u64(2));
        payload.extend_from_slice(&word_address(token));
        payload.extend_from_slice(&word_u64(6));
        payload.extend_from_slice(&symbol_bytes(b"USDC"));
        payload.extend_from_slice(&word_address(silent));
        payload.extend_from_slice(&[0xff; 32]);
        payload.extend_from_slice(&[0_u8; 32]);
        let rows = decode(&payload).unwrap();
        assert_eq!(rows[0].address, token);
        assert_eq!(rows[0].decimals, Some(6));
        assert_eq!(rows[0].symbol, "USDC");
        assert_eq!(rows[1].address, silent);
        assert_eq!(rows[1].decimals, None);
        assert_eq!(rows[1].symbol, "");
        assert!(decode(&[0_u8; 64]).is_err());
    }

    #[test]
    fn a_symbol_is_a_string_or_a_word() {
        let mut word = [0_u8; 32];
        word[..4].copy_from_slice(b"WETH");
        assert_eq!(decode_symbol(&word), "WETH");
        let mut abi = vec![0_u8; 96];
        abi[31] = 32;
        abi[63] = 4;
        abi[64..68].copy_from_slice(b"USDC");
        assert_eq!(decode_symbol(&abi), "USDC");
    }

    fn word_address(address: Address) -> [u8; 32] {
        let mut word = [0_u8; 32];
        word[12..].copy_from_slice(address.as_slice());
        word
    }

    fn symbol_bytes(text: &[u8]) -> [u8; 32] {
        let mut word = [0_u8; 32];
        word[..text.len()].copy_from_slice(text);
        word
    }
}
