//! Hold a small set of outbound mainnet sessions and serve blocks on a local socket.

use clap::Parser;
use eth_p2p::Egress;
use eyre::WrapErr;
use peer::{serve, socket_path, PeerConfig, TARGET_PEERS};
use serde_json::{json, Value};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::sleep,
};

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-peer")]
struct Args {
    /// Ask a running peer process for a block instead of serving.
    #[arg(long)]
    fetch: bool,
    /// Block number for `--fetch`. Omitted means the peer's head minus 32.
    #[arg(long)]
    block: Option<u64>,
    /// Print the peer table and exit.
    #[arg(long)]
    status: bool,
    /// How many outbound sessions to keep.
    #[arg(long, default_value_t = TARGET_PEERS)]
    peers: usize,
    /// Bind IPv4 RLPx to this address. Policy routing sends that source into the tunnel.
    #[arg(long)]
    bind4: Option<Ipv4Addr>,
    /// Bind IPv6 RLPx to this address.
    #[arg(long)]
    bind6: Option<Ipv6Addr>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "peer=info,eth_p2p=info".into()),
        )
        .init();

    let args = Args::parse();
    let socket = socket_path()?;
    if args.fetch || args.status {
        let op = if args.status { "status" } else { "fetch" };
        let started = Instant::now();
        let response = loop {
            let response = ask(&socket, op, args.block).await?;
            let waiting =
                response.get("error").and_then(|err| err.as_str()) == Some("no peers connected");
            if waiting && started.elapsed() < Duration::from_secs(90) {
                sleep(Duration::from_secs(2)).await;
                continue;
            }
            break response;
        };
        print_response(&response)?;
        return Ok(());
    }

    let egress = match (args.bind4, args.bind6) {
        (Some(v4), Some(v6)) => Some(Egress { v4, v6 }),
        (None, None) => None,
        _ => return Err(eyre::eyre!("pass both --bind4 and --bind6, or neither")),
    };
    serve(PeerConfig {
        peers: args.peers,
        socket,
        catalog: directory::default_path(),
        egress,
    })
    .await
}

fn print_response(response: &Value) -> eyre::Result<()> {
    if response.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        return Err(eyre::eyre!(
            "{}",
            response
                .get("error")
                .and_then(|err| err.as_str())
                .unwrap_or("request failed")
        ));
    }
    if let Some(hits) = response.get("hits").and_then(|hits| hits.as_array()) {
        for hit in hits {
            println!("{}", serde_json::to_string(hit)?);
        }
        println!(
            "block {} hash {} logs {} hits {}",
            response["number"],
            response["hash"].as_str().unwrap_or(""),
            response["logs"],
            hits.len()
        );
        return Ok(());
    }
    println!("{}", serde_json::to_string_pretty(response)?);
    Ok(())
}

async fn ask(socket: &Path, op: &str, block: Option<u64>) -> eyre::Result<Value> {
    let started = Instant::now();
    let conn = loop {
        match UnixStream::connect(socket).await {
            Ok(conn) => break conn,
            Err(err) if started.elapsed() < Duration::from_secs(5) => {
                sleep(Duration::from_millis(200)).await;
                let _ = err;
            }
            Err(err) => return Err(err).wrap_err("connect to peer socket"),
        }
    };
    let mut conn = conn;
    let request = serde_json::to_vec(&json!({"op": op, "block": block}))?;
    conn.write_all(&request).await?;
    conn.write_all(b"\n").await?;
    let mut lines = BufReader::new(conn).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| eyre::eyre!("peer process closed the socket"))?;
    Ok(serde_json::from_str(&line)?)
}
