//! Read proxy configuration, then the aggregator's answer bounds.
//!
//! A reverted proxy batch stops the pass. A reverted bounds batch stores the
//! phases without bounds. One subcall failure is that feed's failure.

use crate::abi::{
    bound_calls, decode_aggregate3, decode_bound, decode_proxy_head, encode_aggregate3,
    proxy_calls, CallResult, ProxyHead, CALL_GAS, FEEDS_PER_CALL, MULTICALL,
};
use alloy_primitives::{Address, I256};
use chain::Rpc;
use store_model::{Configuration, Mismatch, Phase, PhasePlan, Read};

pub fn configure(feeds: &store::service::Feeds, rpc: &Rpc) -> eyre::Result<()> {
    let proxies = feeds.proxies()?;
    observability::info!(feeds = proxies.len(), "feed configuration");
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
                observability::warn!(%err, "bounds batch");
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
                    record_read(
                        feeds,
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
                    record_read(feeds, proxy, Read::Failed(message))?;
                }
            }
        }
    }
    Ok(())
}

/// Compare one proxy read with the stored configuration and phases, then write that plan.
pub fn record_read(
    feeds: &store::service::Feeds,
    proxy: Address,
    read: Read,
) -> Result<(), store::FeedError> {
    let stored = feeds.configuration(proxy)?;
    let phases = feeds.phases(proxy)?;
    let plan = phase_plan(stored.as_ref(), &phases, read);
    feeds.apply_phase_plan(proxy, &plan)
}

/// The stored configuration stays. A greater phase id appends one phase and one history job.
/// A smaller phase id, or the same phase id with another aggregator, leaves the stored phases.
pub fn phase_plan(stored: Option<&Configuration>, phases: &[Phase], read: Read) -> PhasePlan {
    let (head, min_answer, max_answer) = match read {
        Read::Failed(text) => return PhasePlan::Failure(text),
        Read::Configured {
            head,
            min_answer,
            max_answer,
        } => (head, min_answer, max_answer),
    };
    let mut mismatches = Vec::new();
    let configuration = match stored {
        None => Some(Configuration {
            decimals: head.decimals,
            description: head.description.clone(),
            version: head.version,
        }),
        Some(stored)
            if stored.decimals != head.decimals
                || stored.description != head.description
                || stored.version != head.version =>
        {
            mismatches.push(Mismatch::Configuration {
                stored_decimals: stored.decimals,
                observed_decimals: head.decimals,
                stored_description: stored.description.clone(),
                observed_description: head.description.clone(),
                stored_version: stored.version,
                observed_version: head.version,
            });
            None
        }
        Some(_) => None,
    };
    let newest = phases.iter().max_by_key(|phase| phase.phase_id);
    let phase = match newest {
        Some(phase) if phase.phase_id == head.phase_id && phase.aggregator == head.aggregator => {
            None
        }
        Some(phase) if phase.phase_id == head.phase_id => {
            mismatches.push(Mismatch::Aggregator {
                phase_id: head.phase_id,
                stored: phase.aggregator,
                observed: head.aggregator,
            });
            None
        }
        Some(phase) if head.phase_id > phase.phase_id => Some(Phase {
            phase_id: head.phase_id,
            aggregator: head.aggregator,
            min_answer,
            max_answer,
        }),
        Some(phase) => {
            mismatches.push(Mismatch::Phase {
                newest: phase.phase_id,
                observed: head.phase_id,
            });
            None
        }
        None => Some(Phase {
            phase_id: head.phase_id,
            aggregator: head.aggregator,
            min_answer,
            max_answer,
        }),
    };
    PhasePlan::Configured {
        configuration,
        mismatches,
        phase,
    }
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
