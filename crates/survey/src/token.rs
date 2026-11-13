use alloy_primitives::Address;
use num_bigint::BigUint;
use num_traits::Pow;
use searcher::{Token, NATIVE};
use venues::TokenRecord;

pub(crate) struct Known {
    pub(crate) name: String,
    pub(crate) token: Token,
}

pub(crate) fn token_of(address: Address) -> Token {
    let mut raw = [0_u8; 20];
    raw.copy_from_slice(address.as_slice());
    Token(raw)
}

/// The non-zero token whose record points at native ETH.
pub(crate) fn wrapper(records: &[TokenRecord]) -> Option<Token> {
    records
        .iter()
        .find(|row| row.address != Address::ZERO && row.wraps == Some(Address::ZERO))
        .map(|row| token_of(row.address))
}

pub(crate) fn labels(records: &[TokenRecord]) -> Vec<Known> {
    let mut out = Vec::new();
    if records.iter().any(|row| row.address == Address::ZERO) {
        out.push(Known {
            name: "ETH".into(),
            token: NATIVE,
        });
    }
    for row in records {
        if row.symbol.is_empty() || row.address == Address::ZERO {
            continue;
        }
        out.push(Known {
            name: row.symbol.clone(),
            token: token_of(row.address),
        });
    }
    out
}

pub(crate) fn label(token: Token, known: &[Known]) -> String {
    if let Some(item) = known.iter().find(|item| item.token == token) {
        return item.name.clone();
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
