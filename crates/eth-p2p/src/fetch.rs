//! Header and receipt requests on an already open session.

use crate::session::OutboundSession;
use alloy_consensus::{constants::EMPTY_ROOT_HASH, proofs::calculate_receipt_root, TxReceipt};
use alloy_primitives::B256;
use corpus::{encode_receipt_bundle, flatten_logs};
use eyre::eyre;
use model::{encode_block_record, BlockLogs, BlockRecord, HeaderRecord};
use reth_eth_wire::EthMessage;
use reth_eth_wire_types::{
    message::RequestPair, BlockHashOrNumber, EthVersion, GetBlockHeaders, GetReceipts,
    GetReceipts70, HeadersDirection,
};
use reth_ethereum_primitives::Receipt;

/// Fetch every log in `block_number` from the connected peer.
pub async fn fetch_block_logs(
    session: &OutboundSession,
    block_number: u64,
) -> eyre::Result<BlockLogs> {
    let header = fetch_header(session, block_number).await?;
    if header.number != block_number {
        return Err(eyre!(
            "peer returned header {} for request {block_number}",
            header.number
        ));
    }
    let hash = header.hash_slow();
    let receipts = fetch_receipts(session, hash).await?;
    Ok(BlockLogs {
        number: block_number,
        hash,
        logs: flatten_logs(block_number, hash, &receipts),
    })
}

async fn fetch_header(
    session: &OutboundSession,
    block_number: u64,
) -> eyre::Result<alloy_consensus::Header> {
    let request_id = session.alloc_request_id();
    let response = session
        .request(EthMessage::GetBlockHeaders(RequestPair {
            request_id,
            message: GetBlockHeaders {
                start_block: BlockHashOrNumber::Number(block_number),
                limit: 1,
                skip: 0,
                direction: HeadersDirection::Rising,
            },
        }))
        .await?;
    let EthMessage::BlockHeaders(pair) = response else {
        return Err(eyre!("peer answered headers with a different message"));
    };
    pair.message
        .0
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("peer has no header for block {block_number}"))
}

async fn fetch_receipts(session: &OutboundSession, hash: B256) -> eyre::Result<Vec<Receipt>> {
    // eth/71 and eth/72 keep the eth/70 receipt encoding.
    if session.version() >= EthVersion::Eth70 {
        fetch_receipts70(session, hash).await
    } else if session.version() == EthVersion::Eth69 {
        fetch_receipts69(session, hash).await
    } else {
        fetch_receipts68(session, hash).await
    }
}

async fn fetch_receipts68(session: &OutboundSession, hash: B256) -> eyre::Result<Vec<Receipt>> {
    let pair = request_receipts(session, hash).await?;
    let EthMessage::Receipts(pair) = pair else {
        return Err(eyre!(
            "peer answered eth/68 receipts with a different message"
        ));
    };
    let block = pair
        .message
        .0
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("peer returned no receipts for {hash}"))?;
    Ok(block.into_iter().map(|wrapped| wrapped.receipt).collect())
}

async fn fetch_receipts69(session: &OutboundSession, hash: B256) -> eyre::Result<Vec<Receipt>> {
    let pair = request_receipts(session, hash).await?;
    let EthMessage::Receipts69(pair) = pair else {
        return Err(eyre!(
            "peer answered eth/69 receipts with a different message"
        ));
    };
    pair.message
        .0
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("peer returned no receipts for {hash}"))
}

async fn request_receipts(session: &OutboundSession, hash: B256) -> eyre::Result<EthMessage> {
    let request_id = session.alloc_request_id();
    session
        .request(EthMessage::GetReceipts(RequestPair {
            request_id,
            message: GetReceipts(vec![hash]),
        }))
        .await
}

async fn fetch_receipts70(session: &OutboundSession, hash: B256) -> eyre::Result<Vec<Receipt>> {
    let mut collected = Vec::new();
    let mut offset = 0_u64;
    loop {
        let request_id = session.alloc_request_id();
        let response = session
            .request(EthMessage::GetReceipts70(RequestPair {
                request_id,
                message: GetReceipts70 {
                    first_block_receipt_index: offset,
                    block_hashes: vec![hash],
                },
            }))
            .await?;
        let EthMessage::Receipts70(pair) = response else {
            return Err(eyre!(
                "peer answered eth/70 receipts with a different message"
            ));
        };
        let page = pair.message.receipts.into_iter().next().unwrap_or_default();
        let page_len = page.len() as u64;
        if page_len == 0 && pair.message.last_block_incomplete {
            return Err(eyre!("eth/70 peer made no progress on {hash}"));
        }
        collected.extend(page);
        if !pair.message.last_block_incomplete {
            return Ok(collected);
        }
        offset = offset.saturating_add(page_len);
    }
}

