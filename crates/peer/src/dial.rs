//! Directory merge, dial rank, cooldown, and the maintain loop.

use crate::jobs::{run_peer, session_is_dead, Slot, Table};
use eth_p2p::{fetch_header_range, Egress, OutboundSession};

use crate::discover::dns_candidates;
use observability::{debug, info, note_peers, warn};
use reth_network_peers::{NodeRecord, PeerId};
use secp256k1::SecretKey;
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};

pub(crate) enum CatalogCmd {
    WritePublished {
        peers: Vec<crate::catalog::PublishedPeer>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

fn merge_directory(directory: &mut Vec<NodeRecord>, more: Vec<NodeRecord>) {
    let mut seen: HashSet<IpAddr> = directory.iter().map(|peer| peer.address).collect();
    for peer in more {
        if seen.insert(peer.address) {
            directory.push(peer);
        }
    }
}

fn next_peer(
    directory: &[NodeRecord],
    cursor: &mut usize,
    held: &HashSet<IpAddr>,
    cooldown: &HashMap<IpAddr, Instant>,
) -> Option<NodeRecord> {
    let len = directory.len();
    if len == 0 {
        return None;
    }
    let now = Instant::now();
    for step in 0..len {
        let index = (*cursor + step) % len;
        let peer = directory[index];
        let cooling = cooldown
            .get(&peer.address)
            .is_some_and(|until| *until > now);
        if held.contains(&peer.address) || cooling {
            continue;
        }
        *cursor = (index + 1) % len;
        return Some(peer);
    }
    None
}

/// Best dial that is not already held or cooling. Equal ranks keep directory order,
/// which rotates as each pick is cooled down.
fn preferred_peer(
    directory: &[NodeRecord],
    held: &HashSet<IpAddr>,
    cooldown: &HashMap<IpAddr, Instant>,
    summaries: &HashMap<PeerId, crate::catalog::Summary>,
    wanted: u64,
) -> Option<NodeRecord> {
    let now = Instant::now();
    let mut best: Option<(u8, NodeRecord)> = None;
    for peer in directory {
        let cooling = cooldown
            .get(&peer.address)
            .is_some_and(|until| *until > now);
        if held.contains(&peer.address) || cooling {
            continue;
        }
        let rank = crate::catalog::dial_rank(summaries.get(&peer.id), wanted);
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, *peer));
        }
    }
    best.map(|(_, peer)| peer)
}

fn seed_directory(directory: &mut Vec<NodeRecord>, catalog: &crate::catalog::Catalog) {
    let known: Vec<_> = catalog
        .summaries
        .iter()
        .map(|(id, summary)| crate::catalog::node_from_summary(*id, summary))
        .collect();
    merge_directory(directory, known);
}

pub(crate) fn sighting_of(
    peer: NodeRecord,
    outcome: u8,
    eth: u8,
    earliest: Option<u64>,
    latest: Option<u64>,
    served: Option<u64>,
) -> crate::catalog::Sighting {
    crate::catalog::Sighting {
        id: peer.id,
        address: peer.address,
        tcp_port: peer.tcp_port,
        udp_port: peer.udp_port,
        outcome,
        eth,
        earliest,
        latest,
        served,
    }
}

fn dial_outcome(err: &str) -> u8 {
    if err.contains("TooManyPeers") || err.contains("too many peers") {
        crate::catalog::TOO_MANY
    } else {
        crate::catalog::DIAL_FAILED
    }
}

pub(crate) fn serve_outcome(err: &str) -> u8 {
    if err.contains("TooManyPeers") || err.contains("too many peers") {
        crate::catalog::TOO_MANY
    } else if session_is_dead(err) {
        crate::catalog::DROPPED
    } else {
        crate::catalog::EMPTY
    }
}

pub(crate) fn note(
    tx: &mpsc::UnboundedSender<crate::catalog::Sighting>,
    sighting: crate::catalog::Sighting,
) {
    let _ = tx.send(sighting);
}

