//! Hold a small set of outbound mainnet sessions and serve blocks on a local socket.
//!
//! `ratatoskr-peer` keeps the peer table. Other programs ask it for a block and exit.
//! The node key in `~/.config/ratatoskr/node.key` stays the same across restarts.
//! Peers we have dialed are remembered in the peer catalog: each status change and
//! each newly covered block is a sighting, and the summary of that history picks
//! the next dial.

use directory as catalog;

use discover::classify;
use eth_p2p::{
    census_mainnet_dns, dns_candidates, encode_block_records, fetch_block_logs, fetch_block_range,
    fetch_header_range, DnsPeer, Egress, OutboundSession, DNS_ALL, DNS_SNAP,
};
use eyre::WrapErr;
use ipc::{BatchKind, HistoryPeer, SessionInfo};
use model::{encode_header_records, BlockRecord, DiscoveryHit, HeaderRecord, ETHEREUM_MAINNET};
use reth_network_peers::{pk2id, NodeRecord, PeerId};
use secp256k1::{SecretKey, SECP256K1};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, Mutex},
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use tracing::{debug, info, warn};

pub const TARGET_PEERS: usize = 16;
/// One pass over a few hundred DNS records takes minutes. Revisit an address
/// only after that pass, and never inside 30 seconds.
const REDIAL_GAP: Duration = Duration::from_secs(600);
const DIAL_BUDGET: Duration = Duration::from_secs(12);
/// Concurrent TCP dials. Each address is still cooled down for `REDIAL_GAP`.
const IN_FLIGHT: usize = 16;
/// New addresses per second while the table is short of `TARGET_PEERS`.
const DIALS_PER_SEC: usize = 20;
const DIRECTORY_SIZE: usize = 2500;
/// Overlapping receipt batches on one socket. Headers for the next batch go
/// out while the previous receipts are still arriving.
const CHUNKS_PER_PEER: usize = 2;

/// What `ratatoskr-peer` passes into [`serve`].
pub struct PeerConfig {
    /// How many outbound sessions to keep.
    pub peers: usize,
    pub socket: PathBuf,
    /// `peers.redb`. Opened once. A held flock is returned, not retried.
    pub catalog: PathBuf,
    pub egress: Option<Egress>,
}

/// Open `peers.redb`, bind the socket, and dial until the process stops.
///
/// The catalog is opened before the socket file is touched. `Database already
/// open` leaves the existing peer process and its socket alone.
pub async fn serve(config: PeerConfig) -> eyre::Result<()> {
    let catalog = catalog::Catalog::open(&config.catalog)?;
    let key = load_node_key()?;
    let id = pk2id(&key.public_key(SECP256K1));
    info!(
        %id,
        peers = config.peers,
        socket = %config.socket.display(),
        catalog = %config.catalog.display(),
        egress = ?config.egress,
        "peer process starting"
    );

    if config.socket.exists() {
        fs::remove_file(&config.socket).wrap_err("remove stale socket")?;
    }
    if let Some(parent) = config.socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&config.socket).wrap_err("bind socket")?;
    fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))?;

    let slots = Arc::new(Mutex::new(Vec::<Slot>::new()));
    let wanted = Arc::new(AtomicU64::new(0));
    let (catalog_tx, catalog_rx) = mpsc::unbounded_channel();
    let local = LocalPeer {
        slots: Arc::clone(&slots),
        wanted: Arc::clone(&wanted),
    };
    tokio::spawn(maintain(
        key,
        config.peers,
        Arc::clone(&local.slots),
        Arc::clone(&local.wanted),
        config.egress,
        catalog,
        catalog_rx,
    ));

    let catalog_path = Arc::new(config.catalog);
    loop {
        let (conn, _) = listener.accept().await.wrap_err("accept")?;
        let slots = Arc::clone(&local.slots);
        let wanted = Arc::clone(&local.wanted);
        let catalog_tx = catalog_tx.clone();
        let catalog_path = Arc::clone(&catalog_path);
        tokio::spawn(async move {
            if let Err(err) = handle_conn(conn, slots, wanted, catalog_tx, catalog_path).await {
                warn!(%err, "client request failed");
            }
        });
    }
}

