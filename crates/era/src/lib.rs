//! Pre-merge history from era1 files, read with HTTP range requests.
//!
//! An era1 file stores each block as a header, a body, receipts, and a total
//! difficulty. The block index at the end of the file points at each header.
//! Callers that want logs read headers, test the logs bloom, and only then
//! read the receipt record. Block bodies are not requested.

mod catalog;
mod era1;
mod http;
mod scan;

pub use http::EraHttp;
pub use scan::{
    enumerate_headers, enumerate_logs, logs_for_blocks, logs_from_snappy, receipts_for_blocks,
    EraBlock, HeaderRow,
};
