//! Follow `AnswerUpdated` logs forward from the stored cursor.
//!
//! The first pass covers the last 8_000 blocks for every current aggregator,
//! then walks further back only for a feed that still has no answer, out to
//! that feed's heartbeat. A confirmed aggregator change reads configuration
//! again before the new aggregator's logs in the same range are stored.

use crate::answer::{
    answer_window, block_spans, decode_aggregator_confirmed, decode_answer_updated, ChainLog,
    AGGREGATOR_CONFIRMED, ANSWER_UPDATED,
};
use crate::configure;
use crate::store::{Feeds, ObservedAnswer};
use alloy_primitives::{Address, B256};
use venues::Rpc;

const SPAN: u64 = 2_000;
const RECENT_BLOCKS: u64 = 8_000;
const ADDRESS_CHUNK: usize = 20;

pub fn catch_up(feeds: &Feeds, rpc: &Rpc, stop: &dyn Fn() -> bool) -> eyre::Result<usize> {
    let head = rpc.block_number()?;
    if stop() {
        return Ok(0);
    }
    let mut written = 0usize;
    let next = feeds
        .follow_block()?
        .unwrap_or_else(|| head.saturating_sub(RECENT_BLOCKS));
    if next <= head {
        written += scan(feeds, rpc, next, head, stop)?;
    }
    if stop() || feeds.lookback_done()? {
        return Ok(written);
    }
    written += fill_quiet(feeds, rpc, stop)?;
    if !stop() {
        feeds.mark_lookback()?;
    }
    Ok(written)
}

fn scan(
    feeds: &Feeds,
    rpc: &Rpc,
    from: u64,
    to: u64,
    stop: &dyn Fn() -> bool,
) -> eyre::Result<usize> {
    let mut written = 0usize;
    for (start, end) in block_spans(from, to, SPAN) {
        if stop() {
            return Ok(written);
        }
        let mut targets = current_aggregators(feeds)?;
        let proxies = feeds.proxies()?;
        let mut observed = logs_for(rpc, start, end, &targets, ANSWER_UPDATED)?;
        let confirms = logs_for(rpc, start, end, &proxies, AGGREGATOR_CONFIRMED)?;
        if confirms
            .iter()
            .any(|log| decode_aggregator_confirmed(log).is_some())
        {
            configure::configure(feeds, rpc)?;
            targets = current_aggregators(feeds)?;
            observed.extend(logs_for(rpc, start, end, &targets, ANSWER_UPDATED)?);
        }
        let answers = observed
            .iter()
            .filter_map(decode_answer_updated)
            .collect::<Vec<_>>();
        written += feeds.commit_follow(end.saturating_add(1), &answers)?;
        tracing::info!(start, end, written, "answer follow");
    }
    Ok(written)
}

fn fill_quiet(feeds: &Feeds, rpc: &Rpc, stop: &dyn Fn() -> bool) -> eyre::Result<usize> {
    let mut written = 0usize;
    let Some(cursor) = feeds.follow_block()? else {
        return Ok(0);
    };
    let head = cursor.saturating_sub(1);
    for proxy in feeds.proxies()? {
        if stop() {
            return Ok(written);
        }
        if !feeds.answers(proxy)?.is_empty() {
            continue;
        }
        let Some(phase) = feeds.phases(proxy)?.into_iter().next_back() else {
            continue;
        };
        let Some(feed) = feeds.feed(proxy)? else {
            continue;
        };
        let start = answer_window(head, feed.row.heartbeat);
        if start >= cursor {
            continue;
        }
        let end = cursor.saturating_sub(1);
        tracing::info!(%proxy, start, end, "answer lookback");
        for (span_start, span_end) in block_spans(start, end, SPAN) {
            if stop() {
                return Ok(written);
            }
            let logs = logs_for(
                rpc,
                span_start,
                span_end,
                &[phase.aggregator],
                ANSWER_UPDATED,
            )?;
            let answers = logs
                .iter()
                .filter_map(decode_answer_updated)
                .collect::<Vec<_>>();
            // The follow cursor already sits at the recent head. These rows are older.
            written += commit_older(feeds, &answers)?;
        }
    }
    Ok(written)
}

fn commit_older(feeds: &Feeds, answers: &[ObservedAnswer]) -> eyre::Result<usize> {
    let next = feeds.follow_block()?.unwrap_or(0);
    Ok(feeds.commit_follow(next, answers)?)
}

fn current_aggregators(feeds: &Feeds) -> eyre::Result<Vec<Address>> {
    let mut out = Vec::new();
    for proxy in feeds.proxies()? {
        if let Some(phase) = feeds.phases(proxy)?.into_iter().next_back() {
            out.push(phase.aggregator);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn logs_for(
    rpc: &Rpc,
    from: u64,
    to: u64,
    addresses: &[Address],
    topic: B256,
) -> eyre::Result<Vec<ChainLog>> {
    if addresses.is_empty() || from > to {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for chunk in addresses.chunks(ADDRESS_CHUNK) {
        let logs = rpc.get_logs(from, to, chunk, topic)?;
        out.extend(logs.into_iter().map(|log| ChainLog {
            address: log.address,
            block: log.block,
            log_index: log.log_index,
            topics: log.topics,
            data: log.data,
        }));
    }
    Ok(out)
}