/// Headers plus receipts for `count` consecutive blocks starting at `start`.
///
/// A short prefix is returned when the peer has only the start of the range.
/// Each block's receipt root is checked against the header. An empty receipt
/// list is kept only when the block has no transactions.
pub async fn fetch_block_range(
    session: &OutboundSession,
    start: u64,
    count: u64,
) -> eyre::Result<Vec<BlockRecord>> {
    if count == 0 || count > 64 {
        return Err(eyre!("block range must be 1 to 64, got {count}"));
    }
    let headers = fetch_headers(session, start, count).await?;
    if headers.is_empty() {
        return Err(eyre!("peer has no header for block {start}"));
    }
    let mut previous_hash = None;
    for header in &headers {
        if let Some(parent) = previous_hash {
            if header.parent_hash != parent {
                return Err(eyre!(
                    "header {} does not follow the previous hash",
                    header.number
                ));
            }
        }
        previous_hash = Some(header.hash_slow());
    }
    let hashes: Vec<B256> = headers.iter().map(|header| header.hash_slow()).collect();
    let receipt_lists = fetch_receipt_lists(session, &hashes).await?;
    if receipt_lists.len() != headers.len() {
        return Err(eyre!(
            "peer returned {} receipt lists for {} headers",
            receipt_lists.len(),
            headers.len()
        ));
    }
    let mut records = Vec::with_capacity(headers.len());
    for (header, receipts) in headers.iter().zip(receipt_lists) {
        receipts_match_header(header, &receipts)?;
        let bundle = encode_receipt_bundle(&receipts);
        let compressed = zstd::bulk::compress(&bundle, 3).map_err(|err| eyre!("zstd: {err}"))?;
        records.push(BlockRecord {
            number: header.number,
            hash: header.hash_slow(),
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
            receipts_root: header.receipts_root,
            transactions_root: header.transactions_root,
            receipts_zstd: compressed,
        });
    }
    Ok(records)
}

/// Concatenate the encoded records from [`fetch_block_range`].
pub fn encode_block_records(records: &[BlockRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&encode_block_record(record));
    }
    out
}

async fn fetch_headers(
    session: &OutboundSession,
    start: u64,
    count: u64,
) -> eyre::Result<Vec<alloy_consensus::Header>> {
    let request_id = session.alloc_request_id();
    let response = session
        .request(EthMessage::GetBlockHeaders(RequestPair {
            request_id,
            message: GetBlockHeaders {
                start_block: BlockHashOrNumber::Number(start),
                limit: count,
                skip: 0,
                direction: HeadersDirection::Rising,
            },
        }))
        .await?;
    let EthMessage::BlockHeaders(pair) = response else {
        return Err(eyre!("peer answered headers with a different message"));
    };
    let mut headers = pair.message.0;
    if headers.first().is_some_and(|header| header.number != start) {
        return Err(eyre!(
            "peer headers start at {} instead of {start}",
            headers.first().map(|header| header.number).unwrap_or(0)
        ));
    }
    let mut expected = start;
    headers.retain(|header| {
        let keep = header.number == expected;
        if keep {
            expected = expected.saturating_add(1);
        }
        keep
    });
    Ok(headers)
}

/// Headers from `start`, rising, at most `count`.
///
/// Each header's hash is checked locally. Parent hashes have to link inside
/// the batch. A short reply is the prefix the peer actually returned.
pub async fn fetch_header_range(
    session: &OutboundSession,
    start: u64,
    count: u64,
) -> eyre::Result<Vec<HeaderRecord>> {
    let headers = fetch_headers(session, start, count).await?;
    if headers.is_empty() {
        return Err(eyre!("peer has no header for block {start}"));
    }
    let mut records = Vec::with_capacity(headers.len());
    let mut previous: Option<B256> = None;
    for header in headers {
        let hash = header.hash_slow();
        if let Some(parent) = previous {
            if header.parent_hash != parent {
                return Err(eyre!(
                    "header {} does not link to the previous header",
                    header.number
                ));
            }
        }
        previous = Some(hash);
        let mut logs_bloom = [0u8; 256];
        logs_bloom.copy_from_slice(header.logs_bloom.as_slice());
        records.push(HeaderRecord {
            number: header.number,
            hash,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
            receipts_root: header.receipts_root,
            logs_bloom,
        });
    }
    Ok(records)
}