/// In-process history reads. The slot table is the one [`serve`] dials.
///
/// `headers` and `chunk` move the vectors `fetch_header_range` and
/// `fetch_block_range` return. Encoding stays on the socket path.
#[derive(Clone)]
pub struct LocalPeer {
    slots: Arc<Mutex<Vec<Slot>>>,
    wanted: Arc<AtomicU64>,
}

impl HistoryPeer for LocalPeer {
    async fn status(&self) -> eyre::Result<Vec<SessionInfo>> {
        let guard = self.slots.lock().await;
        Ok(guard
            .iter()
            .filter(|slot| !slot.jobs.is_closed())
            .map(|slot| SessionInfo {
                label: slot.label.clone(),
                eth: u64::from(slot.eth),
                head: slot.head,
                earliest: slot.earliest,
            })
            .collect())
    }

    async fn headers(&self, start: u64, count: u64) -> eyre::Result<Vec<HeaderRecord>> {
        let count = ipc::clamp_count(BatchKind::Headers, Some(count));
        match range_response(&self.slots, &self.wanted, start, count, BatchKind::Headers).await {
            Ok(RangeRecords::Headers(records)) => Ok(records),
            Ok(RangeRecords::Blocks(_)) => Err(eyre::eyre!("header request returned blocks")),
            Err(err) => Err(eyre::eyre!(err)),
        }
    }

