//! Log filter in the shape `eth_getLogs` uses.
//!
//! Addresses are alternatives. Topics at one position are alternatives.
//! Positions are combined: a log must match every position that has a topic.
//! An empty position is a wildcard. The bloom test has the same shape, so a
//! block that fails it cannot contain a matching log.

use alloy_primitives::{keccak256, Address, Bloom, BloomInput, B256};
use eyre::{eyre, WrapErr};

#[derive(Debug, Clone)]
pub struct LogFilter {
    pub addresses: Vec<Address>,
    /// `topics[i]` is the set of accepted values at topic position `i`.
    pub topics: [Vec<B256>; 4],
}

impl LogFilter {
    pub fn from_parts(
        addresses: Vec<Address>,
        topic0: &[String],
        topic1: &[String],
        topic2: &[String],
        topic3: &[String],
    ) -> eyre::Result<Self> {
        let topics = [
            parse_topics(topic0)?,
            parse_topics(topic1)?,
            parse_topics(topic2)?,
            parse_topics(topic3)?,
        ];
        let filter = Self { addresses, topics };
        if !filter.is_selective() {
            return Err(eyre!(
                "pass an --address or a topic; an empty filter would read every receipt"
            ));
        }
        Ok(filter)
    }

    pub fn is_selective(&self) -> bool {
        !self.addresses.is_empty() || self.topics.iter().any(|group| !group.is_empty())
    }

    /// False only when the bloom proves the block has no matching log.
    pub fn bloom_maybe(&self, bloom: &Bloom) -> bool {
        if !self.addresses.is_empty()
            && !self.addresses.iter().any(|address| in_bloom(bloom, address.as_slice()))
        {
            return false;
        }
        for group in &self.topics {
            if group.is_empty() {
                continue;
            }
            if !group.iter().any(|topic| in_bloom(bloom, topic.as_slice())) {
                return false;
            }
        }
        true
    }

    pub fn matches(&self, address: Address, topics: &[B256]) -> bool {
        if !self.addresses.is_empty() && !self.addresses.contains(&address) {
            return false;
        }
        for (position, group) in self.topics.iter().enumerate() {
            if group.is_empty() {
                continue;
            }
            match topics.get(position) {
                Some(topic) if group.contains(topic) => {}
                _ => return false,
            }
        }
        true
    }
}

fn in_bloom(bloom: &Bloom, bytes: &[u8]) -> bool {
    bloom.contains_input(BloomInput::Raw(bytes))
}

/// A 32-byte hex word, or an event signature hashed with keccak256.
pub fn parse_topic(text: &str) -> eyre::Result<B256> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return format!("0x{hex}").parse().wrap_err("topic hex");
        }
        return Err(eyre!("topic hex must be 32 bytes"));
    }
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return Err(eyre!("event signature must not contain whitespace"));
    }
    Ok(keccak256(trimmed.as_bytes()))
}

fn parse_topics(values: &[String]) -> eyre::Result<Vec<B256>> {
    values.iter().map(|value| parse_topic(value)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn event_signature_hashes_and_hex_is_taken_as_is() {
        let hashed = parse_topic("PoolCreated(address,address,uint24,int24,address)").unwrap();
        assert_eq!(
            hashed,
            b256!("783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118")
        );
        let raw = parse_topic(
            "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118",
        )
        .unwrap();
        assert_eq!(hashed, raw);
    }

    #[test]
    fn bloom_requires_every_position_and_any_alternative() {
        let address: Address = "0x1F98431c8aD98523631AE4a59f267346ea31F984".parse().unwrap();
        let topic = parse_topic("PoolCreated(address,address,uint24,int24,address)").unwrap();
        let other = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let mut bloom = Bloom::ZERO;
        bloom.accrue(BloomInput::Raw(address.as_slice()));
        bloom.accrue(BloomInput::Raw(topic.as_slice()));

        let filter = LogFilter::from_parts(
            vec![address],
            &["PoolCreated(address,address,uint24,int24,address)".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert!(filter.bloom_maybe(&bloom));
        assert!(filter.matches(address, &[topic]));
        assert!(!filter.matches(address, &[other]));

        let both = LogFilter {
            addresses: vec![address],
            topics: [vec![topic, other], vec![other], Vec::new(), Vec::new()],
        };
        assert!(!both.bloom_maybe(&bloom));
        assert!(!both.matches(address, &[topic]));
    }
}
