//! Interface to an EVM chain.
//!
//! [`rpc`] is JSON-RPC. [`lens`], [`detail`], [`tick`], and [`token`] are
//! creation-code `eth_call`s. [`decode`] reads a creation log into a pool
//! identity. [`seeds`] names the contracts those calls address. The header
//! file is `store::service::HeaderChain`.

pub mod decode;
pub mod detail;
pub mod lens;
pub mod rpc;
pub mod seeds;
pub mod tick;
pub mod token;

pub use rpc::{default_call_rpcs, Rpc, RpcLog};
pub use seeds::{custodial_wrap, root_address};