/// `true` when the shutdown watch is set.
async fn pause(stop: &mut watch::Receiver<bool>, secs: u64) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
        _ = sleep(Duration::from_secs(secs)) => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn maintain(
    key: SecretKey,
    settings: config::Peer,
    network: config::Network,
    slots: Arc<Mutex<Table>>,
    wanted: Arc<AtomicU64>,
    egress: Option<Egress>,
    mut catalog: crate::catalog::Catalog,
    mut catalog_rx: mpsc::UnboundedReceiver<CatalogCmd>,
    mut stop: watch::Receiver<bool>,
) {
    let mut held: HashSet<IpAddr> = HashSet::new();
    let mut cooldown: HashMap<IpAddr, Instant> = HashMap::new();
    let mut directory: Vec<NodeRecord> = Vec::new();
    let mut cursor = 0usize;
    let mut attempts = 0u64;
    let mut dial_n = 0u64;
    let mut window_started = Instant::now();
    let mut started_in_window = 0usize;
    let (dead_tx, mut dead_rx) = mpsc::unbounded_channel::<IpAddr>();
    let (sight_tx, mut sight_rx) = mpsc::unbounded_channel::<crate::catalog::Sighting>();
    let mut dials: JoinSet<(NodeRecord, eyre::Result<OutboundSession>)> = JoinSet::new();
    let mut probes: JoinSet<()> = JoinSet::new();
    seed_directory(&mut directory, &catalog);
    // DNS walks take about a minute. Remembered endpoints are dialed while that
    // walk runs, and again whenever the walk is older than ten minutes.
    let mut directory_at = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now);
    let mut dns_not_before = Instant::now();
    let mut dns_task: Option<JoinHandle<eyre::Result<Vec<NodeRecord>>>> = None;
    info!(
        peers = directory.len(),
        summaries = catalog.summaries.len(),
        sightings = catalog.next_seq,
        "peer catalog"
    );
    'run: loop {
        if *stop.borrow() {
            break 'run;
        }
        while let Ok(ip) = dead_rx.try_recv() {
            held.remove(&ip);
            cooldown.insert(ip, Instant::now() + settings.redial_gap());
        }
        while let Ok(sighting) = sight_rx.try_recv() {
            catalog.record(sighting);
        }
        while let Ok(cmd) = catalog_rx.try_recv() {
            apply_catalog_cmd(&catalog, cmd);
        }
        seed_directory(&mut directory, &catalog);
        let live = {
            let mut guard = slots.lock().await;
            guard.slots.retain(|slot| !slot.jobs.is_closed());
            guard.slots.len()
        };
        note_peers(
            live as u64,
            catalog.summaries.len() as u64,
            catalog.next_seq,
        );
        if live >= settings.target && dials.is_empty() {
            if pause(&mut stop, 5).await {
                break 'run;
            }
            continue;
        }
        if dns_task.is_none()
            && Instant::now() >= dns_not_before
            && (directory.is_empty() || directory_at.elapsed() > settings.directory_refresh())
        {
            let limit = settings.directory_size;
            let discovery = network.clone();
            dns_task = Some(tokio::spawn(async move {
                dns_candidates(limit, &discovery).await
            }));
        }
        if dns_task.as_ref().is_some_and(|task| task.is_finished()) {
            let task = dns_task.take().expect("dns task");
            match task.await {
                Ok(Ok(peers)) => {
                    let fresh = directory.is_empty();
                    merge_directory(&mut directory, peers);
                    directory_at = Instant::now();
                    if fresh {
                        cursor = 0;
                    }
                    info!(peers = directory.len(), "peer directory");
                }
                Ok(Err(err)) => {
                    warn!(%err, "discovery failed");
                    dns_not_before = Instant::now() + Duration::from_secs(5);
                }
                Err(err) => {
                    warn!(%err, "discovery task panicked");
                    dns_not_before = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        if window_started.elapsed() >= Duration::from_secs(1) {
            window_started = Instant::now();
            started_in_window = 0;
        }
        // Keep probing until the table is full. The per-address cooldown stops
        // a redial; the per-second cap stops a single burst from walking the
        // whole DNS list.
        let under_target = held.len() < settings.target && live < settings.target;
        let wanted_block = wanted.load(Ordering::Relaxed);
        while under_target
            && dials.len() < settings.in_flight
            && started_in_window < settings.dials_per_sec
        {
            if *stop.borrow() {
                break 'run;
            }
            // One dial in four walks the directory in order so a peer we have
            // not measured yet is still tried. The rest prefer a peer whose
            // history already covers `wanted_block`.
            let explore = dial_n.is_multiple_of(4);
            let peer = if explore {
                next_peer(&directory, &mut cursor, &held, &cooldown)
            } else {
                preferred_peer(
                    &directory,
                    &held,
                    &cooldown,
                    &catalog.summaries,
                    wanted_block,
                )
                .or_else(|| next_peer(&directory, &mut cursor, &held, &cooldown))
            };
            let Some(peer) = peer else {
                break;
            };
            dial_n = dial_n.saturating_add(1);
            started_in_window = started_in_window.saturating_add(1);
            cooldown.insert(peer.address, Instant::now() + settings.redial_gap());
            let budget = settings.dial_budget();
            let dial_timeout = network.dial_timeout();
            let request_timeout = network.request_timeout();
            dials.spawn(async move {
                let dialed = peer;
                let result = match timeout(
                    budget,
                    OutboundSession::connect_with_limits(
                        peer,
                        &key,
                        egress,
                        dial_timeout,
                        request_timeout,
                    ),
                )
                .await
                {
                    Ok(inner) => inner,
                    Err(_) => Err(eyre::eyre!("dial budget elapsed")),
                };
                (dialed, result)
            });
        }
        if dials.is_empty() {
            if pause(&mut stop, 5).await {
                break 'run;
            }
            continue;
        }
        if *stop.borrow() {
            break 'run;
        }
        let joined = tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break 'run;
                }
                continue;
            }
            joined = dials.join_next() => joined,
        };
        let Some(joined) = joined else { continue };
        let (dialed, result) = match joined {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(%err, "dial task panicked");
                continue;
            }
        };
        let session = match result {
            Ok(session) => session,
            Err(err) => {
                attempts += 1;
                let text = format!("{err:#}");
                note(
                    &sight_tx,
                    sighting_of(dialed, dial_outcome(&text), 0, None, None, None),
                );
                let expected = text.contains("TooManyPeers")
                    || text.contains("too many peers")
                    || text.contains("timed out")
                    || text.contains("budget elapsed")
                    || text.contains("Connection refused")
                    || text.contains("Connection reset")
                    || text.contains("not being readable");
                if expected {
                    debug!(ip = %dialed.address, err = %text, "peer failed");
                } else {
                    warn!(ip = %dialed.address, err = %text, "peer failed");
                }
                if attempts.is_multiple_of(25) {
                    info!(
                        attempts,
                        live,
                        held = held.len(),
                        cursor,
                        summaries = catalog.summaries.len(),
                        sightings = catalog.next_seq,
                        wanted = wanted_block,
                        "still dialing"
                    );
                }
                continue;
            }
        };
        let eth = session.eth_version as u8;
        note(
            &sight_tx,
            sighting_of(
                session.peer,
                crate::catalog::SAW_STATUS,
                eth,
                session.earliest_block,
                session.latest_block,
                None,
            ),
        );
        let head = session.latest_block.filter(|head| *head > 32);
        let Some(head) = head else {
            info!(
                peer = %session.peer,
                latest_block = ?session.latest_block,
                "peer has no usable head"
            );
            continue;
        };
        if held.len() >= settings.target {
            continue;
        }
        let remote = session.peer.address;
        held.insert(remote);
        let chunks = settings.chunks_per_peer;
        let (job_tx, job_rx) = mpsc::channel(chunks.saturating_mul(2));
        let slots = Arc::clone(&slots);
        let dead_tx = dead_tx.clone();
        let sightings = sight_tx.clone();
        info!(
            peer = %session.peer,
            eth,
            head,
            earliest = ?session.earliest_block,
            "handshake complete"
        );
        probes.spawn(async move {
            let session = session;
            let block = head.saturating_sub(32);
            // A header proves the session can answer. A receipt probe is large
            // enough that peers disconnect with `too many peers` before the
            // slot is usable.
            match fetch_header_range(&session, block, 1).await {
                Ok(records) if records.len() == 1 => {
                    info!(
                        peer = %session.peer,
                        block = records[0].number,
                        hash = %records[0].hash,
                        "session served a header"
                    );
                    note(
                        &sightings,
                        sighting_of(
                            session.peer,
                            crate::catalog::SERVED,
                            eth,
                            session.earliest_block,
                            session.latest_block,
                            Some(records[0].number),
                        ),
                    );
                    let slot = Slot {
                        label: session.peer.to_string(),
                        eth,
                        head: Arc::clone(&session.head),
                        earliest: session.earliest_block,
                        jobs: job_tx,
                    };
                    if slots.lock().await.publish(slot) {
                        run_peer(session, job_rx, sightings, chunks).await;
                    }
                }
                Ok(_) => {
                    info!(peer = %session.peer, "peer returned no header");
                    note(
                        &sightings,
                        sighting_of(
                            session.peer,
                            crate::catalog::EMPTY,
                            eth,
                            session.earliest_block,
                            session.latest_block,
                            None,
                        ),
                    );
                }
                Err(err) => {
                    let text = err.to_string();
                    info!(peer = %session.peer, err = %text, "peer dropped before serving a header");
                    note(
                        &sightings,
                        sighting_of(
                            session.peer,
                            serve_outcome(&text),
                            eth,
                            session.earliest_block,
                            session.latest_block,
                            None,
                        ),
                    );
                }
            }
            let _ = dead_tx.send(remote);
        });
    }
    drop(dials);
    slots.lock().await.close();
    while probes.join_next().await.is_some() {}
}

fn apply_catalog_cmd(catalog: &crate::catalog::Catalog, cmd: CatalogCmd) {
    match cmd {
        CatalogCmd::WritePublished { peers, reply } => {
            let result = catalog
                .write_published(&peers)
                .map_err(|err| format!("{err:#}"));
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::DNS_ALL;
    use std::{
        fs,
        net::Ipv4Addr,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ratatoskr-peer-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn census_write_uses_the_open_catalog() {
        let dir = temp_dir("census");
        let path = dir.join("peers.redb");
        let catalog = crate::catalog::Catalog::open(&path).unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        apply_catalog_cmd(
            &catalog,
            CatalogCmd::WritePublished {
                peers: vec![crate::catalog::PublishedPeer {
                    id: PeerId::from_slice(&[9u8; 64]),
                    address: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)),
                    tcp_port: 30303,
                    udp_port: 30303,
                    source: DNS_ALL,
                }],
                reply: reply_tx,
            },
        );
        reply_rx.blocking_recv().unwrap().unwrap();
        drop(catalog);
        let _ = fs::remove_dir_all(&dir);
    }
}
