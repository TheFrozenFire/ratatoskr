//! Find mainnet peers that accept TCP.
//!
//! Two sources, run together:
//! - discv4 against the mainnet bootnodes. Those records have no TCP port.
//!   They only answer `FindNode` after they can ping the UDP endpoint we
//!   advertise, so this source is empty when that ping cannot get back.
//! - EIP-1459 DNS (`all.mainnet.ethdisco.net`). That tree is a list of ENRs
//!   and does not require us to be reachable.

use eyre::WrapErr;
use futures::StreamExt;
use reth_chainspec::MAINNET;
use reth_discv4::{Discv4, Discv4Config};
use reth_dns_discovery::{DnsDiscoveryConfig, DnsDiscoveryService, DnsResolver};
use reth_net_nat::NatResolver;
use reth_network_peers::{mainnet_nodes, pk2id, NodeRecord};
use secp256k1::{SecretKey, SECP256K1};
use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout};
use tracing::info;

const DNS_BUDGET: Duration = Duration::from_secs(75);
const DISCV4_BUDGET: Duration = Duration::from_secs(12);
const WANTED_PEERS: usize = 600;

/// Peers with a TCP port, distinct by IP.
///
/// DNS is enough on its own. discv4 runs beside it and is dropped once DNS has
/// filled the list, because a lookup that nobody can ping back just waits out
/// its budget.
pub async fn find_mainnet_peers() -> eyre::Result<Vec<NodeRecord>> {
    let dns = dns_peers();
    tokio::pin!(dns);
    let udp = discv4_peers();
    tokio::pin!(udp);
    let mut dns_result = None;
    let mut udp_result = None;
    loop {
        tokio::select! {
            result = &mut dns, if dns_result.is_none() => dns_result = Some(result),
            result = &mut udp, if udp_result.is_none() => udp_result = Some(result),
        }
        let dns_ready = dns_result
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .is_some_and(|peers| peers.len() >= WANTED_PEERS);
        if dns_ready || (dns_result.is_some() && udp_result.is_some()) {
            break;
        }
    }

    let mut found = Vec::new();
    let mut seen_ips = HashSet::new();
    for (name, result) in [("dns", dns_result), ("discv4", udp_result)] {
        match result {
            Some(Ok(peers)) => {
                info!(source = name, peers = peers.len(), "peer source finished");
                for peer in peers {
                    if peer.tcp_port != 0 && seen_ips.insert(peer.address) {
                        found.push(peer);
                    }
                }
            }
            Some(Err(err)) => info!(source = name, %err, "peer source failed"),
            None => info!(source = name, "peer source cancelled"),
        }
    }
    found.truncate(WANTED_PEERS);
    use rand::seq::SliceRandom;
    found.shuffle(&mut rand::thread_rng());
    if found.is_empty() {
        return Err(eyre::eyre!(
            "no mainnet peer with a TCP port. discv4 needs a reachable UDP port, and the DNS tree returned nothing."
        ));
    }
    Ok(found)
}

async fn discv4_peers() -> eyre::Result<Vec<NodeRecord>> {
    // Bootnodes are IPv4. `external_ip()` often returns the host's IPv6 address,
    // which those bootnodes will not ping. Ask for an IPv4 address explicitly.
    let Some(external) = public_ipv4().await else {
        return Err(eyre::eyre!("could not resolve a public IPv4 address"));
    };
    let secret = SecretKey::new(&mut rand::thread_rng());
    let id = pk2id(&secret.public_key(SECP256K1));
    let bind = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    // `Discv4::spawn` replaces `udp_port` with the bound socket port. The
    // address stays as the public IP so a bootnode's return ping has a target.
    let local = NodeRecord {
        address: external,
        tcp_port: 0,
        udp_port: 0,
        id,
    };
    let config = Discv4Config::builder()
        .add_boot_nodes(mainnet_nodes())
        .enable_dht_random_walk(false)
        .enable_lookup(false)
        .external_ip_resolver(Some(NatResolver::ExternalIp(external)))
        .resolve_external_ip_interval(Some(Duration::from_secs(300)))
        .build();
    let discv4 = Discv4::spawn(bind, local, secret, config)
        .await
        .wrap_err("bind discovery socket")?;
    info!(%external, udp = ?discv4.local_addr(), "discv4 bound");

    let mut found = Vec::new();
    let mut seen_ips = HashSet::new();
    let started = tokio::time::Instant::now();
    while started.elapsed() < DISCV4_BUDGET && found.len() < WANTED_PEERS {
        let remaining = DISCV4_BUDGET.saturating_sub(started.elapsed());
        let batch = match timeout(remaining, discv4.lookup_random()).await {
            Ok(Ok(nodes)) => nodes,
            Ok(Err(err)) => return Err(err).wrap_err("discv4 lookup"),
            Err(_) => break,
        };
        let dialable = batch.iter().filter(|node| node.tcp_port != 0).count();
        info!(returned = batch.len(), dialable, "discv4 lookup batch");
        for node in batch {
            if node.tcp_port == 0 || !seen_ips.insert(node.address) {
                continue;
            }
            found.push(node);
            if found.len() == WANTED_PEERS {
                break;
            }
        }
        if found.len() < WANTED_PEERS {
            sleep(Duration::from_millis(400)).await;
        }
    }
    Ok(found)
}

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

