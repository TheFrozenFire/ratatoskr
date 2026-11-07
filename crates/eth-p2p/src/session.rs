//! Dial one enode and complete the RLPx, Hello, and eth Status handshakes.

use crate::discover::find_mainnet_peers;
use alloy_primitives::{B256, U256};
use eyre::{eyre, WrapErr};
use futures::{SinkExt, StreamExt};
use reth_chainspec::MAINNET;
use reth_ecies::stream::ECIESStream;
use reth_eth_wire::{
    EthMessage, EthStream, HelloMessage, HelloMessageWithProtocols, UnauthedEthStream,
    UnauthedP2PStream,
};
use reth_eth_wire_types::{Capability, EthNetworkPrimitives, EthVersion, UnifiedStatus};
use reth_ethereum_forks::Head;
#[cfg(test)]
use reth_network_peers::PeerId;
use reth_network_peers::{pk2id, NodeRecord};
use secp256k1::{SecretKey, SECP256K1};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tracing::{info, warn};

/// Eth versions this spike will speak. Highest shared version wins.
const OFFERED_ETH: [EthVersion; 5] = [
    EthVersion::Eth72,
    EthVersion::Eth71,
    EthVersion::Eth70,
    EthVersion::Eth69,
    EthVersion::Eth68,
];

const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// A receipt batch is several megabytes. Fifteen seconds was dropping sessions
/// that were still sending.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

type EthIo = EthStream<reth_eth_wire::P2PStream<ECIESStream<TcpStream>>, EthNetworkPrimitives>;

struct PendingRequest {
    message: EthMessage,
    reply: oneshot::Sender<Result<EthMessage, String>>,
}

struct Waiter {
    started: Instant,
    reply: oneshot::Sender<Result<EthMessage, String>>,
}

/// A connected peer. Clone shares the same socket. Several receipt batches
/// can be in flight; responses are matched by request id.
#[derive(Clone)]
pub struct OutboundSession {
    commands: mpsc::Sender<PendingRequest>,
    next_request_id: Arc<AtomicU64>,
    pub peer: NodeRecord,
    pub eth_version: EthVersion,
    /// Peer head from the eth/69+ status message. Absent on eth/68.
    pub latest_block: Option<u64>,
    /// First block the peer says it can serve. Absent on eth/68.
    pub earliest_block: Option<u64>,
}

impl OutboundSession {
    /// Dial `enode` if one was given, then peers from a discv4 lookup, until a
    /// session completes. Each attempt is a separate outbound socket.
    pub async fn connect_first(enode: Option<&str>) -> eyre::Result<Self> {
        let mut failures = Vec::new();
        if let Some(enode) = enode {
            match Self::connect(enode).await {
                Ok(session) => return Ok(session),
                Err(err) => {
                    warn!(%enode, %err, "preferred peer failed");
                    failures.push(format!("{enode}: {err}"));
                }
            }
        }

        let peers = find_mainnet_peers().await?;
        for peer in peers {
            let label = peer.to_string();
            match Self::connect_record(peer).await {
                Ok(session) => return Ok(session),
                Err(err) => {
                    warn!(peer = %label, err = format!("{err:#}"), "peer failed");
                    failures.push(format!("{label}: {err:#}"));
                }
            }
        }
        Err(eyre!(
            "no peer completed a session:\n{}",
            failures.join("\n")
        ))
    }

    /// Connect to `enode`. The socket is outbound only. Listen port in Hello is 0.
    pub async fn connect(enode: &str) -> eyre::Result<Self> {
        let peer: NodeRecord = enode.parse().wrap_err("enode URL")?;
        Self::connect_record(peer).await
    }

    pub async fn connect_record(peer: NodeRecord) -> eyre::Result<Self> {
        let secret = SecretKey::new(&mut rand::thread_rng());
        Self::connect_with_key(peer, &secret).await
    }

