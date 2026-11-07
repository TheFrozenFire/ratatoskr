//! Sync mainnet headers from `ratatoskr-peer` into a redb chain.
//!
//! Peers only serve headers by block range. This process stores the fields a
//! later bloom test needs: hash, parent, receipts root, and logs bloom. It
//! checks that each header links to the previous one, and that block 0 is
//! mainnet genesis. It does not check proof of work.

use clap::Parser;
use eyre::WrapErr;
use chain::HeaderChain;
use ipc::{body_call, max_head, serving, socket_path, status};
use model::{decode_header_records, HeaderRecord};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::sleep};
use tracing::{info, warn};
/// One `GetBlockHeaders` round. Peers cap this near 1024.
const CHUNK: u64 = 512;
const PIPELINE: usize = 16;
const CHUNKS_PER_PEER: usize = 2;
/// Stay behind the peer's head so a reorg does not rewrite the chain tip.
const TIP_LAG: u64 = 32;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-headers")]
struct Args {
    /// redb path. Created if missing.
    #[arg(long)]
    db: Option<PathBuf>,
}

fn main() -> eyre::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .wrap_err("runtime")?
        .block_on(run())
}

async fn run() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "headers=info".into()),
        )
        .init();
    let args = Args::parse();
    let db_path = args.db.unwrap_or_else(HeaderChain::default_path);
    let socket = socket_path()?;
    let chain = HeaderChain::create(&db_path)?;
    let mut next = chain.cursor()?;
    let mut parent = chain.parent_hash(next)?;
    info!(path = %db_path.display(), next, "header chain open");

    loop {
        let head = match peer_head(&socket).await {
            Some(head) => head,
            None => {
                warn!("waiting for a peer head");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let tip = head.saturating_sub(TIP_LAG);
        if next > tip {
            info!(next, head, "caught up");
            sleep(Duration::from_secs(15)).await;
            continue;
        }
        let depth = serving_peers(&socket, next)
            .await
            .saturating_mul(CHUNKS_PER_PEER)
            .clamp(1, PIPELINE);
        let mut plan = Vec::new();
        let mut issue = next;
        for _ in 0..depth {
            if issue > tip {
                break;
            }
            let width = (tip - issue + 1).min(CHUNK);
            plan.push((issue, width));
            issue += width;
        }
        if plan.is_empty() {
            sleep(Duration::from_secs(2)).await;
            continue;
        }
        let mut join = JoinSet::new();
        for (start, width) in &plan {
            let start = *start;
            let width = *width;
            let socket = socket.clone();
            join.spawn(async move {
                let started = Instant::now();
                let result = ask_headers(&socket, start, width).await;
                (start, result, started)
            });
        }
        let mut done = BTreeMap::new();
        while let Some(joined) = join.join_next().await {
            let (start, result, started) = joined.wrap_err("header task")?;
            done.insert(start, (result, started));
        }
        let mut stop = false;
        for (start, _) in &plan {
            if *start != next || stop {
                break;
            }
            let (result, started) = done
                .remove(start)
                .ok_or_else(|| eyre::eyre!("planned headers {start} did not finish"))?;
            match result {
                Ok(records) => match chain.append(&records, next, parent) {
                    Ok(last) => {
                        let blocks = records.len() as u64;
                        let secs = started.elapsed().as_secs_f32().max(0.001);
                        info!(
                            from = records[0].number,
                            to = last.number,
                            blocks,
                            next = last.number + 1,
                            rate = (blocks as f32 / secs) as u64,
                            secs,
                            "headers stored"
                        );
                        parent = last.hash;
                        next = last.number + 1;
                    }
                    Err(err) => {
                        warn!(next, %err, "header batch rejected");
                        stop = true;
                        sleep(Duration::from_secs(2)).await;
                    }
                },
                Err(err) => {
                    warn!(next, %err, "header batch failed");
                    stop = true;
                    let pause = if is_wait(&err) { 2 } else { 5 };
                    sleep(Duration::from_secs(pause)).await;
                }
            }
        }
    }
}

fn is_wait(err: &str) -> bool {
    err.contains("no peers") || err.contains("busy") || err.contains("peer process")
}

async fn peer_head(socket: &Path) -> Option<u64> {
    max_head(&status(socket).await.ok()?)
}

async fn serving_peers(socket: &Path, start: u64) -> usize {
    match status(socket).await {
        Ok(sessions) => serving(&sessions, start),
        Err(_) => 1,
    }
}

async fn ask_headers(
    socket: &Path,
    start: u64,
    count: u64,
) -> Result<Vec<HeaderRecord>, String> {
    let body = body_call(
        socket,
        &serde_json::json!({
            "op": "headers",
            "start": start,
            "count": count,
        }),
        "header",
    )
    .await
    .map_err(|err| format!("{err:#}"))?;
    decode_header_records(&body).map_err(|err| err.to_string())
}
