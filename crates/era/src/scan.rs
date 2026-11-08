//! Read the headers a block range needs, then receipts for bloom hits.

use crate::{
    catalog::{self, EraFile},
    era1::{self, HeaderCut},
    http::{Bucket, EraHttp},
};
use alloy_consensus::Header;
use alloy_primitives::{Bloom, B256};
use eyre::{eyre, WrapErr};
use filter::LogFilter;
use futures::{stream, StreamExt};
use model::NormalizedLog;
use serde::Serialize;
use std::collections::HashSet;

/// Header fields a later bloom test can reuse. Bodies are not included.
#[derive(Debug, Clone, Serialize)]
pub struct HeaderRow {
    pub number: u64,
    pub hash: B256,
    pub parent: B256,
    pub timestamp: u64,
    pub receipts_root: B256,
    pub logs_bloom: Bloom,
}

struct LocatedHeader {
    row: HeaderRow,
    receipt_at: u64,
}

struct Fetched {
    located: LocatedHeader,
    entry_len: u64,
}

pub async fn enumerate_headers(
    http: &EraHttp,
    mirror: &str,
    network: &str,
    from: u64,
    to: u64,
    inflight: usize,
    window: Option<u64>,
) -> eyre::Result<Vec<HeaderRow>> {
    let files = catalog::load_catalog(http, mirror, network).await?;
    let mut rows = Vec::new();
    for epoch in catalog::epoch_of(from)..=catalog::epoch_of(to) {
        let part = read_epoch(http, mirror, &files, epoch, from, to, inflight, window).await?;
        rows.extend(part.into_iter().map(|header| header.row));
    }
    Ok(rows)
}

/// One era1 block whose receipt payload was read and checked.
#[derive(Debug, Clone)]
pub struct EraBlock {
    pub header: HeaderRow,
    /// Snappy frame from the era1 receipt record. Empty when that record is empty.
    pub receipts_snappy: Vec<u8>,
    pub logs: Vec<NormalizedLog>,
}

/// Logs for specific blocks. Each block costs a header range and a receipt
/// range. The receipt root is checked against that header.
pub async fn logs_for_blocks(
    http: &EraHttp,
    mirror: &str,
    network: &str,
    blocks: &[u64],
    filter: &LogFilter,
    inflight: usize,
) -> eyre::Result<Vec<NormalizedLog>> {
    let mut logs = Vec::new();
    for block in receipts_for_blocks(http, mirror, network, blocks, filter, inflight).await? {
        logs.extend(block.logs);
    }
    logs.sort_by_key(|log| (log.block_number, log.tx_index, log.log_index));
    Ok(logs)
}

/// Receipt payloads and matching logs for specific blocks.
///
/// `receipts_snappy` is the exact frame a retry would otherwise download.
pub async fn receipts_for_blocks(
    http: &EraHttp,
    mirror: &str,
    network: &str,
    blocks: &[u64],
    filter: &LogFilter,
    inflight: usize,
) -> eyre::Result<Vec<EraBlock>> {
    if blocks.is_empty() {
        return Ok(Vec::new());
    }
    let files = catalog::load_catalog(http, mirror, network).await?;
    let mut by_epoch: std::collections::BTreeMap<u64, Vec<u64>> = std::collections::BTreeMap::new();
    for &block in blocks {
        by_epoch
            .entry(catalog::epoch_of(block))
            .or_default()
            .push(block);
    }
    let mut found = Vec::new();
    for (epoch, epoch_blocks) in by_epoch {
        let file = epoch_file(&files, epoch)?;
        let url = catalog::file_url(mirror, &file.name);
        let file_len = match file.size {
            Some(size) => size,
            None => http.object_len(&url).await?,
        };
        let index = load_index(http, &url, file_len).await?;
        let mut jobs = Vec::with_capacity(epoch_blocks.len());
        for block in epoch_blocks {
            let at = index
                .header_at(block)
                .ok_or_else(|| eyre!("block {block} is outside {}", file.name))?;
            jobs.push((block, at));
        }
        tracing::info!(epoch, blocks = jobs.len(), "era receipts for bloom hits");
        found.extend(fetch_block_logs(http, &url, file_len, &jobs, filter, inflight).await?);
    }
    found.sort_by_key(|block| block.header.number);
    Ok(found)
}

/// Decode a stored era1 receipt frame and return logs that match `filter`.
pub fn logs_from_snappy(
    header: &HeaderRow,
    payload: &[u8],
    filter: &LogFilter,
) -> eyre::Result<Vec<NormalizedLog>> {
    let receipts = era1::decode_receipts(payload)?;
    let root = era1::receipt_root(&receipts);
    if root != header.receipts_root {
        return Err(eyre!(
            "block {} receipt root {root} does not match header {}",
            header.number,
            header.receipts_root
        ));
    }
    Ok(matching_logs(header, &receipts, filter))
}

