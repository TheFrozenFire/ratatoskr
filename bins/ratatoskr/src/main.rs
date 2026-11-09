//! Own the selected databases and run pipelines in-process.

use clap::Parser;
use eth_p2p::Egress;
use eyre::WrapErr;
use peer::LocalPeer;
use pipeline::{HeaderSource, ReceiptSource, Source, StoreSink, VenueSource};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use store::LocalStore;
use tokio::sync::watch;
use tokio::task::JoinSet;
use venues::{RunOpts, RunPhase};

mod export;
mod status;

const DEFAULT_TARGET_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_LOG: &str =
    "ratatoskr=info,store=info,pipeline=info,peer=info,eth_p2p=info,venues=info,headers=info";

#[derive(Debug, Parser)]
#[command(
    name = "ratatoskr",
    about = "Run the selected file owners and pipelines. ratatoskr-spike must not be run while this peer service is up."
)]
struct Args {
    /// Comma-separated file owners: `db`, `peer`. Default is `db,peer`.
    #[arg(long, default_value = "db,peer")]
    services: String,
    /// Comma-separated pipelines: `headers`, `venues`, `receipts`, `state`.
    /// Default is `headers,venues`. `receipts` and `state` run only when named.
    /// `state` requires `--data-dir` and does not open the live catalog.
    #[arg(long, default_value = "headers,venues")]
    pipeline: String,
    /// Catalog directory for this process. Required for `state`.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Copy a stratified identity sample from this redb file into `--data-dir`.
    #[arg(long)]
    copy_from: Option<PathBuf>,
    /// Identities to copy with `--copy-from`. Default is 10000.
    #[arg(long, default_value_t = 10_000)]
    sample_size: u64,
    /// Delete pools whose snapshot cannot price a positive input, then exit.
    /// A pool with no snapshot yet is kept. Combine with `--fill`.
    #[arg(long)]
    drop_empty: bool,
    /// Copy this many identities that are not already in `--data-dir`.
    /// Requires `--copy-from`. Kinds that are already fully copied are skipped.
    #[arg(long)]
    fill: Option<u64>,
    /// After `--drop-empty`, delete a stride of ids listed in `fill-added.txt`
    /// until the catalog has this many identities.
    #[arg(long)]
    trim_to: Option<u64>,
    /// Block tag for state reads. Default is the header cursor, or the RPC head.
    #[arg(long)]
    pin_block: Option<u64>,
    /// Exit the state pipeline once nothing is left to claim.
    #[arg(long)]
    until_idle: bool,
    /// Print stored state counts for `--data-dir` and exit.
    #[arg(long)]
    summarize: bool,
    /// Listen address for the status exporter.
    #[arg(long, default_value = "127.0.0.1:9737")]
    metrics: String,
    /// Draw the status TUI from the exporter and exit when it closes.
    #[arg(long)]
    status: bool,
    /// Exporter base URL for `--status`.
    #[arg(long, default_value = "http://127.0.0.1:9737")]
    status_url: String,
    /// Rewrite every stored curve from the current fitter, then exit.
    /// Combine with `--export-fit` to write the extract after the rewrite.
    #[arg(long)]
    refit: bool,
    /// Copy stored quote samples for `--data-dir` into a JSONL file and exit.
    #[arg(long)]
    export_fit: Option<PathBuf>,
    /// Load chain quote samples from a JSONL extract into `--data-dir` and exit.
    #[arg(long)]
    import_fit: Option<PathBuf>,
    /// Delete quote rows that stopped on a transport or decode failure, then exit.
    #[arg(long)]
    drop_partial_quotes: bool,
    /// JSON-RPC URL for venue lists. Repeat to rotate.
    #[arg(long)]
    rpc: Vec<String>,
    /// Run only venue jobs whose name contains this string.
    #[arg(long)]
    only: Option<String>,
    /// Do not fold the local corpus into the venue catalog.
    #[arg(long)]
    no_corpus: bool,
    /// Stop receipt ingest after this many stored bytes. Default is 10GiB.
    #[arg(long, default_value_t = DEFAULT_TARGET_BYTES)]
    target_bytes: u64,
    /// Bind IPv4 RLPx to this address. Policy routing sends that source into the tunnel.
    #[arg(long)]
    bind4: Option<Ipv4Addr>,
    /// Bind IPv6 RLPx to this address.
    #[arg(long)]
    bind6: Option<Ipv6Addr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Services {
    db: bool,
    peer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Pipelines {
    headers: bool,
    venues: bool,
    receipts: bool,
    /// Pool details, ticks, curves, and the watch. Off unless named.
    state: bool,
}

struct Paths {
    peers: PathBuf,
    headers: PathBuf,
    corpus: PathBuf,
    venues: PathBuf,
    peer_socket: PathBuf,
    db_socket: PathBuf,
}

struct Owned {
    peer: Option<peer::OpenedPeer>,
    store: Option<LocalStore>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .init();
    supervise(Args::parse()).await
}

async fn supervise(args: Args) -> eyre::Result<()> {
    if args.status {
        let url = args.status_url.clone();
        return tokio::task::spawn_blocking(move || status::run(&url))
            .await
            .wrap_err("status task")?;
    }
    export::serve(&args.metrics)?;
    let services = parse_services(&args.services)?;
    let pipelines = parse_pipelines(&args.pipeline)?;
    let egress = egress_from(args.bind4, args.bind6)?;
    if args.refit && args.data_dir.is_none() {
        return Err(eyre::eyre!("--refit needs --data-dir"));
    }
    if args.export_fit.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--export-fit needs --data-dir"));
    }
    if args.import_fit.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--import-fit needs --data-dir"));
    }
    if args.drop_partial_quotes && args.data_dir.is_none() {
        return Err(eyre::eyre!("--drop-partial-quotes needs --data-dir"));
    }
    let paths = paths_from(args.data_dir.as_deref())?;
    if args.drop_partial_quotes {
        let venues_path = paths.venues.clone();
        let dropped = tokio::task::spawn_blocking(move || {
            let catalog = venues::Catalog::open(&venues_path)?;
            venues::drop_partial_quotes(&catalog)
        })
        .await
        .wrap_err("drop partial quotes")??;
        println!("dropped {dropped}");
        return Ok(());
    }
    if let Some(path) = args.import_fit.clone() {
        let venues_path = paths.venues.clone();
        let report = tokio::task::spawn_blocking(move || {
            let catalog = venues::Catalog::open(&venues_path)?;
            venues::import_curve_fit(&catalog, &path)
        })
        .await
        .wrap_err("fit import task")??;
        println!("pools {}", report.pools);
        println!("resumed {}", report.resumed);
        println!("chain_ok {}", report.chain_ok);
        println!("chain_fail {}", report.chain_fail);
        println!("chain_absent {}", report.chain_absent);
        if args.export_fit.is_none() {
            return Ok(());
        }
    }
    if args.refit {
        let venues_path = paths.venues.clone();
        let wrote = tokio::task::spawn_blocking(move || {
            let catalog = venues::Catalog::open(&venues_path)?;
            venues::refit_curves(&catalog)
        })
        .await
        .wrap_err("refit task")??;
        println!("curves {wrote}");
        if args.export_fit.is_none() {
            return Ok(());
        }
    }
    if let Some(out) = args.export_fit.clone() {
        let venues_path = paths.venues.clone();
        let report = tokio::task::spawn_blocking(move || {
            let catalog = venues::Catalog::open(&venues_path)?;
            venues::export_curve_fit(&catalog, &out)
        })
        .await
        .wrap_err("fit export task")??;
        println!("pools {}", report.pools);
        println!("resumed {}", report.resumed);
        println!("chain_ok {}", report.chain_ok);
        println!("chain_fail {}", report.chain_fail);
        println!("chain_absent {}", report.chain_absent);
        return Ok(());
    }
    if args.drop_empty && args.data_dir.is_none() {
        return Err(eyre::eyre!("--drop-empty needs --data-dir"));
    }
    if args.fill.is_some() && args.copy_from.is_none() {
        return Err(eyre::eyre!("--fill needs --copy-from"));
    }
    if args.trim_to.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--trim-to needs --data-dir"));
    }
    if args.drop_empty || args.fill.is_some() || args.trim_to.is_some() {
        let sample_dir = paths
            .venues
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if args.drop_empty {
            let venues_path = paths.venues.clone();
            let dropped = tokio::task::spawn_blocking(move || {
                let catalog = venues::Catalog::open(&venues_path)?;
                catalog.drop_empty_pools()
            })
            .await
            .wrap_err("drop empty")??;
            venues::append_dropped_ids(&sample_dir, &dropped)?;
            println!("dropped_empty {}", dropped.len());
        }
        if let Some(add) = args.fill {
            let src = args.copy_from.clone().expect("checked");
            let venues_path = paths.venues.clone();
            let report = tokio::task::spawn_blocking(move || {
                venues::fill_identities(&src, &venues_path, add)
            })
            .await
            .wrap_err("fill identities")??;
            venues::append_fill_ids(&sample_dir, &report.ids)?;
            println!("filled {}", report.copied);
            for (kind, count) in &report.by_kind {
                println!("filled {} {count}", venues::kind_name(*kind));
            }
        }
        if let Some(target) = args.trim_to {
            let venues_path = paths.venues.clone();
            let listed = venues::read_fill_ids(&sample_dir)?;
            let (removed, left) = tokio::task::spawn_blocking(move || {
                let catalog = venues::Catalog::open(&venues_path)?;
                catalog.trim_listed(target, &listed)
            })
            .await
            .wrap_err("trim sample")??;
            venues::write_fill_ids(&sample_dir, &left)?;
            println!("trimmed {removed}");
        }
        return Ok(());
    }
    check_combination(&services, &pipelines)?;
    if pipelines.state && args.data_dir.is_none() {
        return Err(eyre::eyre!(
            "state needs --data-dir and does not open the live catalog"
        ));
    }
    if args.summarize && args.data_dir.is_none() {
        return Err(eyre::eyre!("--summarize needs --data-dir"));
    }
    if args.copy_from.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--copy-from needs --data-dir"));
    }
    if let Some(src) = &args.copy_from {
        let report = venues::sample_identities(src, &paths.venues, args.sample_size)?;
        tracing::info!(
            copied = report.copied,
            anchor = report.anchor,
            "identity sample"
        );
        for (kind, count) in &report.by_kind {
            tracing::info!(kind = venues::kind_name(*kind), count, "sample kind");
        }
        if !report.anchor {
            tracing::warn!("sample did not include the USDC/WETH 0.3% pool");
        }
    }
    if args.summarize {
        let catalog = venues::Catalog::open(&paths.venues)?;
        let summary = catalog.state_summary()?;
        println!("identities {}", summary.identities);
        println!("immutable {}", summary.immutable);
        println!("mutable {}", summary.mutable);
        println!("curves {}", summary.curves);
        println!("quotes {}", summary.quotes);
        println!("tick_pages {}", summary.tick_pages);
        println!("mismatches {}", summary.mismatches);
        println!("jobs {}", summary.jobs);
        for (stage, count) in &summary.jobs_by_stage {
            println!("jobs stage {stage} {count}");
        }
        for (error, count) in &summary.errors {
            println!("error {count} {error}");
        }
        return Ok(());
    }
    let rpcs = resolve_rpcs(&args.rpc, pipelines.state)?;
    let mut owned = open_owned(&services, &pipelines, &paths)?;
    let local = owned.peer.as_ref().map(peer::OpenedPeer::local);
    let prepared = match owned.peer.take() {
        Some(opened) => Some(peer::prepare(
            opened,
            &peer::PeerConfig {
                peers: peer::TARGET_PEERS,
                socket: paths.peer_socket.clone(),
                catalog: paths.peers.clone(),
                egress,
            },
        )?),
        None => None,
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let signal_tx = stop_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = shutdown_signal().await {
            tracing::error!(%err, "shutdown signal");
        }
        let _ = signal_tx.send(true);
    });
    let peer_task = prepared.map(|prepared| {
        let stop = stop_rx.clone();
        tokio::spawn(async move { peer::run(prepared, stop).await })
    });
    let store = owned.store.clone();
    let mut db_cancel = None;
    let mut db_result = None;
    let db_thread = owned.store.take().map(|store| {
        let socket = paths.db_socket.clone();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        db_cancel = Some(cancel_tx);
        db_result = Some(result_rx);
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .wrap_err("db runtime")?;
                runtime.block_on(async move {
                    tokio::select! {
                        result = store::serve(Arc::new(store), &socket) => result,
                        _ = cancel_rx => Ok(()),
                    }
                })
            })();
            let _ = result_tx.send(result);
        })
    });
    let opts = RunOpts {
        rpcs,
        only: args.only,
        no_corpus: args.no_corpus,
        corpus_available: !args.no_corpus,
        phase: RunPhase::All,
        pin_block: args.pin_block,
        until_idle: args.until_idle,
    };
    if pipelines.state {
        if let Some(store) = store.clone() {
            spawn_sampler(store);
        }
    }
    let pipelines_started =
        pipelines.headers || pipelines.venues || pipelines.receipts || pipelines.state;
    let mut peer_task = peer_task;
    let mut stop_rx = stop_rx;
    let mut failed = tokio::task::LocalSet::new()
        .run_until(async {
            let mut tasks = spawn_pipelines(
                &pipelines,
                store.clone(),
                local,
                opts,
                args.target_bytes,
                stop_rx.clone(),
            );
            wait_for_stop(
                &mut tasks,
                &mut peer_task,
                &mut db_result,
                pipelines_started,
                &stop_tx,
                &mut stop_rx,
            )
            .await
        })
        .await;
    let _ = stop_tx.send(true);
    if let Some(task) = peer_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => note_failure(&mut failed, err),
            Err(err) => note_failure(&mut failed, eyre::eyre!("peer task: {err}")),
        }
    }
    if let Some(cancel) = db_cancel {
        let _ = cancel.send(());
    }
    if let Some(result) = db_result {
        match result.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => note_failure(&mut failed, err),
            Err(_) => note_failure(&mut failed, eyre::eyre!("db socket thread dropped")),
        }
    }
    if let Some(thread) = db_thread {
        if thread.join().is_err() {
            note_failure(&mut failed, eyre::eyre!("db socket thread panicked"));
        }
    }
    drop(store);
    drop(owned);
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn spawn_sampler(store: LocalStore) {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            if busy.swap(true, std::sync::atomic::Ordering::AcqRel) {
                continue;
            }
            let store = store.clone();
            let busy = std::sync::Arc::clone(&busy);
            tokio::task::spawn_blocking(move || {
                if let Ok(summary) = store.state_summary() {
                    venues::publish_summary(&summary);
                }
                busy.store(false, std::sync::atomic::Ordering::Release);
            });
        }
    });
}

