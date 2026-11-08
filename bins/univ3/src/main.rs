//! Read Uniswap v3 pool rows from the database server.
//!
//! This process does not open corpus.redb and does not write univ3-pools.redb.
//! It needs ratatoskr-db or ratatoskr.

use alloy_primitives::Address;
use clap::Parser;
use std::collections::BTreeMap;
use store::RemoteStore;
use venues::{PANCAKE_V3, SUSHI_V3, UNI_V3, V3_FORK};

/// This process does not open corpus.redb and does not write univ3-pools.redb, and needs ratatoskr-db or ratatoskr.
#[derive(Debug, Parser)]
#[command(name = "ratatoskr-univ3")]
struct Args {
    /// Also page Pancake v3, Sushi v3, and other v3 fork pools.
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
    let store = RemoteStore::new(store::db_socket()?);
    let mut kinds = vec![UNI_V3];
    if args.any_emitter {
        kinds.extend([PANCAKE_V3, SUSHI_V3, V3_FORK]);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let rows = runtime.block_on(async {
        let mut rows = Vec::new();
        for kind in kinds {
            rows.extend(store.scan_venues(kind).await?);
        }
        Ok::<_, eyre::Report>(rows)
    })?;

    println!("rows live in the database server");
    let mut fees: BTreeMap<u32, u64> = BTreeMap::new();
    for row in &rows {
        if let Some(fee) = row.fee {
            *fees.entry(fee).or_default() += 1_u64;
        }
    }
    for (fee, count) in fees {
        println!("fee {fee} pools {count}");
    }
    for row in rows.iter().take(5) {
        let pool = Address::from_slice(&row.id.as_slice()[12..]);
        println!(
            "{}",
            serde_json::json!({
                "pool": pool,
                "tokens": row.tokens,
                "fee": row.fee,
                "tick": row.tick_spacing,
                "factory": row.factory,
                "block": row.block_number,
                "block_hash": row.block_hash,
                "tx_index": row.tx_index,
                "log_index": row.log_index,
            })
        );
    }
    Ok(())
}
