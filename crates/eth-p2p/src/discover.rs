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
use observability::info;
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

/// Peers with a TCP port, distinct by IP.
///
/// DNS is enough on its own. discv4 runs beside it and is dropped once DNS has
/// filled the list, because a lookup that nobody can ping back just waits out
/// its budget.
pub async fn find_mainnet_peers(network: &config::Network) -> eyre::Result<Vec<NodeRecord>> {
    let dns = dns_peers(network);
    tokio::pin!(dns);
    let udp = discv4_peers(network);
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
            .is_some_and(|peers| peers.len() >= network.wanted_peers);
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
    found.truncate(network.wanted_peers);
    use rand::seq::SliceRandom;
    found.shuffle(&mut rand::thread_rng());
    if found.is_empty() {
        return Err(eyre::eyre!(
            "no mainnet peer with a TCP port. discv4 needs a reachable UDP port, and the DNS tree returned nothing."
        ));
    }
    Ok(found)
}

async fn discv4_peers(network: &config::Network) -> eyre::Result<Vec<NodeRecord>> {
    // Bootnodes are IPv4. `external_ip()` often returns the host's IPv6 address,
    // which those bootnodes will not ping. Ask for an IPv4 address explicitly.
    let Some(external) = public_ipv4(&network.egress_ip_url).await else {
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
    while started.elapsed() < network.discv4_budget() && found.len() < network.wanted_peers {
        let remaining = network.discv4_budget().saturating_sub(started.elapsed());
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
            if found.len() == network.wanted_peers {
                break;
            }
        }
        if found.len() < network.wanted_peers {
            sleep(Duration::from_millis(400)).await;
        }
    }
    Ok(found)
}

async fn dns_peers(network: &config::Network) -> eyre::Result<Vec<NodeRecord>> {
    collect_dns(network.wanted_peers, network.dns_budget()).await
}

/// DNS records with a TCP port, distinct by IP, until `limit` or `budget`.
///
/// [`find_mainnet_peers`] and the peer directory share this walk.
pub async fn collect_dns(limit: usize, budget: Duration) -> eyre::Result<Vec<NodeRecord>> {
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

async fn public_ipv4(url: &str) -> Option<std::net::IpAddr> {
    let output = std::process::Command::new("curl")
        .args(["-4", "-fsS", "--max-time", "5", url])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.trim().parse().ok()
}