async fn fetch_block_logs(
    http: &EraHttp,
    url: &str,
    file_len: u64,
    jobs: &[(u64, u64)],
    filter: &LogFilter,
    inflight: usize,
) -> eyre::Result<Vec<EraBlock>> {
    const WINDOW: u64 = 4096;
    let url = url.to_string();
    let http = http.clone();
    let filter = filter.clone();
    let mut blocks = Vec::new();
    let mut stream = stream::iter(jobs.iter().copied().map(|(block, at)| {
        let http = http.clone();
        let url = url.clone();
        let filter = filter.clone();
        async move {
            let fetched = read_header(&http, &url, file_len, block, at, WINDOW).await?;
            let (payload, receipts) =
                read_receipts(&http, &url, fetched.located.receipt_at).await?;
            let root = era1::receipt_root(&receipts);
            if root != fetched.located.row.receipts_root {
                return Err(eyre!(
                    "block {block} receipt root {root} does not match header {}",
                    fetched.located.row.receipts_root
                ));
            }
            let header = fetched.located.row;
            let logs = matching_logs(&header, &receipts, &filter);
            Ok::<_, eyre::Report>(EraBlock {
                header,
                receipts_snappy: payload,
                logs,
            })
        }
    }))
    .buffer_unordered(inflight.max(1));
    while let Some(found) = stream.next().await {
        blocks.push(found?);
    }
    Ok(blocks)
}

#[allow(clippy::too_many_arguments)]
pub async fn enumerate_logs(
    http: &EraHttp,
    mirror: &str,
    network: &str,
    from: u64,
    to: u64,
    filter: &LogFilter,
    inflight: usize,
    window: Option<u64>,
) -> eyre::Result<Vec<NormalizedLog>> {
    let files = catalog::load_catalog(http, mirror, network).await?;
    let mut logs = Vec::new();
    for epoch in catalog::epoch_of(from)..=catalog::epoch_of(to) {
        let headers = read_epoch(http, mirror, &files, epoch, from, to, inflight, window).await?;
        let hits = headers
            .iter()
            .filter(|header| filter.bloom_maybe(&header.row.logs_bloom))
            .count();
        tracing::info!(
            epoch,
            blocks = headers.len(),
            hits,
            "reading receipts for bloom hits"
        );
        let file = epoch_file(&files, epoch)?;
        let url = catalog::file_url(mirror, &file.name);
        logs.extend(fetch_logs(http, &url, &headers, filter, inflight).await?);
    }
    logs.sort_by_key(|log| (log.block_number, log.tx_index, log.log_index));
    Ok(logs)
}

fn epoch_file(
    files: &std::collections::BTreeMap<u64, EraFile>,
    epoch: u64,
) -> eyre::Result<&EraFile> {
    files
        .get(&epoch)
        .ok_or_else(|| eyre!("no era1 file for epoch {epoch}; these mirrors stop at the merge"))
}

