//! Process settings.
//!
//! [`load`] reads every yaml file in a directory and merges them onto these
//! defaults. A directory with no yaml file is an error. A `*.local.yaml` file
//! merges after the shared files, so it overrides them. The binaries pass the
//! loaded value into the domain crates. A field that is not in a yaml file
//! keeps the default written on its struct.

use eyre::WrapErr;
use figment::providers::{Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MIB: usize = 1024 * 1024;

/// Every domain's settings, after the yaml files have been merged.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub process: Process,
    pub peer: Peer,
    pub network: Network,
    pub store: Store,
    pub pipeline: Pipeline,
    pub venues: Venues,
    pub spots: Spots,
    pub oracles: Oracles,
    pub simulate: Simulate,
}

/// Which files this process owns, which pipelines it runs, and where it listens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub data_dir: String,
    pub services: Vec<String>,
    pub pipelines: Vec<String>,
    pub log: String,
    pub metrics: String,
    pub status_url: String,
    pub sample_size: u64,
    pub rate_window_secs: u64,
    pub sample_interval_secs: u64,
    pub slow_sample_ms: u64,
    pub miss_warn: u64,
    pub cache_log_secs: u64,
    pub warn_gap_secs: u64,
}

impl Default for Process {
    fn default() -> Self {
        Self {
            data_dir: "data/ratatoskr".into(),
            services: vec!["db".into(), "peer".into()],
            pipelines: vec!["headers".into(), "venues".into(), "state".into()],
            log: "ratatoskr=info,store=info,pipeline=info,peer=info,eth_p2p=info,venues=info,headers=info,oracles=info,spots=info".into(),
            metrics: "127.0.0.1:9737".into(),
            status_url: "http://127.0.0.1:9737".into(),
            sample_size: 10_000,
            rate_window_secs: 10,
            sample_interval_secs: 1,
            slow_sample_ms: 50,
            miss_warn: 4_096,
            cache_log_secs: 60,
            warn_gap_secs: 10,
        }
    }
}

impl Process {
    pub fn rate_window(&self) -> Duration {
        Duration::from_secs(self.rate_window_secs)
    }

    pub fn sample_interval(&self) -> Duration {
        Duration::from_secs(self.sample_interval_secs)
    }

    pub fn warn_gap(&self) -> Duration {
        Duration::from_secs(self.warn_gap_secs)
    }
}

/// Outbound sessions, the node key, and the local peer socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub target: usize,
    /// Secp256k1 node secret, as hex. A `*.local.yaml` file sets this.
    pub key: String,
    /// Absent means `<data_dir>/peer.sock`.
    pub socket: Option<String>,
    pub bind4: Option<String>,
    pub bind6: Option<String>,
    pub redial_gap_secs: u64,
    pub dial_budget_secs: u64,
    pub in_flight: usize,
    pub dials_per_sec: usize,
    pub directory_size: usize,
    pub chunks_per_peer: usize,
    pub directory_refresh_secs: u64,
}

impl Default for Peer {
    fn default() -> Self {
        Self {
            target: 16,
            key: String::new(),
            socket: None,
            bind4: None,
            bind6: None,
            redial_gap_secs: 600,
            dial_budget_secs: 12,
            in_flight: 16,
            dials_per_sec: 20,
            directory_size: 2500,
            chunks_per_peer: 2,
            directory_refresh_secs: 600,
        }
    }
}

impl Peer {
    pub fn redial_gap(&self) -> Duration {
        Duration::from_secs(self.redial_gap_secs)
    }

    pub fn dial_budget(&self) -> Duration {
        Duration::from_secs(self.dial_budget_secs)
    }

    pub fn directory_refresh(&self) -> Duration {
        Duration::from_secs(self.directory_refresh_secs)
    }
}

/// JSON-RPC, DNS discovery trees, and devp2p timeouts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub call_rpcs: Vec<String>,
    pub dns_all: String,
    pub dns_snap: String,
    pub egress_ip_url: String,
    pub dial_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub dns_budget_secs: u64,
    pub discv4_budget_secs: u64,
    pub wanted_peers: usize,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            call_rpcs: vec![
                "https://ethereum-rpc.publicnode.com".into(),
                "https://eth.drpc.org".into(),
                "https://rpc.mevblocker.io".into(),
                "https://rpc.flashbots.net".into(),
            ],
            dns_all: "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@all.mainnet.ethdisco.net".into(),
            dns_snap: "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@snap.mainnet.ethdisco.net".into(),
            egress_ip_url: "https://ipv4.icanhazip.com".into(),
            dial_timeout_secs: 5,
            request_timeout_secs: 45,
            dns_budget_secs: 75,
            discv4_budget_secs: 12,
            wanted_peers: 600,
        }
    }
}

