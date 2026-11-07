//! Enumerate pre-merge logs, or the headers that select them, from era1 mirrors.
//!
//! Reads use HTTP range requests. Headers share one pooled connection. Receipts
//! are fetched only for blocks whose logs bloom can contain the requested
//! addresses and topics. Block bodies are skipped.

use alloy_primitives::Address;
use clap::{Parser, Subcommand};
use eyre::{eyre, WrapErr};
use era::{enumerate_headers, enumerate_logs, EraHttp};
use filter::LogFilter;
use std::time::Instant;

const DEFAULT_MIRROR: &str = "https://data.ethpandaops.io/era1/mainnet/";

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-era")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Header identity for a block range: hash, parent, bloom, receipt root.
    Headers(RangeArgs),
    /// Logs in a block range. Blooms skip blocks that cannot match.
    Logs(LogsArgs),
}

#[derive(Debug, clap::Args)]
struct RangeArgs {
    /// First block, inclusive.
    #[arg(long)]
    from: u64,
    /// Last block, inclusive.
    #[arg(long)]
    to: u64,
    /// Era1 directory. File names are taken from its listing.
    #[arg(long, default_value = DEFAULT_MIRROR)]
    mirror: String,
    /// Filename prefix and epoch width. Mainnet epochs are 8192 blocks.
    #[arg(long, default_value = "mainnet")]
    network: String,
    /// Concurrent range reads on the pooled connection.
    #[arg(long, default_value_t = 128)]
    inflight: usize,
    /// Bytes to read at each header. Omit to measure a few headers and fit
    /// a window that includes the following body tag.
    #[arg(long)]
    window: Option<u64>,
}

#[derive(Debug, clap::Args)]
struct LogsArgs {
    #[command(flatten)]
    range: RangeArgs,
    /// Emitter. Repeat to accept any of the addresses.
    #[arg(long)]
    address: Vec<Address>,
    /// Event signature, or a 0x topic hash. Repeat to accept any as topic0.
    /// Example: PoolCreated(address,address,uint24,int24,address)
    #[arg(long)]
    event: Vec<String>,
    /// Required topic at position 0. Repeat for alternatives. Combined with --event.
    #[arg(long)]
    topic0: Vec<String>,
    /// Required topic at position 1. Repeat for alternatives.
    #[arg(long)]
    topic1: Vec<String>,
    /// Required topic at position 2. Repeat for alternatives.
    #[arg(long)]
    topic2: Vec<String>,
    /// Required topic at position 3. Repeat for alternatives.
    #[arg(long)]
    topic3: Vec<String>,
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
                .unwrap_or_else(|_| "era=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    match args.command {
        Command::Headers(range) => headers(range).await,
        Command::Logs(logs) => logs_cmd(logs).await,
    }
}

async fn headers(range: RangeArgs) -> eyre::Result<()> {
    check_range(&range)?;
    let http = EraHttp::new(range.inflight)?;
    let started = Instant::now();
    let rows = enumerate_headers(
        &http,
        &range.mirror,
        &range.network,
        range.from,
        range.to,
        range.inflight,
        range.window,
    )
    .await?;
    for row in &rows {
        println!("{}", serde_json::to_string(row).wrap_err("encode header")?);
    }
    report(&http, range.to - range.from + 1, 0, started.elapsed());
    Ok(())
}

async fn logs_cmd(args: LogsArgs) -> eyre::Result<()> {
    check_range(&args.range)?;
    let mut topic0 = args.event;
    topic0.extend(args.topic0);
    let filter = LogFilter::from_parts(args.address, &topic0, &args.topic1, &args.topic2, &args.topic3)?;
    let http = EraHttp::new(args.range.inflight)?;
    let started = Instant::now();
    let logs = enumerate_logs(
        &http,
        &args.range.mirror,
        &args.range.network,
        args.range.from,
        args.range.to,
        &filter,
        args.range.inflight,
        args.range.window,
    )
    .await?;
    for log in &logs {
        println!("{}", serde_json::to_string(log).wrap_err("encode log")?);
    }
    report(&http, args.range.to - args.range.from + 1, logs.len(), started.elapsed());
    Ok(())
}

fn check_range(range: &RangeArgs) -> eyre::Result<()> {
    if range.from > range.to {
        return Err(eyre!("--from {} is after --to {}", range.from, range.to));
    }
    if range.inflight == 0 || range.inflight > 1024 {
        return Err(eyre!("--inflight must be 1..=1024"));
    }
    if range.window.is_some_and(|window| window < 8 || window > 65_536) {
        return Err(eyre!("--window must be 8..=65536 bytes"));
    }
    Ok(())
}

fn report(http: &EraHttp, blocks: u64, logs: usize, elapsed: std::time::Duration) {
    let transfer = http.transfer();
    tracing::info!(
        version = transfer.version,
        requests = transfer.requests,
        catalog_bytes = transfer.catalog,
        index_bytes = transfer.index,
        header_bytes = transfer.header,
        receipt_bytes = transfer.receipt,
        total_bytes = transfer.total(),
        blocks,
        logs,
        seconds = elapsed.as_secs_f64(),
        "era1 range read"
    );
}