#[allow(clippy::too_many_arguments)]
async fn read_epoch(
    http: &EraHttp,
    mirror: &str,
    files: &std::collections::BTreeMap<u64, EraFile>,
    epoch: u64,
    from: u64,
    to: u64,
    inflight: usize,
    window: Option<u64>,
) -> eyre::Result<Vec<LocatedHeader>> {
    let file = epoch_file(files, epoch)?;
    let block_lo = from.max(epoch * era1::BLOCKS_PER_EPOCH);
    let block_hi = to.min((epoch + 1) * era1::BLOCKS_PER_EPOCH - 1);
    read_file(
        http, mirror, file, epoch, block_lo, block_hi, inflight, window,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn read_file(
    http: &EraHttp,
    mirror: &str,
    file: &EraFile,
    epoch: u64,
    from: u64,
    to: u64,
    inflight: usize,
    window: Option<u64>,
) -> eyre::Result<Vec<LocatedHeader>> {
    let url = catalog::file_url(mirror, &file.name);
    let file_len = match file.size {
        Some(size) => size,
        None => http.object_len(&url).await?,
    };
    let index = load_index(http, &url, file_len).await?;
    if index.starting_number != epoch * era1::BLOCKS_PER_EPOCH {
        return Err(eyre!(
            "{} starts at block {}, expected {}",
            file.name,
            index.starting_number,
            epoch * era1::BLOCKS_PER_EPOCH
        ));
    }
    if from < index.starting_number || to > index.last_block() {
        return Err(eyre!(
            "blocks {from}..={to} do not fit in {} ({}..={})",
            file.name,
            index.starting_number,
            index.last_block()
        ));
    }
    let jobs: Vec<(u64, u64)> = (from..=to)
        .map(|block| {
            let at = index
                .header_at(block)
                .expect("block was checked against the index");
            (block, at)
        })
        .collect();

    let (window, mut headers) = match window {
        Some(fixed) => (fixed, Vec::new()),
        None => {
            let sampled = fetch_headers(
                http,
                &url,
                file_len,
                &sample_jobs(&jobs),
                era1::SAMPLE_WINDOW,
                inflight,
            )
            .await?;
            let lens: Vec<u64> = sampled.iter().map(|header| header.entry_len).collect();
            let fitted = era1::fitted_window(&lens);
            tracing::info!(
                epoch,
                file = %file.name,
                blocks = jobs.len(),
                sampled = sampled.len(),
                window = fitted,
                "fitted header window"
            );
            (
                fitted,
                sampled.into_iter().map(|header| header.located).collect(),
            )
        }
    };

    let have: HashSet<u64> = headers.iter().map(|header| header.row.number).collect();
    let pending: Vec<(u64, u64)> = jobs
        .into_iter()
        .filter(|(block, _)| !have.contains(block))
        .collect();
    if !pending.is_empty() {
        tracing::info!(epoch, blocks = pending.len(), window, "reading headers");
        let rest = fetch_headers(http, &url, file_len, &pending, window, inflight).await?;
        headers.extend(rest.into_iter().map(|header| header.located));
    }
    headers.sort_by_key(|header| header.row.number);
    link_parents(&headers)?;
    Ok(headers)
}

fn sample_jobs(jobs: &[(u64, u64)]) -> Vec<(u64, u64)> {
    sample_indexes(jobs.len())
        .into_iter()
        .map(|index| jobs[index])
        .collect()
}

fn sample_indexes(len: usize) -> Vec<usize> {
    if len <= 4 {
        return (0..len).collect();
    }
    let mut indexes = vec![0, len / 3, (2 * len) / 3, len - 1];
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

async fn fetch_headers(
    http: &EraHttp,
    url: &str,
    file_len: u64,
    jobs: &[(u64, u64)],
    window: u64,
    inflight: usize,
) -> eyre::Result<Vec<Fetched>> {
    let url = url.to_string();
    let http = http.clone();
    let mut out = Vec::with_capacity(jobs.len());
    let mut stream = stream::iter(jobs.iter().copied().map(|(block, at)| {
        let http = http.clone();
        let url = url.clone();
        async move { read_header(&http, &url, file_len, block, at, window).await }
    }))
    .buffer_unordered(inflight.max(1));
    while let Some(fetched) = stream.next().await {
        out.push(fetched?);
    }
    Ok(out)
}

async fn read_header(
    http: &EraHttp,
    url: &str,
    file_len: u64,
    block: u64,
    at: u64,
    window: u64,
) -> eyre::Result<Fetched> {
    let end = at
        .saturating_add(window.saturating_sub(1))
        .min(file_len - 1);
    let mut buf = http.get_exact(url, at, end, Bucket::Header).await?;
    let (payload, entry_len, body_len) = loop {
        match era1::cut_header(&buf).wrap_err_with(|| format!("block {block} header at {at}"))? {
            HeaderCut::Ready {
                payload,
                entry_len,
                body_len,
            } => {
                break (payload.to_vec(), entry_len, body_len);
            }
            HeaderCut::Short { have, need } => {
                if need <= have || at + need > file_len {
                    return Err(eyre!(
                        "block {block} header record does not fit in the file"
                    ));
                }
                let more = http
                    .get_exact(url, at + have, at + need - 1, Bucket::Header)
                    .await
                    .wrap_err_with(|| format!("block {block} header follow-up"))?;
                buf.extend_from_slice(&more);
            }
        }
    };
    let decoded = era1::decode_header(&payload).wrap_err_with(|| format!("block {block}"))?;
    if decoded.number != block {
        return Err(eyre!(
            "header at {at} is block {}, index said {block}",
            decoded.number
        ));
    }
    let receipt_at = era1::receipt_at(at, entry_len, body_len)
        .filter(|pos| *pos + 8 <= file_len)
        .ok_or_else(|| eyre!("block {block} receipt offset is outside the file"))?;
    Ok(Fetched {
        entry_len,
        located: LocatedHeader {
            row: row_from(&decoded),
            receipt_at,
        },
    })
}

fn row_from(header: &Header) -> HeaderRow {
    HeaderRow {
        number: header.number,
        hash: header.hash_slow(),
        parent: header.parent_hash,
        timestamp: header.timestamp,
        receipts_root: header.receipts_root,
        logs_bloom: header.logs_bloom,
    }
}

fn link_parents(headers: &[LocatedHeader]) -> eyre::Result<()> {
    for pair in headers.windows(2) {
        if pair[1].row.number == pair[0].row.number + 1 && pair[1].row.parent != pair[0].row.hash {
            return Err(eyre!(
                "block {} parent does not match block {}",
                pair[1].row.number,
                pair[0].row.number
            ));
        }
    }
    Ok(())
}

async fn load_index(http: &EraHttp, url: &str, file_len: u64) -> eyre::Result<era1::BlockIndex> {
    let tail_len = era1::INDEX_TAIL.min(file_len);
    let start = file_len - tail_len;
    let tail = http
        .get_exact(url, start, file_len - 1, Bucket::Index)
        .await?;
    era1::parse_index(file_len, &tail).wrap_err("parse block index")
}

async fn fetch_logs(
    http: &EraHttp,
    url: &str,
    headers: &[LocatedHeader],
    filter: &LogFilter,
    inflight: usize,
) -> eyre::Result<Vec<NormalizedLog>> {
    let hits: Vec<&LocatedHeader> = headers
        .iter()
        .filter(|header| filter.bloom_maybe(&header.row.logs_bloom))
        .collect();
    let url = url.to_string();
    let http = http.clone();
    let filter = filter.clone();
    let mut logs = Vec::new();
    let mut stream = stream::iter(hits.into_iter().map(|header| {
        let http = http.clone();
        let url = url.clone();
        let filter = filter.clone();
        let receipt_at = header.receipt_at;
        let row = header.row.clone();
        async move {
            let (_payload, receipts) = read_receipts(&http, &url, receipt_at)
                .await
                .wrap_err_with(|| format!("block {} receipts", row.number))?;
            let root = era1::receipt_root(&receipts);
            if root != row.receipts_root {
                return Err(eyre!(
                    "block {} receipt root {root} does not match header {}",
                    row.number,
                    row.receipts_root
                ));
            }
            Ok(matching_logs(&row, &receipts, &filter))
        }
    }))
    .buffer_unordered(inflight.max(1));
    while let Some(found) = stream.next().await {
        logs.extend(found?);
    }
    Ok(logs)
}

async fn read_receipts(
    http: &EraHttp,
    url: &str,
    at: u64,
) -> eyre::Result<(Vec<u8>, Vec<alloy_consensus::ReceiptEnvelope>)> {
    let head = http.get_exact(url, at, at + 7, Bucket::Receipt).await?;
    let length = era1::receipt_payload_len(&head)?;
    if length == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let payload = http
        .get_exact(url, at + 8, at + 8 + length as u64 - 1, Bucket::Receipt)
        .await?;
    let receipts = era1::decode_receipts(&payload)?;
    Ok((payload, receipts))
}

fn matching_logs(
    header: &HeaderRow,
    receipts: &[alloy_consensus::ReceiptEnvelope],
    filter: &LogFilter,
) -> Vec<NormalizedLog> {
    let mut logs = Vec::new();
    let mut log_index = 0u64;
    for (tx_index, receipt) in receipts.iter().enumerate() {
        for log in receipt.logs() {
            let topics = log.topics().to_vec();
            if filter.matches(log.address, &topics) {
                logs.push(NormalizedLog {
                    block_number: header.number,
                    block_hash: header.hash,
                    tx_index: tx_index as u64,
                    tx_hash: None,
                    log_index,
                    address: log.address,
                    topics,
                    data: log.data.data.clone(),
                });
            }
            log_index += 1;
        }
    }
    logs
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    #[test]
    fn empty_snappy_matches_an_empty_receipt_root() {
        let mut header = HeaderRow {
            number: 1,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            parent: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            timestamp: 1,
            receipts_root: era1::receipt_root(&[]),
            logs_bloom: Bloom::default(),
        };
        let filter = LogFilter {
            addresses: vec![address!("00000000000000000000000000000000000000aa")],
            topics: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        };
        let logs = logs_from_snappy(&header, &[], &filter).unwrap();
        assert!(logs.is_empty());
        header.receipts_root = B256::ZERO;
        let err = logs_from_snappy(&header, &[], &filter).unwrap_err();
        assert!(err.to_string().contains("receipt root"));
    }
}
