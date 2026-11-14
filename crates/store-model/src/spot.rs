//! One token in one numeraire.

use alloy_primitives::Address;
use num_bigint::BigInt;

/// The sized round trip stored for one token in one numeraire.
///
/// `bought` is the tokens the buy paid. `sold` is the numeraire the sell of
/// those tokens returned. `hops` is the number of conduits, and the custodial
/// wrap counts as one. `block` is the earliest pair conduit on the route. A
/// route that is only the wrap stores block 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub token: Address,
    pub numeraire: Address,
    pub size: BigInt,
    pub bought: BigInt,
    pub sold: BigInt,
    pub hops: u8,
    pub block: u64,
}
