//! Chainlink feed directory, proxy configuration, and one history job per phase.

mod abi;
mod configure;
mod directory;
mod store;

pub use abi::{
    decode_aggregate3, decode_bound, decode_proxy_head, encode_aggregate3, CallResult, ProxyHead,
    CALL_GAS, FEEDS_PER_CALL, MULTICALL,
};
pub use configure::proxy_heads;
pub use directory::{parse_directory, DirectoryError, DirectoryRow, DIRECTORY_URL};
pub use store::{Configuration, Error, Feed, Feeds, HistoryJob, Listing, Mismatch, Phase, Read};

use eyre::WrapErr;
use std::path::{Path, PathBuf};

pub fn default_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr/feeds.redb")
}

/// Fetch the directory, read proxy configuration, insert phase jobs, then wait.
///
/// An empty `rpcs` list uses [`venues::default_call_rpcs`]. The venue catalog
/// stays closed. History jobs are not claimed.
pub async fn run(
    path: PathBuf,
    rpcs: Vec<String>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<()> {
    let listed = tokio::task::spawn_blocking(move || pass(&path, rpcs))
        .await
        .wrap_err("feed pass")??;
    tracing::info!(
        generation = listed.generation,
        named = listed.named,
        "feeds"
    );
    if *stop.borrow() {
        return Ok(());
    }
    let _ = stop.changed().await;
    Ok(())
}

fn pass(path: &Path, rpcs: Vec<String>) -> eyre::Result<Listing> {
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
    let feeds = Feeds::open(path)?;
    let listing = feeds.apply_directory(&body)?;
    let rpcs = if rpcs.is_empty() {
        venues::default_call_rpcs()
    } else {
        rpcs
    };
    let rpc = venues::Rpc::new(rpcs)?;
    configure::configure(&feeds, &rpc)?;
    Ok(listing)
}