const ALL_LINK: &str =
    "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@all.mainnet.ethdisco.net";
const SNAP_LINK: &str =
    "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@snap.mainnet.ethdisco.net";

/// Walk the published mainnet trees to completion.
///
/// Reth's chainspec only bootstraps `all.mainnet`. The crawler also publishes
/// `snap.mainnet` from the same discv4 crawl. Neither tree carries a history
/// window. That arrives later, in the eth/69 Status message.
pub async fn census_mainnet_dns() -> eyre::Result<Vec<DnsPeer>> {
    let mut by_id = std::collections::HashMap::<reth_network_peers::PeerId, DnsPeer>::new();
    for (link, bit) in [(ALL_LINK, DNS_ALL), (SNAP_LINK, DNS_SNAP)] {
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
pub async fn dns_candidates(limit: usize) -> eyre::Result<Vec<NodeRecord>> {
    // A few hundred records takes longer than a short walk. Cap the
    // wait so a stalled resolver cannot sit on the dial loop.
    let budget = Duration::from_secs(u64::try_from(12 + limit / 10).unwrap_or(75).min(75));
    let mut found = collect_dns(limit, budget).await?;
    use rand::seq::SliceRandom;
    found.shuffle(&mut rand::thread_rng());
    if found.is_empty() {
        return Err(eyre::eyre!("dns discovery returned no TCP peers"));
    }
    Ok(found)
}

async fn dns_peers() -> eyre::Result<Vec<NodeRecord>> {
    collect_dns(WANTED_PEERS, DNS_BUDGET).await
}

async fn collect_dns(limit: usize, budget: Duration) -> eyre::Result<Vec<NodeRecord>> {
    let link = MAINNET
        .chain
        .public_dns_network_protocol()
        .ok_or_else(|| eyre::eyre!("mainnet chainspec has no DNS discovery link"))?;
    let resolver = DnsResolver::from_system_conf().wrap_err("system DNS resolver")?;
    let config = DnsDiscoveryConfig {
        max_requests_per_sec: NonZeroUsize::new(20).unwrap(),
        ..DnsDiscoveryConfig::default()
    };
    let (mut service, _handle) = DnsDiscoveryService::new_pair(Arc::new(resolver), config);
    let mut updates = service.node_record_stream();
    service
        .sync_tree(&link)
        .wrap_err("mainnet DNS discovery link")?;
    let task = service.spawn();

    let mut found = Vec::new();
    let mut seen_ips = HashSet::new();
    let collect = async {
        while let Some(update) = updates.next().await {
            let node = update.node_record;
            if node.tcp_port == 0 || !seen_ips.insert(node.address) {
                continue;
            }
            found.push(node);
            if found.len() == limit {
                break;
            }
        }
    };
    timeout(budget, collect).await.ok();
    task.abort();
    info!(peers = found.len(), "dns discovery collected");
    Ok(found)
}

async fn public_ipv4() -> Option<std::net::IpAddr> {
    let output = std::process::Command::new("curl")
        .args([
            "-4",
            "-fsS",
            "--max-time",
            "5",
            "https://ipv4.icanhazip.com",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.trim().parse().ok()
}