    /// Same handshake as [`Self::connect_record`], using a stable node key.
    pub async fn connect_with_key(peer: NodeRecord, secret: &SecretKey) -> eyre::Result<Self> {
        if peer.tcp_port == 0 {
            return Err(eyre!("peer advertises no TCP port"));
        }
        let addr = SocketAddr::new(peer.address, peer.tcp_port);
        let tcp = timeout(DIAL_TIMEOUT, TcpStream::connect(addr))
            .await
            .wrap_err("dial timed out")?
            .wrap_err("dial failed")?;

        let local_id = pk2id(&secret.public_key(SECP256K1));
        let ecies = ECIESStream::connect(tcp, *secret, peer.id)
            .await
            .wrap_err("RLPx handshake")?;

        let hello = HelloMessageWithProtocols::builder(local_id)
            .client_version("ratatoskr/0.1.0")
            .port(0)
            .protocols(OFFERED_ETH.map(Into::into))
            .build();
        let (p2p, their_hello) = UnauthedP2PStream::new(ecies)
            .handshake(hello)
            .await
            .wrap_err("Hello handshake")?;

        let eth_version = shared_eth_version(&their_hello)
            .ok_or_else(|| eyre!("peer advertised none of eth/68 through eth/72"))?;
        let status = mainnet_status(eth_version);
        let fork_filter = MAINNET.fork_filter(status_head());
        let (stream, their_status) = UnauthedEthStream::new(p2p)
            .handshake::<EthNetworkPrimitives>(status, fork_filter)
            .await
            .wrap_err("eth Status handshake")?;

        if their_status.genesis != MAINNET.genesis_hash() {
            return Err(eyre!(
                "peer genesis {} is not mainnet {}",
                their_status.genesis,
                MAINNET.genesis_hash()
            ));
        }

        info!(
            %addr,
            ?eth_version,
            our_fork = ?status.forkid,
            peer_fork = ?their_status.forkid,
            latest_block = their_status.latest_block,
            earliest_block = their_status.earliest_block,
            "outbound session ready"
        );
        let (commands, incoming) = mpsc::channel(16);
        tokio::spawn(drive_session(stream, incoming));
        Ok(Self {
            commands,
            next_request_id: Arc::new(AtomicU64::new(1)),
            peer,
            eth_version,
            latest_block: their_status.latest_block,
            earliest_block: their_status.earliest_block,
        })
    }

    pub(crate) async fn request(&self, message: EthMessage) -> eyre::Result<EthMessage> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(PendingRequest { message, reply })
            .await
            .map_err(|_| eyre!("peer closed the session"))?;
        match timeout(REQUEST_TIMEOUT, response).await {
            Ok(Ok(Ok(message))) => Ok(message),
            Ok(Ok(Err(err))) => Err(eyre!(err)),
            Ok(Err(_)) => Err(eyre!("peer closed the session")),
            Err(_) => Err(eyre!("eth response timed out")),
        }
    }

    pub(crate) fn alloc_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn version(&self) -> EthVersion {
        self.eth_version
    }
}

/// Own the socket. Match responses to request ids so two batches can overlap,
/// and flush after every read so a queued pong actually leaves.
async fn drive_session(mut stream: EthIo, mut commands: mpsc::Receiver<PendingRequest>) {
    let mut waiters: HashMap<u64, Waiter> = HashMap::new();
    loop {
        sweep_waiters(&mut waiters);
        tokio::select! {
            biased;
            incoming = stream.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    Some(Err(err)) => {
                        fail_waiters(&mut waiters, format!("eth message: {err}"));
                        return;
                    }
                    None => {
                        fail_waiters(&mut waiters, "peer closed the session".to_string());
                        return;
                    }
                };
                if let Some(id) = response_id(&message) {
                    if let Some(waiter) = waiters.remove(&id) {
                        let _ = waiter.reply.send(Ok(message));
                    }
                }
                if let Err(err) = stream.flush().await {
                    fail_waiters(&mut waiters, format!("flush session: {err}"));
                    return;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    return;
                };
                let Some(request_id) = request_id_of(&command.message) else {
                    let _ = command.reply.send(Err("message has no request id".to_string()));
                    continue;
                };
                if let Err(err) = stream.send(command.message).await {
                    let _ = command.reply.send(Err(format!("send eth request: {err}")));
                    fail_waiters(&mut waiters, format!("send eth request: {err}"));
                    return;
                }
                if let Err(err) = stream.flush().await {
                    let _ = command.reply.send(Err(format!("flush eth request: {err}")));
                    fail_waiters(&mut waiters, format!("flush eth request: {err}"));
                    return;
                }
                waiters.insert(
                    request_id,
                    Waiter {
                        started: Instant::now(),
                        reply: command.reply,
                    },
                );
            }
            () = tokio::time::sleep(Duration::from_secs(1)) => {
                if let Err(err) = stream.flush().await {
                    fail_waiters(&mut waiters, format!("flush session: {err}"));
                    return;
                }
            }
        }
    }
}

fn sweep_waiters(waiters: &mut HashMap<u64, Waiter>) {
    let expired: Vec<u64> = waiters
        .iter()
        .filter(|(_, waiter)| waiter.started.elapsed() > REQUEST_TIMEOUT)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        if let Some(waiter) = waiters.remove(&id) {
            let _ = waiter.reply.send(Err("eth response timed out".to_string()));
        }
    }
}

fn fail_waiters(waiters: &mut HashMap<u64, Waiter>, err: String) {
    for (_, waiter) in waiters.drain() {
        let _ = waiter.reply.send(Err(err.clone()));
    }
}

/// Highest eth version both sides offered, preferring 70 then 69 then 68.
pub fn shared_eth_version(hello: &HelloMessage) -> Option<EthVersion> {
    OFFERED_ETH
        .into_iter()
        .find(|want| hello.capabilities.iter().any(|cap| is_eth(cap, *want)))
}

fn is_eth(cap: &Capability, version: EthVersion) -> bool {
    cap.name.as_ref() == "eth" && cap.version == version as usize
}

