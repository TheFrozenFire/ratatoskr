//! Ask the running peer service.
//!
//! Status, a one-block read, and the DNS census all go through the peer
//! socket. This command does not dial and does not open the peer directory.

use clap::{Parser, Subcommand};
use eyre::{eyre, WrapErr};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Debug, Parser)]
pub struct PeerArgs {
    /// Directory whose `peer.sock` belongs to the running peer service.
    /// Default is `~/.config/ratatoskr/peer.sock`.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: PeerCommand,
}

#[derive(Debug, Subcommand)]
pub enum PeerCommand {
    /// Print the sessions the peer service holds.
    Status,
    /// Read one block from a connected session and print discovery hits.
    Fetch {
        /// Block number. Omitted means the peer's head minus 32.
        #[arg(long)]
        block: Option<u64>,
    },
    /// Record the published mainnet DNS peer lists into the peer directory.
    Census,
}

pub async fn run(args: PeerArgs) -> eyre::Result<()> {
    let socket = socket_of(args.data_dir.as_deref())?;
    match args.command {
        PeerCommand::Status => print_query(&socket, "status", None).await,
        PeerCommand::Fetch { block } => print_query(&socket, "fetch", block).await,
        PeerCommand::Census => {
            let value = ipc::json_call(&socket, &serde_json::json!({"op": "census"})).await?;
            println!("{}", census_line(&value)?);
            Ok(())
        }
    }
}

fn socket_of(data_dir: Option<&Path>) -> eyre::Result<PathBuf> {
    match data_dir {
        Some(dir) => Ok(dir.join("peer.sock")),
        None => ipc::socket_path(),
    }
}

async fn print_query(socket: &Path, op: &str, block: Option<u64>) -> eyre::Result<()> {
    let started = Instant::now();
    let response = loop {
        let response = ask(socket, op, block).await?;
        let waiting =
            response.get("error").and_then(|err| err.as_str()) == Some("no peers connected");
        if waiting && started.elapsed() < Duration::from_secs(90) {
            sleep(Duration::from_secs(2)).await;
            continue;
        }
        break response;
    };
    for line in lines_of(&response)? {
        println!("{line}");
    }
    Ok(())
}

async fn ask(socket: &Path, op: &str, block: Option<u64>) -> eyre::Result<Value> {
    let started = Instant::now();
    let request = serde_json::json!({"op": op, "block": block});
    loop {
        match ipc::json_call(socket, &request).await {
            Ok(value) => return Ok(value),
            Err(err) if connect_failed(&err) && started.elapsed() < Duration::from_secs(5) => {
                sleep(Duration::from_millis(200)).await;
            }
            Err(err) => return Err(err).wrap_err("connect to peer socket"),
        }
    }
}

fn connect_failed(err: &eyre::Report) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
        })
    })
}

pub fn lines_of(response: &Value) -> eyre::Result<Vec<String>> {
    if response.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        return Err(eyre!(
            "{}",
            response
                .get("error")
                .and_then(|err| err.as_str())
                .unwrap_or("request failed")
        ));
    }
    if let Some(hits) = response.get("hits").and_then(|hits| hits.as_array()) {
        let mut lines = Vec::with_capacity(hits.len() + 1);
        for hit in hits {
            lines.push(serde_json::to_string(hit).wrap_err("encode hit")?);
        }
        lines.push(format!(
            "block {} hash {} logs {} hits {}",
            response["number"],
            response["hash"].as_str().unwrap_or(""),
            response["logs"],
            hits.len()
        ));
        return Ok(lines);
    }
    Ok(vec![
        serde_json::to_string_pretty(response).wrap_err("encode status")?
    ])
}

pub fn census_line(value: &Value) -> eyre::Result<String> {
    if value.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        let err = value
            .get("error")
            .and_then(|err| err.as_str())
            .unwrap_or("request failed");
        return Err(eyre!(err.to_string()));
    }
    Ok(format!(
        "peers {} all-tree {} snap-tree {} in-both {} snap-only {} catalog {}",
        count_field(value, "peers")?,
        count_field(value, "all-tree")?,
        count_field(value, "snap-tree")?,
        count_field(value, "in-both")?,
        count_field(value, "snap-only")?,
        value
            .get("catalog")
            .and_then(|path| path.as_str())
            .ok_or_else(|| eyre!("census response missing catalog"))?
    ))
}

fn count_field(value: &Value, key: &str) -> eyre::Result<u64> {
    value
        .get(key)
        .and_then(|count| count.as_u64())
        .ok_or_else(|| eyre!("census response missing {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_block_read_prints_each_discovery_hit() {
        let response = json!({
            "ok": true,
            "number": 100,
            "hash": "0xabc",
            "logs": 4,
            "hits": [{"pool": "0x1"}, {"pool": "0x2"}]
        });
        let lines = lines_of(&response).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("0x1"), "{}", lines[0]);
        assert_eq!(lines[2], "block 100 hash 0xabc logs 4 hits 2");
    }

    #[test]
    fn a_census_line_names_each_published_bucket() {
        let value = json!({
            "ok": true,
            "peers": 3,
            "all-tree": 2,
            "snap-tree": 2,
            "in-both": 1,
            "snap-only": 1,
            "catalog": "/tmp/peers.redb"
        });
        let line = census_line(&value).unwrap();
        assert_eq!(
            line,
            "peers 3 all-tree 2 snap-tree 2 in-both 1 snap-only 1 catalog /tmp/peers.redb"
        );
    }

    #[test]
    fn a_refused_query_keeps_the_peer_error() {
        let err = lines_of(&json!({"ok": false, "error": "no peers connected"})).unwrap_err();
        assert!(err.to_string().contains("no peers connected"), "{err}");
    }
}
