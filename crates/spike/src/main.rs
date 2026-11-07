//! Dial one mainnet peer, read one block of logs, print discovery hits, exit.

use clap::Parser;
use eyre::WrapErr;
use futures::{stream, StreamExt};
use discover::classify;
use eth_p2p::{fetch_block_logs, find_mainnet_peers, OutboundSession};
use model::ETHEREUM_MAINNET;
use reth_network_peers::NodeRecord;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-spike")]
struct Args {
    /// Mainnet execution-layer peer, `enode://<id>@<ip>:<port>`.
    /// When omitted, discv4 asks the mainnet bootnodes for neighbours.
    #[arg(long)]
    enode: Option<String>,
    /// First block to read. When omitted, uses the peer's head minus 32.
    #[arg(long)]
    block: Option<u64>,
    /// How many consecutive blocks to read on this same socket.
    #[arg(long, default_value_t = 1)]
    count: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "spike=info,eth_p2p=info".into()),
        )
        .init();

    if let Err(err) = run().await {
        eprintln!("spike failed: {err:?}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn run() -> eyre::Result<()> {
    let args = Args::parse();
    if args.count == 0 {
        return Err(eyre::eyre!("--count must be at least 1"));
    }
    let peers = dial_targets(args.enode.as_deref()).await?;
    let tried = peers.len();
    let block = args.block;
    let count = args.count;
    let mut in_flight = stream::iter(peers)
        .map(move |peer| try_peer(peer, block, count))
        .buffer_unordered(16);
    let mut full = 0_u32;
    let mut other = Vec::new();
    while let Some(result) = in_flight.next().await {
        match result {
            Ok(()) => return Ok(()),
            Err(Attempt::Full) => full = full.saturating_add(1),
            Err(Attempt::Other(text)) => other.push(text),
        }
    }
    Err(eyre::eyre!(
        "no peer served a block. tried {tried}, {full} were full.\n{}",
        other.join("\n")
    ))
}

enum Attempt {
    Full,
    Other(String),
}

async fn try_peer(peer: NodeRecord, block: Option<u64>, count: u64) -> Result<(), Attempt> {
    let label = peer.to_string();
    let mut session = match OutboundSession::connect_record(peer).await {
        Ok(session) => session,
        Err(err) => {
            let text = format!("{err:#}");
            if text.contains("too many peers") {
                return Err(Attempt::Full);
            }
            return Err(Attempt::Other(format!("{label}: {text}")));
        }
    };
    let start = match block {
        Some(block) => block,
        None => match session.latest_block {
            Some(head) if head > 32 => head.saturating_sub(32),
            Some(head) => {
                return Err(Attempt::Other(format!(
                    "{label}: peer head {head} is not a usable tip"
                )))
            }
            None => {
                return Err(Attempt::Other(format!(
                    "{label}: peer did not report a head. Pass --block, or use an eth/69+ peer."
                )))
            }
        },
    };
    println!(
        "connected eth/{} peer {} head {:?}",
        session.eth_version as u8, session.peer, session.latest_block
    );
    match read_blocks(&mut session, start, count).await {
        Ok(()) => Ok(()),
        Err(err) => Err(Attempt::Other(format!("{label}: {err:#}"))),
    }
}

async fn dial_targets(enode: Option<&str>) -> eyre::Result<Vec<NodeRecord>> {
    if let Some(enode) = enode {
        let peer = enode.parse().wrap_err("enode URL")?;
        return Ok(vec![peer]);
    }
    find_mainnet_peers().await
}

async fn read_blocks(session: &mut OutboundSession, start: u64, count: u64) -> eyre::Result<()> {
    let end = start.saturating_add(count);
    for number in start..end {
        let block = fetch_block_logs(session, number)
            .await
            .wrap_err_with(|| format!("block {number}"))?;
        let mut hits = 0_u64;
        for log in &block.logs {
            for hit in classify(ETHEREUM_MAINNET, log) {
                println!("{}", serde_json::to_string(&hit).wrap_err("encode hit")?);
                hits = hits.saturating_add(1);
            }
        }
        println!(
            "block {number} hash {} logs {} hits {hits}",
            block.hash,
            block.logs.len()
        );
    }
    Ok(())
}
