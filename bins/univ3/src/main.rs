//! Enumerate Uniswap v3 pool identities from the receipt corpus.

use clap::Parser;
use corpus::Corpus;
use univ3::{PoolScan, CANONICAL_FACTORY};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-univ3")]
struct Args {
    /// Corpus produced by `ratatoskr-ingest`.
    #[arg(long)]
    corpus: Option<PathBuf>,
    /// Pool catalog. Created if missing.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep `PoolCreated` logs from every emitter, not only the canonical factory.
    #[arg(long)]
    any_emitter: bool,
}

fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "univ3=info".into()),
        )
        .init();
    let args = Args::parse();
    let corpus_path = args.corpus.unwrap_or_else(Corpus::default_path);
    let out_path = args.out.unwrap_or_else(PoolScan::default_path);
    let corpus = Corpus::open(&corpus_path)?;
    let factory = (!args.any_emitter).then_some(CANONICAL_FACTORY);
    let scan = PoolScan::from_corpus(&corpus, factory)?;
    scan.write(&out_path)?;

    println!(
        "corpus {} blocks {} holes {} span {:?}..={:?}",
        corpus_path.display(),
        scan.blocks,
        scan.holes,
        scan.first_block,
        scan.last_block
    );
    println!(
        "univ3 pools {} catalog {} other emitters {}",
        scan.pools.len(),
        out_path.display(),
        scan.other_emitters
    );
    for (fee, count) in scan.fee_counts() {
        println!("fee {fee} pools {count}");
    }
    for (pool, row) in scan.pools.iter().take(5) {
        println!(
            "{}",
            serde_json::json!({
                "pool": pool,
                "token0": row.token0,
                "token1": row.token1,
                "fee": row.fee,
                "tick_spacing": row.tick_spacing,
                "factory": row.factory,
                "block": row.block_number,
                "log_index": row.log_index,
            })
        );
    }
    Ok(())
}
