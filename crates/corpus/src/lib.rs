//! Receipt bundles inside a stored block.
//!
//! The corpus file is `store::service::Corpus`. Decoding a receipt bundle
//! does not open that file.

mod logs;

pub use logs::{decode_receipt_bundle, encode_receipt_bundle, flatten_logs, logs_in_block};
