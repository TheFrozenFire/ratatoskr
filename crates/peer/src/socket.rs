//! Bind the local socket and answer one newline-JSON request.

use crate::dial::CatalogCmd;
use crate::history::{clamp_count, BatchKind};
use crate::jobs::{slot_head, Job, RangeRecords, Table};
use eth_p2p::{census_mainnet_dns, encode_block_records, DnsPeer, DNS_ALL, DNS_SNAP};
use eyre::WrapErr;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use store_model::encode_header_records;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, Mutex},
};

/// Remove a stale socket file and bind mode 0600. Does not dial.
pub(crate) fn bind_socket(socket: &Path) -> eyre::Result<UnixListener> {
    if socket.exists() {
        fs::remove_file(socket).wrap_err("remove stale socket")?;
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket).wrap_err("bind socket")?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    block: Option<u64>,
    start: Option<u64>,
    count: Option<u64>,
}

fn census_json(peers: &[DnsPeer], catalog_path: &Path) -> Value {
    let all = peers
        .iter()
        .filter(|peer| peer.source & DNS_ALL != 0)
        .count();
    let snap = peers
        .iter()
        .filter(|peer| peer.source & DNS_SNAP != 0)
        .count();
    let both = peers
        .iter()
        .filter(|peer| peer.source & (DNS_ALL | DNS_SNAP) == DNS_ALL | DNS_SNAP)
        .count();
    let snap_only = peers
        .iter()
        .filter(|peer| peer.source & DNS_SNAP != 0 && peer.source & DNS_ALL == 0)
        .count();
    json!({
        "ok": true,
        "peers": peers.len(),
        "all-tree": all,
        "snap-tree": snap,
        "in-both": both,
        "snap-only": snap_only,
        "catalog": catalog_path.display().to_string(),
    })
}

async fn census_response(
    catalog_tx: &mpsc::UnboundedSender<CatalogCmd>,
    catalog_path: &Path,
    network: &config::Network,
) -> Value {
    let peers = match census_mainnet_dns(network).await {
        Ok(peers) => peers,
        Err(err) => return json!({"ok": false, "error": format!("{err:#}")}),
    };
    let published: Vec<crate::catalog::PublishedPeer> = peers
        .iter()
        .map(|peer| crate::catalog::PublishedPeer {
            id: peer.record.id,
            address: peer.record.address,
            tcp_port: peer.record.tcp_port,
            udp_port: peer.record.udp_port,
            source: peer.source,
        })
        .collect();
    let (reply_tx, reply_rx) = oneshot::channel();
    if catalog_tx
        .send(CatalogCmd::WritePublished {
            peers: published,
            reply: reply_tx,
        })
        .is_err()
    {
        return json!({"ok": false, "error": "peer catalog stopped"});
    }
    match reply_rx.await {
        Ok(Ok(())) => census_json(&peers, catalog_path),
        Ok(Err(err)) => json!({"ok": false, "error": err}),
        Err(_) => json!({"ok": false, "error": "peer catalog dropped the census"}),
    }
}

