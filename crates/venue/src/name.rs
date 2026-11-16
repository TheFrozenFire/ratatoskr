//! The action a stored pool key names on one ordered pair.

use alloy_primitives::Address;

use crate::{root_address, VenueRow, BALANCER_V2, BALANCER_V3, UNI_V4};

/// Name the swap that spends `token_in` and pays `token_out` through `key`.
///
/// Uniswap v4 and Balancer v2 take the factory on `venue` when it is present.
/// Balancer v3 takes its root contract. Every other kind takes the low 20
/// bytes of the pool id. A missing factory falls back to the root contract,
/// then to those low 20 bytes.
pub fn swap_action(
    token_in: action::Token,
    token_out: action::Token,
    key: &routing::Action,
    venue: Option<&VenueRow>,
) -> Result<action::Action, action::ActionError> {
    let invariant = match key.kind {
        1..=4 => action::Invariant::Product,
        5..=10 => action::Invariant::Concentrated,
        11 | 12 => action::Invariant::Weighted,
        13 => action::Invariant::Stable,
        14 => action::Invariant::Bin,
        _ => action::Invariant::Product,
    };
    let contract = action::Address::new(contract_of(key, venue))?;
    Ok(action::Action::Swap(action::Swap::new(
        contract, key.pool, token_in, token_out, invariant,
    )?))
}

fn contract_of(key: &routing::Action, venue: Option<&VenueRow>) -> [u8; 20] {
    match key.kind {
        UNI_V4 | BALANCER_V2 => {
            if let Some(row) = venue {
                if row.factory != Address::ZERO {
                    return bytes_of(row.factory);
                }
            }
            root_address(key.kind)
                .map(bytes_of)
                .unwrap_or_else(|| low20(key))
        }
        BALANCER_V3 => root_address(BALANCER_V3)
            .map(bytes_of)
            .unwrap_or_else(|| low20(key)),
        _ => low20(key),
    }
}

fn bytes_of(address: Address) -> [u8; 20] {
    let mut bytes = [0_u8; 20];
    bytes.copy_from_slice(address.as_slice());
    bytes
}

fn low20(key: &routing::Action) -> [u8; 20] {
    let mut bytes = [0_u8; 20];
    bytes.copy_from_slice(&key.pool[12..]);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, B256};

    fn token(byte: u8) -> action::Token {
        action::Token([byte; 20])
    }

    fn key(kind: u8, pool: u8) -> routing::Action {
        routing::Action {
            kind,
            pool: [pool; 32],
        }
    }

    #[test]
    fn a_factory_pool_uses_the_low_address() {
        let action = swap_action(token(1), token(2), &key(1, 9), None).expect("swap");
        let action::Action::Swap(swap) = action else {
            panic!("swap");
        };
        assert_eq!(swap.contract.0, [9; 20]);
        assert_eq!(swap.invariant, action::Invariant::Product);
    }

    #[test]
    fn a_v4_pool_uses_the_venue_factory() {
        let factory = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let row = VenueRow {
            kind: UNI_V4,
            id: B256::from([4; 32]),
            factory,
            tokens: Vec::new(),
            fee: None,
            tick_spacing: None,
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let action = swap_action(token(1), token(2), &key(UNI_V4, 4), Some(&row)).expect("swap");
        let action::Action::Swap(swap) = action else {
            panic!("swap");
        };
        assert_eq!(swap.contract.0.as_slice(), factory.as_slice());
        assert_eq!(swap.invariant, action::Invariant::Concentrated);
    }
}
