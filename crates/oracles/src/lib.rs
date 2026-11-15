//! Chainlink feed directory, proxy configuration, phase history jobs, and answers.

mod abi;
mod answer;
mod configure;
mod directory;
mod follow;

pub use abi::{
    decode_aggregate3, decode_bound, decode_proxy_head, encode_aggregate3, CallResult, CALL_GAS,
    FEEDS_PER_CALL, MULTICALL,
};
pub use answer::{decode_answer_updated, ChainLog, ANSWER_UPDATED};
pub use configure::proxy_heads;
pub use directory::{parse_directory, DirectoryError, DIRECTORY_URL};
pub use store_model::{
    Answer, Configuration, DirectoryRow, Feed, Gauges, HistoryJob, Listing, Mismatch,
    ObservedAnswer, Phase, ProxyHead, Read,
};

use eyre::WrapErr;

/// Fetch the directory, read proxy configuration, then follow answer logs.
///
/// An empty `rpcs` list uses [`venues::default_call_rpcs`]. The venue catalog
/// stays closed. History jobs are not claimed. A follow error is logged and
/// the next pass resumes from the stored cursor. The caller opens the feed file.
pub async fn run(
    feeds: store::service::Feeds,
    rpcs: Vec<String>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<()> {
    tokio::task::spawn_blocking(move || pass(&feeds, rpcs, stop))
        .await
        .wrap_err("feed pass")??;
    Ok(())
}

fn pass(
    feeds: &store::service::Feeds,
    rpcs: Vec<String>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Listing> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("ratatoskr-oracles/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .wrap_err("directory client")?;
    let body = client
        .get(DIRECTORY_URL)
        .send()
        .wrap_err("feed directory")?
        .error_for_status()
        .wrap_err("feed directory")?
        .text()
        .wrap_err("feed directory")?;
    let rows = parse_directory(&body)?;
    let listing = feeds.apply_directory(&rows)?;
    observability::info!(
        generation = listing.generation,
        named = listing.named,
        "feeds"
    );
    let rpcs = if rpcs.is_empty() {
        venues::default_call_rpcs()
    } else {
        rpcs
    };
    let rpc = venues::Rpc::new(rpcs)?;
    configure::configure(feeds, &rpc)?;
    let _ = feeds.remember_answers();
    if let Ok(gauges) = feeds.gauges() {
        observability::note_feeds(
            gauges.feeds,
            gauges.configured,
            gauges.answers,
            gauges.jobs,
            gauges.follow,
            gauges.lookback,
        );
    }
    if *stop.borrow() {
        return Ok(listing);
    }
    loop {
        let stopped = || *stop.borrow();
        match follow::catch_up(feeds, &rpc, &stopped) {
            Ok(written) => observability::info!(written, "answers"),
            Err(err) => observability::error!(%err, "answer follow"),
        }
        if *stop.borrow() {
            return Ok(listing);
        }
        for _ in 0..12 {
            if stop.has_changed().unwrap_or(true) && *stop.borrow() {
                return Ok(listing);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