impl Network {
    pub fn dial_timeout(&self) -> Duration {
        Duration::from_secs(self.dial_timeout_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    pub fn dns_budget(&self) -> Duration {
        Duration::from_secs(self.dns_budget_secs)
    }

    pub fn discv4_budget(&self) -> Duration {
        Duration::from_secs(self.discv4_budget_secs)
    }
}

/// Page caches and the read sizes of the header chain and the receipt corpus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Store {
    /// A header window is a few megabytes, and the next append reads one parent.
    pub headers: usize,
    /// Receipt batches are multi-megabyte sequential reads and writes.
    pub corpus: usize,
    pub discovery: usize,
    pub entities: usize,
    pub work: usize,
    pub read: usize,
    /// Summaries load once. Later writes are single sightings.
    pub peers: usize,
    /// Feed rows are small. The cache holds the directory and the latest answers.
    pub feeds: usize,
    /// One pass replaces the table. The cache holds that write.
    pub spots: usize,
    /// The histogram write is one pass over the pools gathered in memory.
    pub pools: usize,
    pub header_batch: u64,
    pub block_page: usize,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            headers: 64 * MIB,
            corpus: 64 * MIB,
            discovery: 128 * MIB,
            entities: 256 * MIB,
            work: 16 * MIB,
            read: 64 * MIB,
            peers: 16 * MIB,
            feeds: 8 * MIB,
            spots: 8 * MIB,
            pools: 16 * MIB,
            header_batch: 8192,
            block_page: 8 * MIB,
        }
    }
}

/// Header sync and the idle time between discovery passes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub follow_secs: u64,
    pub chunk: u64,
    pub width: usize,
    pub chunks_per_peer: usize,
    pub tip_lag: u64,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            follow_secs: 60,
            chunk: 512,
            width: 16,
            chunks_per_peer: 2,
            tip_lag: 32,
        }
    }
}

impl Pipeline {
    pub fn follow(&self) -> Duration {
        Duration::from_secs(self.follow_secs)
    }
}

/// Era1 receipt fetch and the state pipeline's claim size.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Venues {
    pub era_mirror: String,
    pub chunk_max: u64,
    pub receipt_pipeline: usize,
    pub era_inflight: usize,
    pub era_batch: usize,
    pub commit_spans: usize,
    pub scan_flush: u64,
    pub storage_chunk: usize,
    pub detail_batch: u32,
    pub lease_secs: u64,
    pub retry_secs: u64,
}

impl Default for Venues {
    fn default() -> Self {
        Self {
            era_mirror: "https://data.ethpandaops.io/era1/mainnet/".into(),
            chunk_max: 64,
            receipt_pipeline: 8,
            era_inflight: 32,
            era_batch: 128,
            commit_spans: 32,
            scan_flush: 1_000_000,
            storage_chunk: 1024,
            detail_batch: 16,
            lease_secs: 120,
            retry_secs: 30,
        }
    }
}

/// Hub ranking for a spot pass, and how long the pass waits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Spots {
    pub hub_floor: usize,
    pub hub_limit: usize,
    pub pass_secs: u64,
}

impl Default for Spots {
    fn default() -> Self {
        Self {
            hub_floor: 3,
            hub_limit: 32,
            pass_secs: 60,
        }
    }
}

impl Spots {
    pub fn pass(&self) -> Duration {
        Duration::from_secs(self.pass_secs)
    }
}

/// Chainlink feed directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Oracles {
    pub directory_url: String,
    pub directory_timeout_secs: u64,
}

impl Default for Oracles {
    fn default() -> Self {
        Self {
            directory_url: "https://reference-data-directory.vercel.app/feeds-mainnet.json".into(),
            directory_timeout_secs: 60,
        }
    }
}

impl Oracles {
    pub fn directory_timeout(&self) -> Duration {
        Duration::from_secs(self.directory_timeout_secs)
    }
}

/// Log filter for the simulate binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Simulate {
    pub log: String,
}

impl Default for Simulate {
    fn default() -> Self {
        Self {
            log: "survey::measure=info".into(),
        }
    }
}

