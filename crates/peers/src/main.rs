//! Ask the peer process to record the published mainnet DNS peer lists.
//!
//! This does not dial and does not open `peers.redb`. The peer process already
//! holds that file. Block ranges are not in the ENR or the DNS tree.

use clap::Parser;
use eyre::eyre;
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-peers")]
struct Args {}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "peers=info".into()),
        )
        .init();
    let _args = Args::parse();
    let value = ipc::json_call(&ipc::socket_path()?, &serde_json::json!({"op": "census"})).await?;
    if value.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        let err = value
            .get("error")
            .and_then(|err| err.as_str())
            .unwrap_or("request failed");
        return Err(eyre!(err.to_string()));
    }
    println!(
        "peers {} all-tree {} snap-tree {} in-both {} snap-only {} catalog {}",
        count_field(&value, "peers")?,
        count_field(&value, "all-tree")?,
        count_field(&value, "snap-tree")?,
        count_field(&value, "in-both")?,
        count_field(&value, "snap-only")?,
        value
            .get("catalog")
            .and_then(|path| path.as_str())
            .ok_or_else(|| eyre!("census response missing catalog"))?
    );
    Ok(())
}

fn count_field(value: &Value, key: &str) -> eyre::Result<u64> {
    value
        .get(key)
        .and_then(|count| count.as_u64())
        .ok_or_else(|| eyre!("census response missing {key}"))
}
