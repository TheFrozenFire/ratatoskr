//! Era1 file names from a mirror's directory listing.
//!
//! The filename carries the epoch and the accumulator prefix
//! (`mainnet-01510-87aec91b.era1`). Checksums do not. The ethPandaOps page
//! embeds `{ "name", "size" }` records; a plain file index links the same names.

use eyre::{eyre, WrapErr};
use std::collections::BTreeMap;

/// One era1 object on a mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraFile {
    pub epoch: u64,
    pub name: String,
    /// Present when the listing publishes a length. Saves a probe read.
    pub size: Option<u64>,
}

pub fn file_url(mirror: &str, name: &str) -> String {
    format!("{}/{}", mirror.trim_end_matches('/'), name)
}

pub fn listing_url(mirror: &str) -> String {
    format!("{}/", mirror.trim_end_matches('/'))
}

/// Pull `network-EEEEE-HHHHHHHH.era1` names out of a listing.
pub fn parse_catalog(text: &str, network: &str) -> eyre::Result<BTreeMap<u64, EraFile>> {
    let prefix = format!("{network}-");
    let mut files = BTreeMap::new();
    let mut rest = text;
    while let Some(at) = rest.find(&prefix) {
        let tail = &rest[at + prefix.len()..];
        if let Some((epoch, hash)) = file_suffix(tail) {
            let name = format!("{prefix}{epoch:05}-{hash}.era1");
            let size = size_near(&tail[19.min(tail.len())..]);
            let file = EraFile {
                epoch,
                name: name.clone(),
                size,
            };
            if let Some(prev) = files.insert(epoch, file) {
                if prev.name != name {
                    return Err(eyre!("epoch {epoch} listed as {} and {name}", prev.name));
                }
            }
        }
        rest = &rest[at + prefix.len()..];
    }
    if files.is_empty() {
        return Err(eyre!("no {network} era1 files in the listing"));
    }
    Ok(files)
}

/// `01510-87aec91b.era1` at the start of `tail`.
fn file_suffix(tail: &str) -> Option<(u64, String)> {
    let bytes = tail.as_bytes();
    if bytes.len() < 19 || bytes[5] != b'-' || &bytes[14..19] != b".era1" {
        return None;
    }
    if !bytes[..5].iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !bytes[6..14].iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let epoch = std::str::from_utf8(&bytes[..5]).ok()?.parse().ok()?;
    let hash = std::str::from_utf8(&bytes[6..14])
        .ok()?
        .to_ascii_lowercase();
    Some((epoch, hash))
}

fn size_near(after_name: &str) -> Option<u64> {
    let window = &after_name[..after_name.len().min(48)];
    let key = "\"size\":";
    let at = window.find(key)?;
    let digits: String = window[at + key.len()..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub fn epoch_of(block: u64) -> u64 {
    block / crate::era1::BLOCKS_PER_EPOCH
}

pub async fn load_catalog(
    http: &crate::http::EraHttp,
    mirror: &str,
    network: &str,
) -> eyre::Result<BTreeMap<u64, EraFile>> {
    let url = listing_url(mirror);
    let body = http.get_text(&url).await.wrap_err("fetch era1 listing")?;
    parse_catalog(&body, network).wrap_err("parse era1 listing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_json_keeps_epoch_and_size() {
        let text = r#"
            { "name": "mainnet-01510-87aec91b.era1", "size": 494333356 },
            { "name": "mainnet-01511-b041bb19.era1", "size": 499685896 },
            <a href="./${file.name}">skip</a>
        "#;
        let files = parse_catalog(text, "mainnet").unwrap();
        assert_eq!(files.len(), 2);
        let file = &files[&1510];
        assert_eq!(file.name, "mainnet-01510-87aec91b.era1");
        assert_eq!(file.size, Some(494_333_356));
    }
}