fn status_head() -> Head {
    static HEAD: OnceLock<Head> = OnceLock::new();
    *HEAD.get_or_init(resolve_status_head)
}

fn resolve_status_head() -> Head {
    // Block-number forks are all behind the current tip. The timestamp selects
    // the post-merge fork the network is on right now. The number and hash come
    // from a public RPC so the status we advertise is a real recent block.
    // Peers with a full inbound table drop a session whose head is far behind.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    if let Some((number, hash)) = tip_from_public_rpc() {
        info!(number, %hash, "using public tip for status");
        return Head {
            number,
            hash,
            difficulty: U256::ZERO,
            total_difficulty: U256::from(1_u64),
            timestamp,
        };
    }
    warn!("public tip lookup failed; advertising block 26000000");
    Head {
        number: 26_000_000,
        hash: MAINNET.genesis_hash(),
        difficulty: U256::ZERO,
        total_difficulty: U256::from(1_u64),
        timestamp,
    }
}

fn tip_from_public_rpc() -> Option<(u64, B256)> {
    const ENDPOINTS: [&str; 2] = [
        "https://ethereum-rpc.publicnode.com",
        "https://eth.drpc.org",
    ];
    for endpoint in ENDPOINTS {
        let Ok(output) = std::process::Command::new("curl")
            .args([
                "-fsS",
                "--max-time",
                "8",
                "-H",
                "content-type: application/json",
                "-d",
                r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}"#,
                endpoint,
            ])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(body) = String::from_utf8(output.stdout) else {
            continue;
        };
        let Some(number) = json_string_field(&body, "number") else {
            continue;
        };
        let Some(hash) = json_string_field(&body, "hash") else {
            continue;
        };
        let Ok(number) = u64::from_str_radix(number.trim_start_matches("0x"), 16) else {
            continue;
        };
        let Ok(hash) = hash.parse() else {
            continue;
        };
        return Some((number, hash));
    }
    None
}

fn json_string_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":\"");
    let mut rest = body;
    while let Some(pos) = rest.find(&pattern) {
        let at_boundary = pos == 0 || !rest.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if at_boundary {
            let tail = rest.get(pos + pattern.len()..)?;
            let end = tail.find('"')?;
            return Some(&tail[..end]);
        }
        rest = rest.get(pos + 1..)?;
    }
    None
}

fn mainnet_status(version: EthVersion) -> UnifiedStatus {
    let head = status_head();
    UnifiedStatus {
        version,
        chain: MAINNET.chain,
        genesis: MAINNET.genesis_hash(),
        forkid: MAINNET.fork_id(&head),
        blockhash: head.hash,
        total_difficulty: Some(U256::from(1_u64)),
        earliest_block: Some(head.number.saturating_sub(128)),
        latest_block: Some(head.number),
    }
}

fn request_id_of(message: &EthMessage) -> Option<u64> {
    match message {
        EthMessage::GetBlockHeaders(pair) => Some(pair.request_id),
        EthMessage::GetBlockBodies(pair) => Some(pair.request_id),
        EthMessage::GetReceipts(pair) => Some(pair.request_id),
        EthMessage::GetReceipts70(pair) => Some(pair.request_id),
        _ => None,
    }
}

fn response_id(message: &EthMessage) -> Option<u64> {
    match message {
        EthMessage::BlockHeaders(pair) => Some(pair.request_id),
        EthMessage::BlockBodies(pair) => Some(pair.request_id),
        EthMessage::Receipts(pair) => Some(pair.request_id),
        EthMessage::Receipts69(pair) => Some(pair.request_id),
        EthMessage::Receipts70(pair) => Some(pair.request_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_eth_wire::Capability;

    fn hello(versions: &[usize]) -> HelloMessage {
        HelloMessage {
            protocol_version: Default::default(),
            client_version: "test".to_string(),
            capabilities: versions
                .iter()
                .copied()
                .map(|version| Capability::new_static("eth", version))
                .collect(),
            port: 30303,
            id: PeerId::ZERO,
        }
    }

    #[test]
    fn picks_highest_shared_eth_version() {
        assert_eq!(
            shared_eth_version(&hello(&[66, 68, 69])),
            Some(EthVersion::Eth69)
        );
        assert_eq!(
            shared_eth_version(&hello(&[70, 69])),
            Some(EthVersion::Eth70)
        );
        assert_eq!(
            shared_eth_version(&hello(&[72, 69])),
            Some(EthVersion::Eth72)
        );
        assert_eq!(shared_eth_version(&hello(&[67])), None);
    }

    #[test]
    fn json_field_does_not_match_a_longer_key() {
        let body = r#"{"parentHash":"0xabc","hash":"0xdef","number":"0x10"}"#;
        assert_eq!(json_string_field(body, "hash"), Some("0xdef"));
        assert_eq!(json_string_field(body, "number"), Some("0x10"));
    }
}
