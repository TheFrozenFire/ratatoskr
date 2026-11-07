//! Record the published mainnet DNS peer lists.
//!
//! This does not dial. Block ranges are not in the ENR or the DNS tree. They
//! show up only after an eth/69 Status handshake, and the header sync owns
//! those sockets for now.

use clap::Parser;
use directory::{default_path, Catalog, PublishedPeer};
use eth_p2p::{census_mainnet_dns, DNS_ALL, DNS_SNAP};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-peers")]
struct Args {
    /// Peer catalog. Created if missing.
    #[arg(long)]
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "peers=info,eth_p2p=info".into()),
        )
        .init();
    let args = Args::parse();
    let path = args.db.unwrap_or_else(default_path);
    let peers = census_mainnet_dns().await?;
    let all = peers.iter().filter(|peer| peer.source & DNS_ALL != 0).count();
    let snap = peers.iter().filter(|peer| peer.source & DNS_SNAP != 0).count();
    let both = peers
        .iter()
        .filter(|peer| peer.source & (DNS_ALL | DNS_SNAP) == DNS_ALL | DNS_SNAP)
        .count();
    let snap_only = peers
        .iter()
        .filter(|peer| peer.source & DNS_SNAP != 0 && peer.source & DNS_ALL == 0)
        .count();
    let published: Vec<PublishedPeer> = peers
        .iter()
        .map(|peer| PublishedPeer {
            id: peer.record.id,
            address: peer.record.address,
            tcp_port: peer.record.tcp_port,
            udp_port: peer.record.udp_port,
            source: peer.source,
        })
        .collect();
    let catalog = Catalog::open(&path)?;
    catalog.write_published(&published)?;
    println!(
        "peers {} all-tree {} snap-tree {} in-both {} snap-only {} catalog {}",
        peers.len(),
        all,
        snap,
        both,
        snap_only,
        path.display()
    );
    Ok(())
}
