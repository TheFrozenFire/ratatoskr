//! One initialized tick word stored beside a concentrated snapshot.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickWord {
    pub tick: i32,
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
}