/// Merge every yaml file in `dir` onto [`Config::default`].
///
/// `dir` must be a directory and must contain at least one `.yaml` or `.yml`
/// file. A file that fails to parse fails the load. Shared files merge in
/// name order, then each `*.local.yaml` file merges in name order.
pub fn load(dir: &Path) -> eyre::Result<Config> {
    if !dir.is_dir() {
        return Err(eyre::eyre!("config directory {} is missing", dir.display()));
    }
    let mut shared = Vec::new();
    let mut local = Vec::new();
    for entry in fs::read_dir(dir).wrap_err_with(|| format!("read {}", dir.display()))? {
        let path = entry.wrap_err("read config directory")?.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        if is_local_yaml(&path) {
            local.push(path);
        } else {
            shared.push(path);
        }
    }
    if shared.is_empty() && local.is_empty() {
        return Err(eyre::eyre!(
            "config directory {} has no config file",
            dir.display()
        ));
    }
    shared.sort();
    local.sort();
    let mut figment = Figment::from(Serialized::defaults(Config::default()));
    for file in shared.iter().chain(&local) {
        figment = figment.merge(Yaml::file(file));
    }
    figment
        .extract()
        .wrap_err_with(|| format!("parse config in {}", dir.display()))
}

fn is_local_yaml(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".local.yaml"))
}

/// Expand a leading `~/` with the `HOME` directory.
pub fn expand_home(path: &str) -> eyre::Result<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").wrap_err("HOME")?;
        return Ok(PathBuf::from(home).join(rest));
    }
    if path == "~" {
        let home = std::env::var("HOME").wrap_err("HOME")?;
        return Ok(PathBuf::from(home));
    }
    Ok(PathBuf::from(path))
}

/// The peer socket. An unset path is `peer.sock` inside `data_dir`.
pub fn peer_socket(data_dir: &Path, socket: &Option<String>) -> eyre::Result<PathBuf> {
    match socket {
        Some(path) => expand_home(path),
        None => Ok(data_dir.join("peer.sock")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-config-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let err = load(Path::new("/tmp/ratatoskr-config-missing-dir")).unwrap_err();
        assert!(err.to_string().contains("is missing"), "{err}");
    }

    #[test]
    fn an_empty_directory_is_an_error() {
        let dir = TempDir::new();
        let err = load(&dir.0).unwrap_err();
        assert!(err.to_string().contains("has no config file"), "{err}");
    }

    #[test]
    fn yaml_files_merge_onto_the_schema_defaults() {
        let dir = TempDir::new();
        fs::write(dir.0.join("process.yaml"), "process:\n  sample_size: 7\n").unwrap();
        fs::write(dir.0.join("peer.yaml"), "peer:\n  target: 3\n").unwrap();
        let loaded = load(&dir.0).unwrap();
        assert_eq!(loaded.process.sample_size, 7);
        assert_eq!(loaded.peer.target, 3);
        assert_eq!(loaded.process.data_dir, Process::default().data_dir);
        assert_eq!(loaded.store, Store::default());
        assert_eq!(loaded.network.call_rpcs, Network::default().call_rpcs);
    }

    #[test]
    fn a_local_file_overrides_the_shared_file() {
        let dir = TempDir::new();
        fs::write(
            dir.0.join("peer.yaml"),
            "peer:\n  target: 3\n  key: shared\n",
        )
        .unwrap();
        fs::write(dir.0.join("peer.local.yaml"), "peer:\n  key: local\n").unwrap();
        let loaded = load(&dir.0).unwrap();
        assert_eq!(loaded.peer.target, 3);
        assert_eq!(loaded.peer.key, "local");
    }

    #[test]
    fn an_unknown_field_is_an_error() {
        let dir = TempDir::new();
        fs::write(dir.0.join("process.yaml"), "process:\n  nope: 1\n").unwrap();
        assert!(load(&dir.0).is_err());
    }

    #[test]
    fn the_shipped_directory_loads() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.process.data_dir, "data/ratatoskr");
        assert_eq!(loaded.process.services, ["db", "peer"]);
        assert_eq!(loaded.process.pipelines, ["headers", "venues", "state"]);
        assert_eq!(loaded.peer.target, 16);
        let key = loaded.peer.key.trim();
        assert!(
            key.is_empty() || (key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "peer.key length {}",
            key.len()
        );
        assert_eq!(loaded.spots.hub_floor, 3);
        assert_eq!(loaded.spots.hub_limit, 32);
        assert!(loaded.peer.socket.is_none());
        assert!(loaded.peer.bind4.is_none());
    }
}