pub(crate) async fn handle_conn(
    conn: UnixStream,
    slots: Arc<Mutex<Table>>,
    wanted: Arc<AtomicU64>,
    catalog_tx: mpsc::UnboundedSender<CatalogCmd>,
    catalog_path: Arc<PathBuf>,
    network: config::Network,
) -> eyre::Result<()> {
    let (read, mut write) = conn.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Request = serde_json::from_str(&line).wrap_err("request json")?;
    if request.op == "chunk" || request.op == "headers" {
        let start = request
            .start
            .ok_or_else(|| eyre::eyre!("range request needs start"))?;
        let kind = if request.op == "headers" {
            BatchKind::Headers
        } else {
            BatchKind::Chunk
        };
        let count = clamp_count(kind, request.count);
        match range_response(&slots, &wanted, start, count, kind).await {
            Ok(records) => {
                let body = match records {
                    RangeRecords::Headers(records) => encode_header_records(&records),
                    RangeRecords::Blocks(records) => encode_block_records(&records),
                };
                let header = serde_json::to_vec(&json!({
                    "ok": true,
                    "bytes": body.len(),
                }))?;
                write.write_all(&header).await?;
                write.write_all(b"\n").await?;
                write.write_all(&body).await?;
            }
            Err(err) => {
                let mut bytes = serde_json::to_vec(&json!({"ok": false, "error": err}))?;
                bytes.push(b'\n');
                write.write_all(&bytes).await?;
            }
        }
        return Ok(());
    }
    let response = match request.op.as_str() {
        "status" => status_response(&slots).await,
        "fetch" => fetch_response(&slots, request.block).await,
        "census" => census_response(&catalog_tx, &catalog_path, &network).await,
        other => json!({"ok": false, "error": format!("unknown op {other}")}),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    Ok(())
}

async fn status_response(slots: &Mutex<Table>) -> Value {
    let guard = slots.lock().await;
    let peers: Vec<_> = guard
        .slots
        .iter()
        .filter(|slot| !slot.jobs.is_closed())
        .map(|slot| {
            json!({
                "peer": slot.label,
                "eth": slot.eth,
                "head": slot_head(slot),
                "earliest": slot.earliest,
            })
        })
        .collect();
    json!({"ok": true, "peers": peers})
}

async fn fetch_response(slots: &Mutex<Table>, block: Option<u64>) -> Value {
    let mut errors = Vec::new();
    for _ in 0..3 {
        let job = {
            let guard = slots.lock().await;
            let mut ranked: Vec<_> = guard
                .slots
                .iter()
                .filter(|slot| !slot.jobs.is_closed())
                .collect();
            ranked.sort_by_key(|slot| std::cmp::Reverse(slot_head(slot).unwrap_or(0)));
            let Some(slot) = ranked.first() else {
                return json!({"ok": false, "error": "no peers connected"});
            };
            let number = match block {
                Some(number) => number,
                None => match slot_head(slot) {
                    Some(head) if head > 32 => head.saturating_sub(32),
                    Some(head) => {
                        return json!({"ok": false, "error": format!("peer head {head} is not a usable tip")});
                    }
                    None => {
                        return json!({"ok": false, "error": "peer did not report a head"});
                    }
                },
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if slot
                .jobs
                .try_send(Job::Block {
                    block: number,
                    reply: reply_tx,
                })
                .is_err()
            {
                errors.push(format!("{}: busy or closed", slot.label));
                continue;
            }
            reply_rx
        };
        match job.await {
            Ok(Ok(fetched)) => {
                return json!({
                    "ok": true,
                    "peer": fetched.peer,
                    "eth": fetched.eth,
                    "head": fetched.head,
                    "number": fetched.number,
                    "hash": fetched.hash,
                    "logs": fetched.logs,
                    "hits": fetched.hits,
                });
            }
            Ok(Err(err)) => errors.push(err),
            Err(_) => errors.push("peer dropped the request".to_string()),
        }
    }
    json!({"ok": false, "error": errors.join("; ")})
}

pub(crate) async fn range_response(
    slots: &Mutex<Table>,
    wanted: &AtomicU64,
    start: u64,
    count: u64,
    kind: BatchKind,
) -> Result<RangeRecords, String> {
    // A pipeline asks for later blocks alongside the cursor. The dial
    // target stays on the oldest block a client still needs.
    wanted.fetch_min(start, Ordering::Relaxed);
    let mut errors = Vec::new();
    for _ in 0..3 {
        let job = {
            let guard = slots.lock().await;
            let mut ranked: Vec<_> = guard
                .slots
                .iter()
                .filter(|slot| !slot.jobs.is_closed())
                .collect();
            if ranked.is_empty() {
                return Err("no peers connected".to_string());
            }
            // Ask a peer that advertised this block before one that only has a
            // recent window. eth/68 omits earliest, so those peers stay eligible.
            if !ranked.is_empty()
                && ranked
                    .iter()
                    .all(|slot| slot.earliest.is_some_and(|earliest| earliest > start))
            {
                return Err(format!("no peer serves block {start}"));
            }
            ranked.sort_by_key(|slot| {
                let rank = match slot.earliest {
                    Some(earliest) if earliest <= start => 0,
                    None => 1,
                    Some(_) => 2,
                };
                (rank, std::cmp::Reverse(slot_head(slot).unwrap_or(0)))
            });
            let mut receiver = None;
            for slot in ranked {
                if slot.earliest.is_some_and(|earliest| earliest > start) {
                    continue;
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                let job = match kind {
                    BatchKind::Chunk => Job::Chunk {
                        start,
                        count,
                        reply: reply_tx,
                    },
                    BatchKind::Headers => Job::Headers {
                        start,
                        count,
                        reply: reply_tx,
                    },
                };
                if slot.jobs.try_send(job).is_ok() {
                    receiver = Some(reply_rx);
                    break;
                }
            }
            let Some(reply_rx) = receiver else {
                return Err("peers are busy".to_string());
            };
            reply_rx
        };
        match job.await {
            Ok(Ok(records)) => return Ok(records),
            Ok(Err(err)) => return Err(err),
            Err(_) => errors.push("peer dropped the request".to_string()),
        }
    }
    Err(if errors.is_empty() {
        "no peers connected".to_string()
    } else {
        errors.join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::Slot;
    use reth_network_peers::{NodeRecord, PeerId};
    use std::net::{IpAddr, Ipv4Addr};
    use store_model::decode_header_records;
    use tokio::io::AsyncReadExt;

    fn header_bytes(number: u64) -> Vec<u8> {
        let mut out = vec![0u8; 369];
        out[0] = 1;
        out[1..9].copy_from_slice(&number.to_le_bytes());
        out
    }

    fn dns_peer(byte: u8, source: u8) -> DnsPeer {
        DnsPeer {
            record: NodeRecord {
                address: IpAddr::V4(Ipv4Addr::new(198, 51, 100, byte)),
                tcp_port: 30303,
                udp_port: 30303,
                id: PeerId::from_slice(&[byte; 64]),
            },
            source,
        }
    }

    #[test]
    fn census_counts_match_the_printed_buckets() {
        let peers = vec![
            dns_peer(1, DNS_ALL),
            dns_peer(2, DNS_SNAP),
            dns_peer(3, DNS_ALL | DNS_SNAP),
        ];
        let value = census_json(&peers, Path::new("/tmp/ratatoskr-peer-test/peers.redb"));
        assert_eq!(value["peers"].as_u64(), Some(3));
        assert_eq!(value["all-tree"].as_u64(), Some(2));
        assert_eq!(value["snap-tree"].as_u64(), Some(2));
        assert_eq!(value["in-both"].as_u64(), Some(1));
        assert_eq!(value["snap-only"].as_u64(), Some(1));
        assert_eq!(value["ok"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn socket_handler_encodes_headers_and_clamps_a_missing_count() {
        let (client, server) = UnixStream::pair().unwrap();
        let slots = Arc::new(Mutex::new(Table::new()));
        let wanted = Arc::new(AtomicU64::new(u64::MAX));
        let (tx, mut rx) = mpsc::channel(4);
        slots.lock().await.slots.push(Slot {
            label: "fake".into(),
            eth: 69,
            head: Arc::new(AtomicU64::new(80)),
            earliest: Some(0),
            jobs: tx,
        });
        let worker = tokio::spawn(async move {
            let Job::Headers { count, reply, .. } = rx.recv().await.unwrap() else {
                panic!("expected a header job");
            };
            assert_eq!(count, 512);
            let records = decode_header_records(&header_bytes(4)).unwrap();
            reply.send(Ok(RangeRecords::Headers(records))).unwrap();
        });
        let (catalog_tx, mut catalog_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move { while catalog_rx.recv().await.is_some() {} });
        let handler = tokio::spawn(handle_conn(
            server,
            slots,
            wanted,
            catalog_tx,
            Arc::new(PathBuf::from("/tmp/ratatoskr-peer-test/peers.redb")),
            config::Network::default(),
        ));
        let (read, mut write) = client.into_split();
        write
            .write_all(br#"{"op":"headers","start":4}"#)
            .await
            .unwrap();
        write.write_all(b"\n").await.unwrap();
        drop(write);
        let mut lines = BufReader::new(read);
        let mut line = String::new();
        lines.read_line(&mut line).await.unwrap();
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["ok"].as_bool(), Some(true));
        let len = value["bytes"].as_u64().unwrap() as usize;
        let mut body = vec![0u8; len];
        lines.read_exact(&mut body).await.unwrap();
        assert_eq!(body, header_bytes(4));
        worker.await.unwrap();
        handler.await.unwrap().unwrap();
    }
}
