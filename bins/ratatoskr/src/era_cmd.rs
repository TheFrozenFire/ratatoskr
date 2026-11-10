//! Read pre-merge headers or logs from era1 files.
//!
//! This command does not open the venue catalog and does not dial.

use alloy_primitives::Address;
use clap::{Parser, Subcommand};
use era::{enumerate_headers, enumerate_logs, EraHttp};
use eyre::{eyre, WrapErr};
use filter::LogFilter;
use std::time::Instant;

const DEFAULT_MIRROR: &str = "https://data.ethpandaops.io/era1/mainnet/";

#[derive(Debug, Parser)]
pub struct EraArgs {
    #[command(subcommand)]
    pub command: EraCommand,
}

#[derive(Debug, Subcommand)]
pub enum EraCommand {
    /// Header identity for a block range: hash, parent, bloom, receipt root.
    Headers(RangeArgs),
    /// Logs in a block range. Blooms skip blocks that cannot match.
    Logs(LogsArgs),
}

#[derive(Debug, clap::Args)]
pub struct RangeArgs {
    /// First block, inclusive.
    #[arg(long)]
    pub from: u64,
    /// Last block, inclusive.
    #[arg(long)]
    pub to: u64,
    /// Era1 directory. File names are taken from its listing.
    #[arg(long, default_value = DEFAULT_MIRROR)]
    pub mirror: String,
    /// Filename prefix and epoch width. Mainnet epochs are 8192 blocks.
    #[arg(long, default_value = "mainnet")]
    pub network: String,
    /// Concurrent range reads on the pooled connection.
    #[arg(long, default_value_t = 128)]
    pub inflight: usize,
    /// Bytes to read at each header. Omit to measure a few headers and fit
    /// a window that includes the following body tag.
    #[arg(long)]
    pub window: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub range: RangeArgs,
    /// Emitter. Repeat to accept any of the addresses.
    #[arg(long)]
    pub address: Vec<Address>,
    /// Event signature, or a 0x topic hash. Repeat to accept any as topic0.
    #[arg(long)]
    pub event: Vec<String>,
    /// Required topic at position 0. Repeat for alternatives. Combined with --event.
    #[arg(long)]
    pub topic0: Vec<String>,
    /// Required topic at position 1. Repeat for alternatives.
    #[arg(long)]
    pub topic1: Vec<String>,
    /// Required topic at position 2. Repeat for alternatives.
    #[arg(long)]
    pub topic2: Vec<String>,
    /// Required topic at position 3. Repeat for alternatives.
    #[arg(long)]
    pub topic3: Vec<String>,
}

pub async fn run(args: EraArgs) -> eyre::Result<()> {
    match args.command {
        EraCommand::Headers(range) => headers(range).await,
        EraCommand::Logs(logs) => logs_cmd(logs).await,
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
    let filter = LogFilter::from_parts(
        args.address,
        &topic0,
        &args.topic1,
        &args.topic2,
        &args.topic3,
    )?;
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
    report(
        &http,
        args.range.to - args.range.from + 1,
        logs.len(),
        started.elapsed(),
    );
    Ok(())
}

pub fn check_range(range: &RangeArgs) -> eyre::Result<()> {
    if range.from > range.to {
        return Err(eyre!("--from {} is after --to {}", range.from, range.to));
    }
    if range.inflight == 0 || range.inflight > 1024 {
        return Err(eyre!("--inflight must be 1..=1024"));
    }
    if range
        .window
        .is_some_and(|window| !(8..=65_536).contains(&window))
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn range(from: u64, to: u64) -> RangeArgs {
        RangeArgs {
            from,
            to,
            mirror: DEFAULT_MIRROR.into(),
            network: "mainnet".into(),
            inflight: 128,
            window: None,
        }
    }

    #[test]
    fn a_range_starts_at_or_before_its_end() {
        assert!(check_range(&range(1, 2)).is_ok());
        let err = check_range(&range(3, 1)).unwrap_err();
        assert!(err.to_string().contains("after"), "{err}");
    }

    #[test]
    fn inflight_and_window_stay_inside_the_read_bounds() {
        let mut wide = range(1, 1);
        wide.inflight = 0;
        assert!(check_range(&wide)
            .unwrap_err()
            .to_string()
            .contains("inflight"));
        wide.inflight = 1025;
        assert!(check_range(&wide)
            .unwrap_err()
            .to_string()
            .contains("inflight"));
        wide.inflight = 1;
        wide.window = Some(7);
        assert!(check_range(&wide)
            .unwrap_err()
            .to_string()
            .contains("window"));
        wide.window = Some(65_537);
        assert!(check_range(&wide)
            .unwrap_err()
            .to_string()
            .contains("window"));
        wide.window = Some(8);
        assert!(check_range(&wide).is_ok());
    }
}
