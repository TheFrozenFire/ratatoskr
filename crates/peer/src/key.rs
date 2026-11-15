//! Parse `peer.key` as hex.

use eyre::WrapErr;
use secp256k1::SecretKey;

pub(crate) fn parse_node_key(text: &str) -> eyre::Result<SecretKey> {
    let text = text.trim();
    if text.is_empty() {
        return Err(eyre::eyre!("peer.key is missing"));
    }
    let bytes = hex_decode(text).wrap_err("peer.key")?;
    SecretKey::from_slice(&bytes).wrap_err("peer.key")
}

fn hex_decode(text: &str) -> eyre::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(eyre::eyre!("odd hex length"));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_val(bytes[index])?;
        let lo = hex_val(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_val(byte: u8) -> eyre::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(eyre::eyre!("bad hex")),
    }
}