    async fn chunk(&self, start: u64, count: u64) -> eyre::Result<Vec<BlockRecord>> {
        let count = ipc::clamp_count(BatchKind::Chunk, Some(count));
        match range_response(&self.slots, &self.wanted, start, count, BatchKind::Chunk).await {
            Ok(RangeRecords::Blocks(records)) => Ok(records),
            Ok(RangeRecords::Headers(_)) => Err(eyre::eyre!("chunk request returned headers")),
            Err(err) => Err(eyre::eyre!(err)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    block: Option<u64>,
    start: Option<u64>,
    count: Option<u64>,
}

struct Slot {
    label: String,
    eth: u8,
    head: Option<u64>,
    earliest: Option<u64>,
    jobs: mpsc::Sender<Job>,
}

#[derive(Debug)]
enum RangeRecords {
    Headers(Vec<HeaderRecord>),
    Blocks(Vec<BlockRecord>),
}

enum Job {
    Block {
        block: u64,
        reply: oneshot::Sender<Result<Fetched, String>>,
    },
    Chunk {
        start: u64,
        count: u64,
        reply: oneshot::Sender<Result<RangeRecords, String>>,
    },
    Headers {
        start: u64,
        count: u64,
        reply: oneshot::Sender<Result<RangeRecords, String>>,
    },
}

enum CatalogCmd {
    WritePublished {
        peers: Vec<catalog::PublishedPeer>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

struct Fetched {
    peer: String,
    eth: u8,
    head: Option<u64>,
    number: u64,
    hash: String,
    logs: usize,
    hits: Vec<DiscoveryHit>,
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
    summaries: &HashMap<PeerId, catalog::Summary>,
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
        let rank = catalog::dial_rank(summaries.get(&peer.id), wanted);
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, *peer));
        }
    }
    best.map(|(_, peer)| peer)
}

fn seed_directory(directory: &mut Vec<NodeRecord>, catalog: &catalog::Catalog) {
    let known: Vec<_> = catalog
        .summaries
        .iter()
        .map(|(id, summary)| catalog::node_from_summary(*id, summary))
        .collect();
    merge_directory(directory, known);
}

fn sighting_of(
    peer: NodeRecord,
    outcome: u8,
    eth: u8,
    earliest: Option<u64>,
    latest: Option<u64>,
    served: Option<u64>,
) -> catalog::Sighting {
    catalog::Sighting {
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
        catalog::TOO_MANY
    } else {
        catalog::DIAL_FAILED
    }
}

fn serve_outcome(err: &str) -> u8 {
    if err.contains("TooManyPeers") || err.contains("too many peers") {
        catalog::TOO_MANY
    } else if session_is_dead(err) {
        catalog::DROPPED
    } else {
        catalog::EMPTY
    }
}

fn note(tx: &mpsc::UnboundedSender<catalog::Sighting>, sighting: catalog::Sighting) {
    let _ = tx.send(sighting);
}

async fn maintain(
    key: SecretKey,
    target: usize,
    slots: Arc<Mutex<Vec<Slot>>>,
    wanted: Arc<AtomicU64>,
    egress: Option<Egress>,
    mut catalog: catalog::Catalog,
    mut catalog_rx: mpsc::UnboundedReceiver<CatalogCmd>,
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
    let (sight_tx, mut sight_rx) = mpsc::unbounded_channel::<catalog::Sighting>();
    let mut dials: JoinSet<(NodeRecord, eyre::Result<OutboundSession>)> = JoinSet::new();
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
    loop {
        while let Ok(ip) = dead_rx.try_recv() {
            held.remove(&ip);
            cooldown.insert(ip, Instant::now() + REDIAL_GAP);
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
            guard.retain(|slot| !slot.jobs.is_closed());
            guard.len()
        };
        if live >= target && dials.is_empty() {
            sleep(Duration::from_secs(5)).await;
            continue;
        }
        if dns_task.is_none()
            && Instant::now() >= dns_not_before
            && (directory.is_empty() || directory_at.elapsed() > Duration::from_secs(600))
        {
            dns_task = Some(tokio::spawn(async { dns_candidates(DIRECTORY_SIZE).await }));
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
        let under_target = held.len() < target && live < target;
        let wanted_block = wanted.load(Ordering::Relaxed);
        while under_target && dials.len() < IN_FLIGHT && started_in_window < DIALS_PER_SEC {
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
            cooldown.insert(peer.address, Instant::now() + REDIAL_GAP);
            dials.spawn(async move {
                let dialed = peer;
                let result = match timeout(
                    DIAL_BUDGET,
                    OutboundSession::connect_via(peer, &key, egress),
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
            sleep(Duration::from_secs(5)).await;
            continue;
        }
        let joined = dials.join_next().await;
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
                catalog::SAW_STATUS,
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
        if held.len() >= target {
            continue;
        }
        let remote = session.peer.address;
        held.insert(remote);
        let (job_tx, job_rx) = mpsc::channel(CHUNKS_PER_PEER.saturating_mul(2));
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
        tokio::spawn(async move {
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
                            catalog::SERVED,
                            eth,
                            session.earliest_block,
                            session.latest_block,
                            Some(records[0].number),
                        ),
                    );
                    let slot = Slot {
                        label: session.peer.to_string(),
                        eth,
                        head: session.latest_block,
                        earliest: session.earliest_block,
                        jobs: job_tx,
                    };
                    slots.lock().await.push(slot);
                    run_peer(session, job_rx, sightings).await;
                }
                Ok(_) => {
                    info!(peer = %session.peer, "peer returned no header");
                    note(
                        &sightings,
                        sighting_of(
                            session.peer,
                            catalog::EMPTY,
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
}

async fn run_peer(
    session: OutboundSession,
    mut jobs: mpsc::Receiver<Job>,
    sightings: mpsc::UnboundedSender<catalog::Sighting>,
) {
    let mut inflight: JoinSet<bool> = JoinSet::new();
    let mut jobs_open = true;
    loop {
        if !jobs_open && inflight.is_empty() {
            return;
        }
        tokio::select! {
            biased;
            joined = inflight.join_next(), if !inflight.is_empty() => {
                match joined {
                    Some(Ok(true)) | Some(Err(_)) => return,
                    _ => {}
                }
            }
            job = jobs.recv(), if jobs_open && inflight.len() < CHUNKS_PER_PEER => {
                match job {
                    Some(job) => {
                        let session = session.clone();
                        let sightings = sightings.clone();
                        inflight.spawn(async move { run_job(session, job, sightings).await });
                    }
                    None => jobs_open = false,
                }
            }
        }
    }
}

fn remember_serve(
    sightings: &mpsc::UnboundedSender<catalog::Sighting>,
    session: &OutboundSession,
    served: Option<u64>,
    err: Option<&str>,
) {
    let outcome = match (served, err) {
        (Some(_), None) => catalog::SERVED,
        (_, Some(err)) => serve_outcome(err),
        (None, None) => catalog::EMPTY,
    };
    note(
        sightings,
        sighting_of(
            session.peer,
            outcome,
            session.eth_version as u8,
            session.earliest_block,
            session.latest_block,
            served,
        ),
    );
}

async fn run_job(
    session: OutboundSession,
    job: Job,
    sightings: mpsc::UnboundedSender<catalog::Sighting>,
) -> bool {
    match job {
        Job::Block { block, reply } => {
            let result = fetch_one(&session, block).await;
            match &result {
                Ok(_) => remember_serve(&sightings, &session, Some(block), None),
                Err(err) => remember_serve(&sightings, &session, None, Some(err)),
            }
            let failed = result.is_err();
            let _ = reply.send(result);
            failed
        }
        Job::Chunk {
            start,
            count,
            reply,
        } => {
            let result = fetch_block_range(&session, start, count)
                .await
                .map_err(|err| format!("{err:#}"));
            // A peer can refuse one old block and still serve others.
            // Drop the socket only when the stream itself failed.
            let failed = result.as_ref().is_err_and(|err| session_is_dead(err));
            match &result {
                Ok(records) if !records.is_empty() => {
                    remember_serve(&sightings, &session, Some(start), None);
                }
                Ok(_) => remember_serve(&sightings, &session, None, None),
                Err(err) => {
                    info!(peer = %session.peer, start, count, err, "chunk failed");
                    remember_serve(&sightings, &session, None, Some(err));
                }
            }
            let _ = reply.send(result.map(RangeRecords::Blocks));
            failed
        }
        Job::Headers {
            start,
            count,
            reply,
        } => {
            let result = fetch_header_range(&session, start, count)
                .await
                .map_err(|err| format!("{err:#}"));
            let failed = result.as_ref().is_err_and(|err| session_is_dead(err));
            match &result {
                Ok(records) if !records.is_empty() => {
                    remember_serve(&sightings, &session, Some(start), None);
                }
                Ok(_) => remember_serve(&sightings, &session, None, None),
                Err(err) => {
                    info!(peer = %session.peer, start, count, err, "headers failed");
                    remember_serve(&sightings, &session, None, Some(err));
                }
            }
            let _ = reply.send(result.map(RangeRecords::Headers));
            failed
        }
    }
}

async fn fetch_one(session: &OutboundSession, block: u64) -> Result<Fetched, String> {
    let block = fetch_block_logs(session, block)
        .await
        .map_err(|err| format!("{err:#}"))?;
    let hits = block
        .logs
        .iter()
        .flat_map(|log| classify(ETHEREUM_MAINNET, log))
        .collect::<Vec<_>>();
    Ok(Fetched {
        peer: session.peer.to_string(),
        eth: session.eth_version as u8,
        head: session.latest_block,
        number: block.number,
        hash: block.hash.to_string(),
        logs: block.logs.len(),
        hits,
    })
}

fn session_is_dead(err: &str) -> bool {
    err.contains("disconnected")
        || err.contains("peer closed")
        || err.contains("eth message")
        || err.contains("stream")
        || err.contains("Connection reset")
        || err.contains("broken pipe")
}

fn apply_catalog_cmd(catalog: &catalog::Catalog, cmd: CatalogCmd) {
    match cmd {
        CatalogCmd::WritePublished { peers, reply } => {
            let result = catalog
                .write_published(&peers)
                .map_err(|err| format!("{err:#}"));
            let _ = reply.send(result);
        }
    }
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
) -> Value {
    let peers = match census_mainnet_dns().await {
        Ok(peers) => peers,
        Err(err) => return json!({"ok": false, "error": format!("{err:#}")}),
    };
    let published: Vec<catalog::PublishedPeer> = peers
        .iter()
        .map(|peer| catalog::PublishedPeer {
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

async fn handle_conn(
    conn: UnixStream,
    slots: Arc<Mutex<Vec<Slot>>>,
    wanted: Arc<AtomicU64>,
    catalog_tx: mpsc::UnboundedSender<CatalogCmd>,
    catalog_path: Arc<PathBuf>,
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
        let count = ipc::clamp_count(kind, request.count);
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
        "census" => census_response(&catalog_tx, &catalog_path).await,
        other => json!({"ok": false, "error": format!("unknown op {other}")}),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    Ok(())
}

async fn status_response(slots: &Mutex<Vec<Slot>>) -> Value {
    let guard = slots.lock().await;
    let peers: Vec<_> = guard
        .iter()
        .filter(|slot| !slot.jobs.is_closed())
        .map(|slot| {
            json!({
                "peer": slot.label,
                "eth": slot.eth,
                "head": slot.head,
                "earliest": slot.earliest,
            })
        })
        .collect();
    json!({"ok": true, "peers": peers})
}

async fn fetch_response(slots: &Mutex<Vec<Slot>>, block: Option<u64>) -> Value {
    let mut errors = Vec::new();
    for _ in 0..3 {
        let job = {
            let guard = slots.lock().await;
            let mut ranked: Vec<_> = guard.iter().filter(|slot| !slot.jobs.is_closed()).collect();
            ranked.sort_by_key(|slot| std::cmp::Reverse(slot.head.unwrap_or(0)));
            let Some(slot) = ranked.first() else {
                return json!({"ok": false, "error": "no peers connected"});
            };
            let number = match block {
                Some(number) => number,
                None => match slot.head {
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

async fn range_response(
    slots: &Mutex<Vec<Slot>>,
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
            let mut ranked: Vec<_> = guard.iter().filter(|slot| !slot.jobs.is_closed()).collect();
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
                (rank, std::cmp::Reverse(slot.head.unwrap_or(0)))
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

fn config_dir() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").wrap_err("HOME")?;
    let dir = PathBuf::from(home).join(".config/ratatoskr");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn socket_path() -> eyre::Result<PathBuf> {
    Ok(config_dir()?.join("peer.sock"))
}

fn load_node_key() -> eyre::Result<SecretKey> {
    let path = config_dir()?.join("node.key");
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        let bytes = hex_decode(text.trim()).wrap_err("node.key")?;
        return SecretKey::from_slice(&bytes).wrap_err("node.key");
    }
    let key = SecretKey::new(&mut rand::thread_rng());
    let mut hex = String::new();
    for byte in key.secret_bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    fs::write(&path, hex + "\n")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    info!(%created, path = %path.display(), "wrote a new node key");
    Ok(key)
}

fn hex_decode(text: &str) -> eyre::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(eyre::eyre!("odd hex length"));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_val(bytes[index])?;
        let lo = hex_val(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_val(byte: u8) -> eyre::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(eyre::eyre!("bad hex")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eth_p2p::{DnsPeer, DNS_ALL, DNS_SNAP};
    use model::{decode_header_records, encode_header_records};
    use reth_network_peers::{NodeRecord, PeerId};
    use std::{
        net::{IpAddr, Ipv4Addr},
        process::{Command, Stdio},
        sync::Mutex as StdMutex,
        time::UNIX_EPOCH,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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

    #[test]
    fn census_write_uses_the_open_catalog() {
        let dir = temp_dir("census");
        let path = dir.join("peers.redb");
        let catalog = catalog::Catalog::open(&path).unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        apply_catalog_cmd(
            &catalog,
            CatalogCmd::WritePublished {
                peers: vec![catalog::PublishedPeer {
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

    #[tokio::test]
    async fn local_peer_returns_fetched_headers_without_a_second_codec() {
        let slots = Arc::new(Mutex::new(Vec::new()));
        let wanted = Arc::new(AtomicU64::new(u64::MAX));
        let seen = Arc::new(StdMutex::new(None));
        let (tx, mut rx) = mpsc::channel(4);
        slots.lock().await.push(Slot {
            label: "fake".into(),
            eth: 69,
            head: Some(80),
            earliest: Some(0),
            jobs: tx,
        });
        let seen_job = Arc::clone(&seen);
        let worker = tokio::spawn(async move {
            let Job::Headers {
                start,
                count,
                reply,
            } = rx.recv().await.unwrap()
            else {
                panic!("expected a header job");
            };
            *seen_job.lock().unwrap() = Some((start, count));
            let records = decode_header_records(&header_bytes(11)).unwrap();
            reply.send(Ok(RangeRecords::Headers(records))).unwrap();
        });
        let peer = LocalPeer {
            slots,
            wanted: Arc::clone(&wanted),
        };
        let headers = peer.headers(11, 5_000).await.unwrap();
        worker.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((11, 1024)));
        assert_eq!(headers, decode_header_records(&header_bytes(11)).unwrap());
        assert_eq!(encode_header_records(&headers), header_bytes(11));
        assert_eq!(wanted.load(Ordering::Relaxed), 11);
        let err = LocalPeer {
            slots: Arc::new(Mutex::new(Vec::new())),
            wanted: Arc::new(AtomicU64::new(0)),
        }
        .chunk(1, 1)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no peers connected"), "{err}");
    }

    #[tokio::test]
    async fn socket_handler_encodes_headers_and_clamps_a_missing_count() {
        let (client, server) = UnixStream::pair().unwrap();
        let slots = Arc::new(Mutex::new(Vec::new()));
        let wanted = Arc::new(AtomicU64::new(u64::MAX));
        let (tx, mut rx) = mpsc::channel(4);
        slots.lock().await.push(Slot {
            label: "fake".into(),
            eth: 69,
            head: Some(80),
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

    #[tokio::test]
    async fn second_catalog_open_fails_without_retry() {
        let dir = temp_dir("lock");
        let path = dir.join("peers.redb");
        let socket = dir.join("peer.sock");
        let child = Command::new("flock")
            .arg("-x")
            .arg(&path)
            .arg("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let kill = Kill(child);
        let wait_started = Instant::now();
        loop {
            let status = Command::new("flock")
                .arg("-n")
                .arg("-x")
                .arg(&path)
                .arg("true")
                .status()
                .unwrap();
            if !status.success() {
                break;
            }
            assert!(
                wait_started.elapsed() < Duration::from_secs(2),
                "catalog lock was not taken"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            serve(PeerConfig {
                peers: 1,
                socket: socket.clone(),
                catalog: path,
                egress: None,
            }),
        )
        .await;
        match result {
            Ok(Err(err)) => {
                let text = format!("{err:#}");
                assert!(
                    text.contains("Database already open"),
                    "fail-fast error was {text}"
                );
            }
            Ok(Ok(())) => panic!("serve returned without holding the accept loop"),
            Err(_) => panic!("serve did not fail within 2s"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!socket.exists());
        drop(kill);
        let _ = fs::remove_dir_all(&dir);
    }
}