fn spawn_pipelines(
    pipelines: &Pipelines,
    store: Option<LocalStore>,
    local: Option<LocalPeer>,
    opts: RunOpts,
    target_bytes: u64,
    stop: watch::Receiver<bool>,
) -> JoinSet<eyre::Result<()>> {
    let mut tasks = JoinSet::new();
    if pipelines.headers {
        let peer = local.clone().expect("headers needs both db and peer");
        let store = store.clone().expect("headers needs both db and peer");
        let stop = stop.clone();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            HeaderSource::new(peer, store).run(&mut sink, stop).await
        });
    }
    if pipelines.receipts {
        let peer = local.clone().expect("receipts needs both db and peer");
        let store = store.clone().expect("receipts needs both db and peer");
        let stop = stop.clone();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            ReceiptSource::new(store, peer, target_bytes)
                .run(&mut sink, stop)
                .await
        });
    }
    if pipelines.venues {
        let store = store.clone().expect("venues needs db");
        let venue_peer = local.clone();
        let venue_opts = opts.clone();
        let stop = stop.clone();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            VenueSource::new(store, venue_peer, venue_opts)
                .run(&mut sink, stop)
                .await
        });
    }
    if pipelines.state {
        let store = store.expect("state needs db");
        let stop = stop.clone();
        let mut opts = opts;
        opts.phase = RunPhase::State;
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            VenueSource::new(store, local, opts)
                .run(&mut sink, stop)
                .await
        });
    }
    tasks
}