async fn fetch_receipt_lists(
    session: &OutboundSession,
    hashes: &[B256],
) -> eyre::Result<Vec<Vec<Receipt>>> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    if session.version() >= EthVersion::Eth70 {
        fetch_receipt_lists70(session, hashes).await
    } else if session.version() == EthVersion::Eth69 {
        let lists = match fetch_receipt_lists69(session, hashes).await {
            Ok(lists) if lists.len() == hashes.len() => lists,
            _ => return fetch_receipt_lists_one_by_one(session, hashes).await,
        };
        Ok(lists)
    } else {
        let lists = match fetch_receipt_lists68(session, hashes).await {
            Ok(lists) if lists.len() == hashes.len() => lists,
            _ => return fetch_receipt_lists_one_by_one(session, hashes).await,
        };
        Ok(lists)
    }
}

async fn fetch_receipt_lists_one_by_one(
    session: &OutboundSession,
    hashes: &[B256],
) -> eyre::Result<Vec<Vec<Receipt>>> {
    let mut lists = Vec::with_capacity(hashes.len());
    for hash in hashes {
        lists.push(fetch_receipts(session, *hash).await?);
    }
    Ok(lists)
}

async fn fetch_receipt_lists68(
    session: &OutboundSession,
    hashes: &[B256],
) -> eyre::Result<Vec<Vec<Receipt>>> {
    let request_id = session.alloc_request_id();
    let response = session
        .request(EthMessage::GetReceipts(RequestPair {
            request_id,
            message: GetReceipts(hashes.to_vec()),
        }))
        .await?;
    let EthMessage::Receipts(pair) = response else {
        return Err(eyre!(
            "peer answered eth/68 receipts with a different message"
        ));
    };
    Ok(pair
        .message
        .0
        .into_iter()
        .map(|block| block.into_iter().map(|wrapped| wrapped.receipt).collect())
        .collect())
}

async fn fetch_receipt_lists69(
    session: &OutboundSession,
    hashes: &[B256],
) -> eyre::Result<Vec<Vec<Receipt>>> {
    let request_id = session.alloc_request_id();
    let response = session
        .request(EthMessage::GetReceipts(RequestPair {
            request_id,
            message: GetReceipts(hashes.to_vec()),
        }))
        .await?;
    let EthMessage::Receipts69(pair) = response else {
        return Err(eyre!(
            "peer answered eth/69 receipts with a different message"
        ));
    };
    Ok(pair.message.0)
}

async fn fetch_receipt_lists70(
    session: &OutboundSession,
    hashes: &[B256],
) -> eyre::Result<Vec<Vec<Receipt>>> {
    let mut collected = Vec::with_capacity(hashes.len());
    let mut index = 0;
    while index < hashes.len() {
        let request_id = session.alloc_request_id();
        let response = session
            .request(EthMessage::GetReceipts70(RequestPair {
                request_id,
                message: GetReceipts70 {
                    first_block_receipt_index: 0,
                    block_hashes: hashes[index..].to_vec(),
                },
            }))
            .await?;
        let EthMessage::Receipts70(pair) = response else {
            return Err(eyre!(
                "peer answered eth/70 receipts with a different message"
            ));
        };
        let mut groups = pair.message.receipts;
        if pair.message.last_block_incomplete {
            groups.pop();
        }
        if groups.is_empty() {
            collected.push(fetch_receipts70(session, hashes[index]).await?);
            index += 1;
            continue;
        }
        index += groups.len();
        collected.extend(groups);
    }
    Ok(collected)
}

fn receipts_match_header(
    header: &alloy_consensus::Header,
    receipts: &[Receipt],
) -> eyre::Result<()> {
    if header.transactions_root == EMPTY_ROOT_HASH {
        if receipts.is_empty() && header.receipts_root == EMPTY_ROOT_HASH {
            return Ok(());
        }
        return Err(eyre!(
            "block {} is empty but the peer sent receipts",
            header.number
        ));
    }
    if receipts.is_empty() {
        return Err(eyre!(
            "peer returned no receipts for block {} which has transactions",
            header.number
        ));
    }
    let with_bloom: Vec<_> = receipts
        .iter()
        .cloned()
        .map(|receipt| receipt.into_with_bloom())
        .collect();
    let root = calculate_receipt_root(&with_bloom);
    if root != header.receipts_root {
        return Err(eyre!("receipt root mismatch for block {}", header.number));
    }
    Ok(())
}
