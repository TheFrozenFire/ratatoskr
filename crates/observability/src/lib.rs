//! Process logging and the metrics this process publishes.
//!
//! A domain crate records what happened. It does not install a subscriber,
//! keep a gauge, or render an exposition format.

mod board;
pub mod cache;
mod metrics;

pub use board::{
    note_corpus, note_feeds, note_header_cursor, note_headers, note_peers, note_spots,
    note_token_lens, note_token_reading, note_token_records, snapshot as board_snapshot,
};
pub use metrics::{
    begin_batch, note_paused, prometheus_text, publish_summary, record_batch, set_block,
    status_json,
};
pub use tracing::{debug, error, info, info_span, warn};

/// Install the process logger. `RUST_LOG` overrides `default_filter`.
pub fn init(default_filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();
}

/// Install the process logger and close a span in the log when it ends.
pub fn init_spans(default_filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();
}
