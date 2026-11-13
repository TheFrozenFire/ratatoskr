use std::str::FromStr;

use alloy_primitives::Address;
use num_bigint::BigUint;
use num_traits::Pow;
use searcher::{Token, NATIVE};

const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
const WBTC: &str = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";

pub(crate) struct Known {
    pub(crate) name: &'static str,
    pub(crate) token: Token,
}

pub(crate) fn token_of(address: Address) -> Token {
    let mut raw = [0_u8; 20];
    raw.copy_from_slice(address.as_slice());
    Token(raw)
}

pub(crate) fn known_tokens() -> Vec<Known> {
    vec![
        Known {
            name: "ETH",
            token: NATIVE,
        },
        named("WETH", WETH),
        named("USDC", USDC),
        named("USDT", USDT),
        named("DAI", DAI),
        named("WBTC", WBTC),
    ]
}

pub(crate) fn named(name: &'static str, hex_addr: &str) -> Known {
    let address = Address::from_str(hex_addr).expect("address");
    Known {
        name,
        token: token_of(address),
    }
}

pub(crate) fn known_named(known: &[Known], name: &str) -> Token {
    known
        .iter()
        .find(|item| item.name == name)
        .map(|item| item.token)
        .expect(name)
}

pub(crate) fn label(token: Token, known: &[Known]) -> String {
    if let Some(item) = known.iter().find(|item| item.token == token) {
        return item.name.into();
    }
    hex(token)
}

pub(crate) fn hex(token: Token) -> String {
    let mut out = String::from("0x");
    for byte in token.0 {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(crate) fn amount(exp: u32) -> BigUint {
    BigUint::from(10_u32).pow(exp)
}
