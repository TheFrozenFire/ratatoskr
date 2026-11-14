//! Read proxy configuration, then the aggregator's answer bounds.
//!
//! A reverted proxy batch stops the pass. A reverted bounds batch stores the
//! phases without bounds. One subcall failure is that feed's failure.

use crate::abi::{
    bound_calls, decode_aggregate3, decode_bound, decode_proxy_head, encode_aggregate3,
    proxy_calls, CallResult, ProxyHead, CALL_GAS, FEEDS_PER_CALL, MULTICALL,
};
use alloy_primitives::{Address, I256};
use store_model::Read;
use venues::Rpc;

pub fn configure(feeds: &store::service::Feeds, rpc: &Rpc) -> eyre::Result<()> {
    let proxies = feeds.proxies()?;
    tracing::info!(feeds = proxies.len(), "feed configuration");
    for chunk in proxies.chunks(FEEDS_PER_CALL) {
        let owned: Vec<(Address, [u8; 4])> = chunk.iter().copied().flat_map(proxy_calls).collect();
        let calls: Vec<(Address, &[u8])> = owned
            .iter()
            .map(|(target, data)| (*target, data.as_slice()))
            .collect();
        let data = encode_aggregate3(&calls);
        let response = rpc.eth_call(&data, Some(MULTICALL), CALL_GAS)?;
        if response.reverted {
            return Err(eyre::eyre!("proxy batch reverted: {}", response.message));
        }
        let results = decode_aggregate3(&response.bytes)?;
        let heads = proxy_heads(chunk, &results)?;
        let ready: Vec<(Address, ProxyHead)> = heads
            .iter()
            .filter_map(|(proxy, head)| head.as_ref().map(|head| (*proxy, head.clone())))
            .collect();
        let bounds = match bound_answers(rpc, &ready) {
            Ok(bounds) => bounds,
            Err(err) => {
                tracing::warn!(%err, "bounds batch");
                vec![(None, None); ready.len()]
            }
        };
        let mut ready_at = 0usize;
        for (proxy, head) in heads {
            match head {
                Some(head) => {
                    let (min_answer, max_answer) =
                        bounds.get(ready_at).copied().unwrap_or((None, None));
                    ready_at += 1;
                    feeds.apply_read(
                        proxy,
                        Read::Configured {
                            head,
                            min_answer,
                            max_answer,
                        },
                    )?;
                }
                None => {
                    let message = proxy_failure(chunk, &results, proxy);
                    feeds.apply_read(proxy, Read::Failed(message))?;
                }
            }
        }
    }
    Ok(())
}

/// One head per proxy, in proxy order. `None` is a subcall failure.
pub fn proxy_heads(
    proxies: &[Address],
    results: &[CallResult],
) -> eyre::Result<Vec<(Address, Option<ProxyHead>)>> {
    if results.len() != proxies.len() * 5 {
        return Err(eyre::eyre!("proxy batch length"));
    }
    let mut out = Vec::with_capacity(proxies.len());
    for (index, proxy) in proxies.iter().enumerate() {
        let slice = &results[index * 5..index * 5 + 5];
        let head = decode_proxy_head(slice).ok();
        out.push((*proxy, head));
    }
    Ok(out)
}

fn proxy_failure(proxies: &[Address], results: &[CallResult], proxy: Address) -> String {
    let Some(index) = proxies.iter().position(|item| *item == proxy) else {
        return "proxy read is short".into();
    };
    let slice = &results[index * 5..index * 5 + 5];
    decode_proxy_head(slice)
        .err()
        .unwrap_or_else(|| "proxy read is short".into())
}

fn bound_answers(
    rpc: &Rpc,
    ready: &[(Address, ProxyHead)],
) -> eyre::Result<Vec<(Option<I256>, Option<I256>)>> {
    if ready.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(ready.len());
    for chunk in ready.chunks(FEEDS_PER_CALL) {
        let owned: Vec<(Address, [u8; 4])> = chunk
            .iter()
            .flat_map(|(_, head)| bound_calls(head.aggregator))
            .collect();
        let calls: Vec<(Address, &[u8])> = owned
            .iter()
            .map(|(target, data)| (*target, data.as_slice()))
            .collect();
        let data = encode_aggregate3(&calls);
        let response = rpc.eth_call(&data, Some(MULTICALL), CALL_GAS)?;
        if response.reverted {
            return Err(eyre::eyre!("bounds batch reverted: {}", response.message));
        }
        let results = decode_aggregate3(&response.bytes)?;
        if results.len() != chunk.len() * 2 {
            return Err(eyre::eyre!("bounds batch length"));
        }
        for index in 0..chunk.len() {
            out.push((
                decode_bound(&results[index * 2]),
                decode_bound(&results[index * 2 + 1]),
            ));
        }
    }
    Ok(out)
}
