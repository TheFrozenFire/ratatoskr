//! The peer directory: published DNS trees and a short candidate walk.
//!
//! The dialer refills from [`dns_candidates`]. A census walks both published
//! trees to completion. The TCP record walk is [`eth_p2p::collect_dns`].

use eyre::WrapErr;
use futures::StreamExt;
use observability::info;
use reth_dns_discovery::{DnsDiscoveryConfig, DnsDiscoveryService, DnsResolver};
use reth_network_peers::{NodeRecord, PeerId};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

/// One ENR from an EIP-1459 tree. `source` bits are [`DNS_ALL`] and [`DNS_SNAP`].
#[derive(Clone, Debug)]
pub struct DnsPeer {
    pub record: NodeRecord,
    /// Bit set of which published trees contained this node.
    pub source: u8,
}

/// `all.mainnet.ethdisco.net`. The tree Reth bootstraps from.
pub const DNS_ALL: u8 = 1;
/// `snap.mainnet.ethdisco.net`. Nodes that advertise the snap ENR entry to the crawler.
pub const DNS_SNAP: u8 = 2;

/// Walk the published mainnet trees to completion.
///
/// Reth's chainspec only bootstraps `all.mainnet`. The crawler also publishes
/// `snap.mainnet` from the same discv4 crawl. Neither tree carries a history
/// window. That arrives later, in the eth/69 Status message.
pub async fn census_mainnet_dns(network: &config::Network) -> eyre::Result<Vec<DnsPeer>> {
    let mut by_id = HashMap::<PeerId, DnsPeer>::new();
    for (link, bit) in [(&network.dns_all, DNS_ALL), (&network.dns_snap, DNS_SNAP)] {
        let peers = walk_dns_tree(link).await?;
        info!(link, peers = peers.len(), "dns tree walked");
        for record in peers {
            by_id
                .entry(record.id)
                .and_modify(|peer| peer.source |= bit)
                .or_insert(DnsPeer {
                    record,
                    source: bit,
                });
        }
    }
    let mut found: Vec<_> = by_id.into_values().collect();
    found.sort_by_key(|peer| peer.record.id);
    Ok(found)
}

async fn walk_dns_tree(link: &str) -> eyre::Result<Vec<NodeRecord>> {
    let resolver = DnsResolver::from_system_conf().wrap_err("system DNS resolver")?;
    let config = DnsDiscoveryConfig {
        max_requests_per_sec: NonZeroUsize::new(30).unwrap(),
        ..DnsDiscoveryConfig::default()
    };
    let (mut service, _handle) = DnsDiscoveryService::new_pair(Arc::new(resolver), config);
    let mut updates = service.node_record_stream();
    service.sync_tree(link).wrap_err("dns discovery link")?;
    let task = service.spawn();

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut idle = tokio::time::interval(Duration::from_secs(15));
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    idle.tick().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            update = updates.next() => {
                let Some(update) = update else { break };
                let node = update.node_record;
                if node.tcp_port == 0 || !seen.insert(node.id) {
                    continue;
                }
                found.push(node);
                idle.reset();
            }
            _ = idle.tick() => {
                if !found.is_empty() {
                    break;
                }
            }
        }
    }
    task.abort();
    Ok(found)
}

/// A short DNS walk for the peer service.
pub async fn dns_candidates(
    limit: usize,
    network: &config::Network,
) -> eyre::Result<Vec<NodeRecord>> {
    // A few hundred records takes longer than a short walk. Cap the
    // wait so a stalled resolver cannot sit on the dial loop.
    let cap = network.dns_budget_secs;
    let budget = Duration::from_secs(u64::try_from(12 + limit / 10).unwrap_or(cap).min(cap));
    let mut found = eth_p2p::collect_dns(limit, budget).await?;
    use rand::seq::SliceRandom;
    found.shuffle(&mut rand::thread_rng());
    if found.is_empty() {
        return Err(eyre::eyre!("dns discovery returned no TCP peers"));
    }
    Ok(found)
}
