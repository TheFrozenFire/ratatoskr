//! Seeds in `corpus/mainnet.yaml`.
//!
//! The file names the contracts the chain does not point at: the wrapper and
//! the enumeration roots. A list root is probed. A log root carries its event.
//! Symbols and decimals are read from the token.

use alloy_primitives::{Address, B256};
use std::sync::OnceLock;

const TEXT: &str = include_str!("../../../../corpus/mainnet.yaml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrap {
    pub token: Address,
    pub wrapper: Address,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub name: String,
    pub kind: u8,
    pub address: Address,
    pub topics: Vec<B256>,
    /// Probe this root for a list view. Log-only roots leave this false.
    pub list: bool,
    /// This address enumerates Curve registries. It is not itself a pool list.
    pub registries: bool,
}

impl Root {
    pub fn logs(&self) -> bool {
        !self.topics.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub wraps: Vec<Wrap>,
    pub multicall: Address,
    pub roots: Vec<Root>,
}

impl Document {
    pub fn by_name(&self, name: &str) -> &Root {
        self.roots
            .iter()
            .find(|root| root.name == name)
            .unwrap_or_else(|| panic!("seed {name}"))
    }

    pub fn by_address(&self, address: Address) -> Option<&Root> {
        self.roots.iter().find(|root| root.address == address)
    }

    pub fn kind_address(&self, kind: u8) -> Option<Address> {
        let mut found = self
            .roots
            .iter()
            .filter(|root| root.kind == kind && !root.registries);
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(first.address)
    }
}

pub fn document() -> &'static Document {
    static DOC: OnceLock<Document> = OnceLock::new();
    DOC.get_or_init(|| super::parse::parse_document(TEXT).expect("corpus/mainnet.yaml"))
}

/// The one enumeration root of `kind`. Several roots of one kind have no single home.
pub fn root_address(kind: u8) -> Option<Address> {
    document().kind_address(kind)
}

/// Native token and the wrapped token from `corpus/mainnet.yaml`.
///
/// The seed file names that custodial wrap. The first pair is the one a spot uses.
pub fn custodial_wrap() -> (Address, Address) {
    let wrap = document()
        .wraps
        .first()
        .expect("corpus/mainnet.yaml names a wrapper");
    (wrap.token, wrap.wrapper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, keccak256};
    use store_model::{UNI_V2, V2_FORK};

    #[test]
    fn the_seed_file_names_each_root_once() {
        let doc = document();
        assert!(doc.wraps.iter().any(|wrap| wrap.token == Address::ZERO));
        assert_ne!(doc.multicall, Address::ZERO);
        let mut names = std::collections::BTreeSet::new();
        for root in &doc.roots {
            assert!(names.insert(root.name.clone()), "duplicate {}", root.name);
            assert_ne!(root.address, Address::ZERO);
        }
        assert!(doc.roots.iter().any(|root| root.registries));
        assert!(doc.by_name("uni-v2").list);
        assert!(doc.by_name("uni-v3").logs());
        assert!(!doc.by_name("uni-v3").list);
        assert!(doc.by_name("metric").list && doc.by_name("metric").logs());
        assert_eq!(
            doc.kind_address(UNI_V2),
            Some(doc.by_name("uni-v2").address)
        );
        assert!(doc.kind_address(V2_FORK).is_none());
    }

    #[test]
    fn log_topics_are_the_events_the_decoder_already_matches() {
        let doc = document();
        assert_eq!(
            doc.by_name("uni-v3").topics[0],
            b256!("783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118")
        );
        assert_eq!(
            doc.by_name("uni-v4").topics[0],
            B256::from(keccak256(
                b"Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)"
            ))
        );
        assert_eq!(
            doc.by_name("balancer-v2").topics[0],
            b256!("3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e")
        );
        assert_eq!(
            doc.by_name("balancer-v2").topics[1],
            b256!("f5847d3f2197b16cdcd2098ec95d0905cd1abdaf415f07bb7cef2bba8ac5dec4")
        );
        assert_eq!(
            doc.by_name("metric").topics[0],
            b256!("4b36a0ddce54edb36597ee7d496df06c53fe875aba9d7257534a38d5177899aa")
        );
    }
}
