//! Command line: services, pipelines, kinds, and the bind addresses.

use clap::{Parser, Subcommand};
use eth_p2p::Egress;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "ratatoskr",
    about = "Own the selected files and run the pipelines in one process."
)]
pub(crate) struct Cli {
    /// Directory of yaml files. Default is `config` in the working directory.
    #[arg(long, global = true, default_value = "config")]
    pub(crate) config_dir: PathBuf,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
    #[command(flatten)]
    pub(crate) args: Args,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Ask the running peer service. This does not dial.
    Peer(crate::peer_cmd::PeerArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Comma-separated file owners: `db`, `peer`. Omitted uses the loaded config.
    #[arg(long)]
    pub(crate) services: Option<String>,
    /// Comma-separated pipelines: `headers`, `venues`, `state`, `oracles`, `spots`.
    /// Omitted uses the loaded config. `oracles` writes `feeds.redb` and does not open the venue catalog.
    /// `spots` reads that catalog and writes `spots.redb`.
    #[arg(long)]
    pub(crate) pipeline: Option<String>,
    /// Catalog directory for this process. Default is the live catalog.
    #[arg(long)]
    pub(crate) data_dir: Option<PathBuf>,
    /// Copy a stratified identity sample from this redb file into `--data-dir`.
    #[arg(long)]
    pub(crate) copy_from: Option<PathBuf>,
    /// Identities to copy with `--copy-from`. Omitted uses the loaded config.
    #[arg(long)]
    pub(crate) sample_size: Option<u64>,
    /// Delete pools whose snapshot cannot price a positive input, then exit.
    /// A pool with no snapshot yet is kept. Combine with `--fill`.
    #[arg(long)]
    pub(crate) drop_empty: bool,
    /// Copy this many identities that are not already in `--data-dir`.
    /// Requires `--copy-from`. Kinds that are already fully copied are skipped.
    #[arg(long)]
    pub(crate) fill: Option<u64>,
    /// After `--drop-empty`, delete a stride of ids listed in `fill-added.txt`
    /// until the catalog has this many identities.
    #[arg(long)]
    pub(crate) trim_to: Option<u64>,
    /// Block tag for state reads. Default is the header cursor, or the RPC head.
    #[arg(long)]
    pub(crate) pin_block: Option<u64>,
    /// Exit the state pipeline once nothing is left to claim.
    #[arg(long)]
    pub(crate) until_idle: bool,
    /// Print stored state counts for `--data-dir` and exit.
    #[arg(long)]
    pub(crate) summarize: bool,
    /// Listen address for the status exporter. Omitted uses the loaded config.
    #[arg(long)]
    pub(crate) metrics: Option<String>,
    /// Draw the status TUI from the exporter and exit when it closes.
    #[arg(long)]
    pub(crate) status: bool,
    /// Exporter base URL for `--status`. Omitted uses the loaded config.
    #[arg(long)]
    pub(crate) status_url: Option<String>,
    /// Rewrite every stored curve from the current fitter, then exit.
    #[arg(long)]
    pub(crate) refit: bool,
    /// Re-read these catalog kinds through the detail lens, store each curve, rebuild
    /// the conduit jobs those writes queued, then exit. Kinds are comma-separated numbers.
    #[arg(long)]
    pub(crate) refresh_kinds: Option<String>,
    /// JSON-RPC URL for venue lists. Repeat to rotate.
    #[arg(long)]
    pub(crate) rpc: Vec<String>,
    /// Run only venue jobs whose name contains this string.
    #[arg(long)]
    pub(crate) only: Option<String>,
    /// Do not fold the local corpus into the venue catalog.
    #[arg(long)]
    pub(crate) no_corpus: bool,
    /// Bind IPv4 RLPx to this address. Policy routing sends that source into the tunnel.
    #[arg(long)]
    pub(crate) bind4: Option<Ipv4Addr>,
    /// Bind IPv6 RLPx to this address.
    #[arg(long)]
    pub(crate) bind6: Option<Ipv6Addr>,
    /// How many outbound sessions the peer service keeps. Omitted uses the loaded config.
    #[arg(long)]
    pub(crate) peers: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Services {
    pub(crate) db: bool,
    pub(crate) peer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Pipelines {
    pub(crate) headers: bool,
    pub(crate) venues: bool,
    /// Pool details, ticks, curves, and the watch.
    pub(crate) state: bool,
    /// Chainlink feed directory, proxy configuration, and phase jobs.
    pub(crate) oracles: bool,
    /// Sized round trips through pair conduits. Writes `spots.redb`.
    pub(crate) spots: bool,
}

pub(crate) fn bind_addr<T>(cli: Option<T>, configured: Option<&str>) -> eyre::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    if let Some(addr) = cli {
        return Ok(Some(addr));
    }
    let Some(text) = configured else {
        return Ok(None);
    };
    text.parse()
        .map(Some)
        .map_err(|err| eyre::eyre!("bind address {text}: {err}"))
}

pub(crate) fn egress_from(
    bind4: Option<Ipv4Addr>,
    bind6: Option<Ipv6Addr>,
) -> eyre::Result<Option<Egress>> {
    match (bind4, bind6) {
        (Some(v4), Some(v6)) => Ok(Some(Egress { v4, v6 })),
        (None, None) => Ok(None),
        _ => Err(eyre::eyre!("pass both --bind4 and --bind6, or neither")),
    }
}

pub(crate) fn parse_kind_list(text: &str) -> eyre::Result<Vec<u8>> {
    store_model::parse_kind_list(text)
}

pub(crate) fn parse_services(text: &str) -> eyre::Result<Services> {
    let mut services = Services::default();
    if text.trim().is_empty() {
        return Err(eyre::eyre!("unknown service"));
    }
    for part in text.split(',') {
        match part.trim() {
            "db" => services.db = true,
            "peer" => services.peer = true,
            other => return Err(eyre::eyre!("unknown service {other}")),
        }
    }
    if !services.db && !services.peer {
        return Err(eyre::eyre!("unknown service"));
    }
    Ok(services)
}

pub(crate) fn parse_pipelines(text: &str) -> eyre::Result<Pipelines> {
    let mut pipelines = Pipelines::default();
    if text.trim().is_empty() {
        return Ok(pipelines);
    }
    for part in text.split(',') {
        match part.trim() {
            "headers" => pipelines.headers = true,
            "venues" => pipelines.venues = true,
            "state" => pipelines.state = true,
            "oracles" => pipelines.oracles = true,
            "spots" => pipelines.spots = true,
            other => return Err(eyre::eyre!("unknown pipeline {other}")),
        }
    }
    Ok(pipelines)
}

pub(crate) fn resolve_rpcs(
    given: &[String],
    state: bool,
    network: &config::Network,
) -> eyre::Result<Vec<String>> {
    if !given.is_empty() {
        return Ok(given.to_vec());
    }
    if state {
        let Some(key) = alchemy_key() else {
            return Err(eyre::eyre!("state needs --rpc or ALCHEMY_API_KEY"));
        };
        observability::info!("state rpc is the alchemy endpoint");
        return Ok(vec![format!("https://eth-mainnet.g.alchemy.com/v2/{key}")]);
    }
    Ok(network.call_rpcs.clone())
}

fn alchemy_key() -> Option<String> {
    if let Ok(key) = std::env::var("ALCHEMY_API_KEY") {
        let key = key.trim().trim_matches('"').to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    let text = std::fs::read_to_string(".env").ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("ALCHEMY_API_KEY=") else {
            continue;
        };
        let key = rest.trim().trim_matches('"').to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;

    fn both() -> Services {
        Services {
            db: true,
            peer: true,
        }
    }

    fn every_pipeline() -> Pipelines {
        Pipelines {
            headers: true,
            venues: true,
            state: true,
            oracles: false,
            spots: false,
        }
    }

    #[test]
    fn default_services_are_db_and_peer() {
        let args = Cli::try_parse_from(["ratatoskr"]).unwrap().args;
        assert!(args.services.is_none());
        assert!(args.peers.is_none());
        let loaded =
            config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")).unwrap();
        assert_eq!(
            parse_services(&loaded.process.services.join(",")).unwrap(),
            both()
        );
        assert_eq!(loaded.peer.target, config::Peer::default().target);
    }

    #[test]
    fn default_pipelines_are_header_sync_discovery_and_state() {
        let args = Cli::try_parse_from(["ratatoskr"]).unwrap().args;
        assert!(args.pipeline.is_none());
        let loaded =
            config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")).unwrap();
        assert_eq!(
            parse_pipelines(&loaded.process.pipelines.join(",")).unwrap(),
            every_pipeline()
        );
    }

    #[test]
    fn a_named_pipeline_list_selects_only_those_pipelines() {
        let headers = parse_pipelines("headers").unwrap();
        assert!(headers.headers);
        assert!(!headers.venues);
        assert!(!headers.state);
        assert!(!headers.oracles);
        assert!(!headers.spots);
        assert!(parse_pipelines("venues").unwrap().venues);
        assert!(parse_pipelines("state").unwrap().state);
        assert!(!parse_pipelines("headers,venues").unwrap().state);
        let oracles = parse_pipelines("oracles").unwrap();
        assert!(oracles.oracles);
        assert!(!oracles.headers);
        assert!(!oracles.venues);
        assert!(!oracles.state);
        assert!(!oracles.spots);
        let spots = parse_pipelines("spots").unwrap();
        assert!(spots.spots);
        assert!(!spots.oracles);
        assert!(!spots.headers);
        assert!(!spots.venues);
        assert!(!spots.state);
        assert!(parse_pipelines("receipts").is_err());
    }

    #[test]
    fn the_state_pipeline_invocation_has_no_subcommand() {
        let cli = Cli::try_parse_from([
            "ratatoskr",
            "--data-dir",
            "data/ratatoskr",
            "--services",
            "db",
            "--pipeline",
            "state",
            "--until-idle",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert!(cli.args.until_idle);
        assert_eq!(cli.args.services.as_deref(), Some("db"));
        assert_eq!(cli.args.pipeline.as_deref(), Some("state"));
    }

    #[test]
    fn help_names_one_entrypoint() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        cmd.write_long_help(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("peer"), "{text}");
        assert!(!text.contains("era"), "{text}");
        assert!(!text.contains("univ3"), "{text}");
        assert!(!text.contains("spike"), "{text}");
        assert!(!text.contains("ratatoskr-"), "{text}");
    }
}