fn note_failure(failed: &mut Option<eyre::Report>, err: eyre::Report) {
    tracing::error!(%err);
    if failed.is_none() {
        *failed = Some(err);
    }
}

async fn wait_for_stop(
    tasks: &mut JoinSet<eyre::Result<()>>,
    peer_task: &mut Option<tokio::task::JoinHandle<eyre::Result<()>>>,
    db_result: &mut Option<tokio::sync::oneshot::Receiver<eyre::Result<()>>>,
    pipelines_started: bool,
    stop_tx: &watch::Sender<bool>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Option<eyre::Report> {
    let mut failed = None;
    loop {
        if tasks.is_empty() && *stop_rx.borrow() {
            break;
        }
        if tasks.is_empty() && pipelines_started && failed.is_none() && !*stop_rx.borrow() {
            break;
        }
        if tasks.is_empty() && peer_task.is_none() && db_result.is_none() {
            break;
        }
        tokio::select! {
            biased;
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(err))) => {
                        tracing::error!(%err, "pipeline");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Some(Err(err)) => {
                        let err = eyre::eyre!("pipeline task: {err}");
                        tracing::error!(%err, "pipeline task");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    None => {}
                }
            }
            joined = async { peer_task.as_mut().unwrap().await }, if peer_task.is_some() => {
                *peer_task = None;
                match joined {
                    Ok(Ok(())) if *stop_rx.borrow() => {}
                    Ok(Ok(())) => {
                        let err = eyre::eyre!("peer stopped");
                        tracing::error!(%err, "peer");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Ok(Err(err)) => {
                        tracing::error!(%err, "peer");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Err(err) => {
                        let err = eyre::eyre!("peer maintain: {err}");
                        tracing::error!(%err, "peer task");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                }
            }
            result = async { db_result.as_mut().unwrap().await }, if db_result.is_some() => {
                *db_result = None;
                let err = match result {
                    Ok(Err(err)) => err,
                    Ok(Ok(())) => eyre::eyre!("db socket stopped"),
                    Err(_) => eyre::eyre!("db socket thread dropped"),
                };
                tracing::error!(%err, "db socket");
                if failed.is_none() {
                    failed = Some(err);
                }
                let _ = stop_tx.send(true);
            }
            changed = stop_rx.changed(), if !*stop_rx.borrow() => {
                if changed.is_err() {
                    let _ = stop_tx.send(true);
                }
            }
        }
    }
    failed
}

async fn shutdown_signal() -> eyre::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

fn egress_from(bind4: Option<Ipv4Addr>, bind6: Option<Ipv6Addr>) -> eyre::Result<Option<Egress>> {
    match (bind4, bind6) {
        (Some(v4), Some(v6)) => Ok(Some(Egress { v4, v6 })),
        (None, None) => Ok(None),
        _ => Err(eyre::eyre!("pass both --bind4 and --bind6, or neither")),
    }
}

fn parse_services(text: &str) -> eyre::Result<Services> {
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

fn parse_pipelines(text: &str) -> eyre::Result<Pipelines> {
    let mut pipelines = Pipelines::default();
    if text.trim().is_empty() {
        return Ok(pipelines);
    }
    for part in text.split(',') {
        match part.trim() {
            "headers" => pipelines.headers = true,
            "venues" => pipelines.venues = true,
            "receipts" => pipelines.receipts = true,
            "state" => pipelines.state = true,
            other => return Err(eyre::eyre!("unknown pipeline {other}")),
        }
    }
    Ok(pipelines)
}

fn check_combination(services: &Services, pipelines: &Pipelines) -> eyre::Result<()> {
    if pipelines.headers && !(services.db && services.peer) {
        return Err(eyre::eyre!("headers needs both db and peer"));
    }
    if pipelines.receipts && !(services.db && services.peer) {
        return Err(eyre::eyre!("receipts needs both db and peer"));
    }
    if pipelines.venues && !services.db {
        return Err(eyre::eyre!("venues needs db"));
    }
    if pipelines.state && !services.db {
        return Err(eyre::eyre!("state needs db"));
    }
    Ok(())
}

/// Open the selected files. `peers.redb` is first. A bad combination opens nothing.
fn open_owned(services: &Services, pipelines: &Pipelines, paths: &Paths) -> eyre::Result<Owned> {
    check_combination(services, pipelines)?;
    let peer_owner = if services.peer {
        Some(peer::open(&paths.peers)?)
    } else {
        None
    };
    let store = if services.db {
        match LocalStore::open(&paths.headers, &paths.corpus, &paths.venues) {
            Ok(store) => Some(store),
            Err(err) => {
                drop(peer_owner);
                return Err(err);
            }
        }
    } else {
        None
    };
    Ok(Owned {
        peer: peer_owner,
        store,
    })
}

fn default_paths() -> eyre::Result<Paths> {
    Ok(Paths {
        peers: directory::default_path(),
        headers: chain::HeaderChain::default_path(),
        corpus: corpus::Corpus::default_path(),
        venues: venues::Catalog::default_path(),
        peer_socket: peer::socket_path()?,
        db_socket: store::db_socket()?,
    })
}

fn paths_from(data_dir: Option<&Path>) -> eyre::Result<Paths> {
    let Some(dir) = data_dir else {
        return default_paths();
    };
    Ok(Paths {
        peers: dir.join("peers.redb"),
        headers: dir.join("headers.redb"),
        corpus: dir.join("corpus.redb"),
        venues: dir.join("entities.redb"),
        peer_socket: dir.join("peer.sock"),
        db_socket: dir.join("db.sock"),
    })
}

#[cfg(test)]
fn live_share() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}

#[cfg(test)]
fn refuse_live(dir: &Path) -> eyre::Result<()> {
    let live = live_share();
    if dir == live {
        return Err(eyre::eyre!("refusing to open the live catalog"));
    }
    if dir.exists() && live.exists() && std::fs::canonicalize(dir)? == std::fs::canonicalize(&live)?
    {
        return Err(eyre::eyre!("refusing to open the live catalog"));
    }
    Ok(())
}

fn resolve_rpcs(given: &[String], state: bool) -> eyre::Result<Vec<String>> {
    if !given.is_empty() {
        return Ok(given.to_vec());
    }
    if !state {
        return Ok(Vec::new());
    }
    let Some(key) = alchemy_key() else {
        return Err(eyre::eyre!("state needs --rpc or ALCHEMY_API_KEY"));
    };
    tracing::info!("state rpc is the alchemy endpoint");
    Ok(vec![format!("https://eth-mainnet.g.alchemy.com/v2/{key}")])
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
    use std::os::unix::fs::FileTypeExt;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn must_err(result: eyre::Result<Owned>) -> eyre::Report {
        match result {
            Err(err) => err,
            Ok(_) => panic!("startup opened a database"),
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-supervisor-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            peers: dir.join("peers.redb"),
            headers: dir.join("headers.redb"),
            corpus: dir.join("corpus.redb"),
            venues: dir.join("venues.redb"),
            peer_socket: dir.join("peer.sock"),
            db_socket: dir.join("db.sock"),
        }
    }

    fn both() -> Services {
        Services {
            db: true,
            peer: true,
        }
    }

    fn headers_venues() -> Pipelines {
        Pipelines {
            headers: true,
            venues: true,
            receipts: false,
            state: false,
        }
    }

    fn assert_no_databases(paths: &Paths) {
        assert!(!paths.peers.exists());
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        let dir = paths.venues.parent().expect("catalog dir");
        assert!(!dir.join("entities.redb").exists());
        assert!(!dir.join("discovery.redb").exists());
        assert!(!dir.join("work.redb").exists());
    }

    #[test]
    fn default_services_are_db_and_peer() {
        let args = Args::try_parse_from(["ratatoskr"]).unwrap();
        assert_eq!(parse_services(&args.services).unwrap(), both());
    }

    #[test]
    fn default_pipelines_are_headers_and_venues() {
        let args = Args::try_parse_from(["ratatoskr"]).unwrap();
        assert_eq!(parse_pipelines(&args.pipeline).unwrap(), headers_venues());
    }

    #[test]
    fn receipts_run_only_when_named() {
        let args = Args::try_parse_from(["ratatoskr"]).unwrap();
        assert!(!parse_pipelines(&args.pipeline).unwrap().receipts);
        assert!(parse_pipelines("receipts").unwrap().receipts);
        assert!(!parse_pipelines("headers,venues").unwrap().receipts);
        assert!(!parse_pipelines("headers,venues").unwrap().state);
        assert!(parse_pipelines("state").unwrap().state);
    }

    #[test]
    fn data_dir_paths_stay_inside_that_directory() {
        let dir = TempDir::new();
        let paths = paths_from(Some(&dir.0)).unwrap();
        assert_eq!(paths.venues, dir.0.join("entities.redb"));
        assert_eq!(paths.headers, dir.0.join("headers.redb"));
        assert_eq!(paths.db_socket, dir.0.join("db.sock"));
        assert!(refuse_live(&dir.0).is_ok());
    }

    #[test]
    fn state_without_db_opens_nothing() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let err = must_err(open_owned(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: false,
                receipts: false,
                state: true,
            },
            &paths,
        ));
        assert!(err.to_string().contains("state"), "{err}");
        assert_no_databases(&paths);
    }

    #[test]
    fn spike_is_not_a_pipeline() {
        let err = parse_pipelines("spike").unwrap_err();
        assert!(err.to_string().contains("spike"), "{err}");
    }

    #[test]
    fn help_says_not_to_run_spike_beside_the_peer_service() {
        let mut cmd = Args::command();
        let mut buf = Vec::new();
        cmd.write_long_help(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("ratatoskr-spike must not be run while this peer service is up"),
            "{text}"
        );
    }

    #[test]
    fn a_pipeline_missing_its_service_opens_nothing() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let headers = must_err(open_owned(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: true,
                venues: false,
                receipts: false,
                state: false,
            },
            &paths,
        ));
        assert!(headers.to_string().contains("headers"), "{headers}");
        assert_no_databases(&paths);
        let receipts = must_err(open_owned(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: false,
                receipts: true,
                state: false,
            },
            &paths,
        ));
        assert!(receipts.to_string().contains("receipts"), "{receipts}");
        assert_no_databases(&paths);
        let venues = must_err(open_owned(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: true,
                receipts: false,
                state: false,
            },
            &paths,
        ));
        assert!(venues.to_string().contains("venues"), "{venues}");
        assert_no_databases(&paths);
    }

    #[test]
    fn peer_service_opens_only_the_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_owned(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines::default(),
            &paths,
        )
        .unwrap();
        assert!(paths.peers.exists());
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        drop(owned);
    }

    #[test]
    fn db_service_does_not_create_the_peer_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_owned(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: false,
                venues: true,
                receipts: false,
                state: false,
            },
            &paths,
        )
        .unwrap();
        assert!(!paths.peers.exists());
        assert!(paths.headers.exists());
        assert!(paths.corpus.exists());
        assert!(dir.0.join("entities.redb").exists());
        assert!(dir.0.join("discovery.redb").exists());
        assert!(dir.0.join("work.redb").exists());
        assert!(!paths.venues.exists());
        drop(owned);
    }

    #[test]
    fn locked_peer_catalog_does_not_open_entity_files() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        std::fs::write(&paths.peer_socket, b"stale-peer").unwrap();
        std::fs::write(&paths.db_socket, b"stale-db").unwrap();
        let held = peer::open(&paths.peers).unwrap();
        let err = must_err(open_owned(&both(), &headers_venues(), &paths));
        let text = format!("{err:#}");
        assert!(text.contains("already open"), "{text}");
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        assert_eq!(std::fs::read(&paths.peer_socket).unwrap(), b"stale-peer");
        assert_eq!(std::fs::read(&paths.db_socket).unwrap(), b"stale-db");
        drop(held);
    }

    #[test]
    fn locked_entity_file_releases_the_peer_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        std::fs::write(&paths.peer_socket, b"stale-peer").unwrap();
        std::fs::write(&paths.db_socket, b"stale-db").unwrap();
        let held = LocalStore::open_headers(&paths.headers).unwrap();
        let err = must_err(open_owned(&both(), &headers_venues(), &paths));
        let text = format!("{err:#}");
        assert!(text.contains("already open"), "{text}");
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        assert_eq!(std::fs::read(&paths.peer_socket).unwrap(), b"stale-peer");
        assert_eq!(std::fs::read(&paths.db_socket).unwrap(), b"stale-db");
        let reopened = peer::open(&paths.peers).unwrap();
        drop(reopened);
        drop(held);
    }

    #[tokio::test]
    async fn db_socket_is_replaced_after_the_entity_files_open() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        std::fs::write(&paths.db_socket, b"stale-db").unwrap();
        std::fs::write(&paths.peer_socket, b"stale-peer").unwrap();
        let owned = open_owned(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: false,
                venues: false,
                receipts: false,
                state: false,
            },
            &paths,
        )
        .unwrap();
        assert!(!paths.peers.exists());
        assert_eq!(std::fs::read(&paths.db_socket).unwrap(), b"stale-db");
        let store = Arc::new(owned.store.unwrap());
        let socket = paths.db_socket.clone();
        let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
        let path = socket.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::select! {
                    result = store::serve(store, &path) => result.unwrap(),
                    _ = cancel_rx => {}
                }
            });
        });
        let mut ready = false;
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ready, "db socket was not bound");
        assert!(socket.metadata().unwrap().file_type().is_socket());
        assert_eq!(std::fs::read(&paths.peer_socket).unwrap(), b"stale-peer");
        let _ = cancel.send(());
        thread.join().unwrap();
    }
}
