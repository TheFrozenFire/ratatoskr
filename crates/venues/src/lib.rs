//! Venue catalog for the mainnet pools people trade.
//!
//! List walks, bloom hits, token rows, and pool state are written here.
//! Reading the chain — JSON-RPC, the creation-code lenses, and creation logs —
//! is [`chain`]. Factories with no list are found in the local header blooms.
//! Hits stay in the venue catalog. Receipts come from era1 before the merge
//! and from the peer daemon after it, and those payloads stay in redb.

mod catalog;
mod conduit;
mod fit;
mod history;
mod hooks;
mod import;
mod ingest;
mod lists;
mod logs;
mod state;
mod tokens;

pub use catalog::{
    append_dropped_ids, append_fill_ids, decode_body, decode_row, encode_body, encode_row,
    fill_identities, kind_name, read_fill_ids, sample_identities, write_fill_ids, Catalog,
    EntityRead, FillReport, Put, ReceiptBody, SampleReport, TokenRecord, VenueRow, BALANCER_V2,
    BALANCER_V3, PANCAKE_V3, SUSHI_V3, UNI_V3, UNI_V4, V3_FORK,
};
pub use chain::detail::{load_details, DetailQuery, PoolDetail, Quote};
pub use chain::{custodial_wrap, default_call_rpcs, root_address, Rpc, RpcLog};
pub use conduit::{ConduitFail, ConduitJob};
pub use fit::{estimate, refit_curves};
pub use history::BloomScan;
pub use hooks::install_catalog_hooks;
pub use logs::Walk;
pub use state::{
    drop_empty_pools, refresh_kinds, CurveRecord, ImmutablePut, ImmutableRow, MutableStored,
    MutableWrite, PoolState, RefreshReport, StateSummary, TickPage, WatchHit, WorkFail, WorkJob,
};
pub use store::{ChainStore, ConduitStore, CorpusStore, DiscoveryStore, TokenStore, VenueStore};

use peer::HistoryPeer;
use std::sync::OnceLock;
use tokio::sync::watch;

struct WorkPause {
    tx: watch::Sender<bool>,
    _rx: watch::Receiver<bool>,
}

static WORK_PAUSE: OnceLock<WorkPause> = OnceLock::new();

fn work_pause() -> &'static WorkPause {
    WORK_PAUSE.get_or_init(|| {
        let (tx, rx) = watch::channel(false);
        WorkPause { tx, _rx: rx }
    })
}

/// Stop claiming work on every stage. Peer sessions stay connected.
pub fn pause_work() {
    let _ = work_pause().tx.send(true);
    observability::note_paused(true);
    observability::info!("work paused");
}

/// Let every stage claim again. Peer sessions were not dropped.
pub fn resume_work() {
    let _ = work_pause().tx.send(false);
    observability::note_paused(false);
    observability::info!("work resumed");
}

pub fn work_paused() -> bool {
    *work_pause().tx.borrow()
}

/// Wait while work is paused. Returns true when the caller should shut down.
pub async fn hold_for_pause(stop: &watch::Receiver<bool>) -> bool {
    let mut paused = work_pause().tx.subscribe();
    let mut stop = stop.clone();
    loop {
        if *stop.borrow() {
            return true;
        }
        if !*paused.borrow() {
            return false;
        }
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return true;
                }
            }
            changed = paused.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

/// Which part of a venue run to perform. A standalone binary splits these so
/// one phase does not hold a file the next phase must leave free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    Lists,
    Corpus,
    ListsAndCorpus,
    History,
    /// Pool details, tick pages, curves, and the watch. Not part of [`Self::All`].
    State,
    All,
}

#[derive(Clone)]
pub struct RunOpts {
    pub rpcs: Vec<String>,
    pub only: Option<String>,
    pub no_corpus: bool,
    /// The caller opened the corpus file this import should scan.
    pub corpus_available: bool,
    pub phase: RunPhase,
    /// `eth_call` block. When set, state does not wait on the header cursor.
    pub pin_block: Option<u64>,
    /// Leave the state loop once nothing is left to claim. A job that fails
    /// four times is dropped and recorded.
    pub until_idle: bool,
    pub venues: config::Venues,
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub list_errors: Vec<String>,
    pub notes: Vec<String>,
}

pub fn job_wanted(only: &Option<String>, name: &str) -> bool {
    match only {
        None => true,
        Some(part) => name.to_lowercase().contains(&part.to_lowercase()),
    }
}

pub fn history_selected(only: &Option<String>) -> bool {
    chain::seeds::document()
        .roots
        .iter()
        .filter(|root| root.logs())
        .any(|root| job_wanted(only, &format!("logs:{}", root.name)))
}

pub async fn execute<S, P>(
    store: &S,
    peer: Option<&P>,
    opts: &RunOpts,
    stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<RunReport>
where
    S: ChainStore
        + CorpusStore
        + DiscoveryStore
        + VenueStore
        + ConduitStore
        + TokenStore
        + Clone
        + 'static,
    P: HistoryPeer + Clone + 'static,
{
    install_catalog_hooks();
    if opts.phase == RunPhase::State {
        return state::run(store, peer, opts, stop).await;
    }
    let mut report = RunReport::default();
    let lists = matches!(
        opts.phase,
        RunPhase::Lists | RunPhase::ListsAndCorpus | RunPhase::All
    );
    let import = matches!(
        opts.phase,
        RunPhase::Corpus | RunPhase::ListsAndCorpus | RunPhase::All
    );
    let history = matches!(opts.phase, RunPhase::History | RunPhase::All);
    let calls = if lists || history {
        match open_calls(opts).await {
            Ok(calls) => Some(calls),
            Err(err) => {
                let message = format!("seeds: {err:#}");
                observability::error!(%message);
                report.list_errors.push(message);
                None
            }
        }
    } else {
        None
    };
    let planned = if let Some(calls) = &calls {
        match ingest::plan(store, calls).await {
            Ok(plan) => Some(plan),
            Err(err) => {
                let message = format!("seeds: {err:#}");
                observability::error!(%message);
                report.list_errors.push(message);
                None
            }
        }
    } else {
        None
    };
    if lists {
        if let (Some(calls), Some(plan)) = (&calls, &planned) {
            for job in &plan.lists {
                if hold_for_pause(&stop).await {
                    return Ok(report);
                }
                if !job_wanted(&opts.only, &job.name) {
                    continue;
                }
                match lists::walk(calls, store, job, &stop).await {
                    Ok(wrote) => observability::info!(job = %job.name, wrote, "list finished"),
                    Err(err) => {
                        let message = format!("{}: {err}", job.name);
                        observability::error!(%message, "list failed");
                        report.list_errors.push(message);
                    }
                }
            }
        }
    }
    if import && !opts.no_corpus && opts.corpus_available && job_wanted(&opts.only, "corpus") {
        if hold_for_pause(&stop).await {
            return Ok(report);
        }
        match import::import_corpus(store).await {
            Ok(wrote) => {
                let note = format!("corpus wrote {wrote}");
                observability::info!(%note);
                report.notes.push(note);
            }
            Err(err) => {
                let message = format!("corpus: {err}");
                observability::error!(%message);
                report.list_errors.push(message);
            }
        }
    }
    if history {
        if hold_for_pause(&stop).await {
            return Ok(report);
        }
        let logs_wanted = planned
            .as_ref()
            .map(|plan| {
                plan.logs
                    .iter()
                    .filter(|job| job_wanted(&opts.only, &job.name))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !logs_wanted.is_empty() {
            match history::ingest_all(store, peer, &logs_wanted, &stop, &opts.venues).await {
                Ok(notes) => report.notes.extend(notes),
                Err(err) => {
                    let note = format!("history: {err:#}");
                    observability::error!(%note);
                    report.notes.push(note);
                }
            }
        }
    }
    if job_wanted(&opts.only, "tokens") {
        if hold_for_pause(&stop).await {
            return Ok(report);
        }
        let rpc = match &calls {
            Some(calls) => Some(calls.clone()),
            None => match open_calls(opts).await {
                Ok(rpc) => Some(rpc),
                Err(err) => {
                    let message = format!("tokens: {err:#}");
                    observability::error!(%message);
                    report.list_errors.push(message);
                    None
                }
            },
        };
        if let Some(rpc) = rpc {
            match tokens::walk(&rpc, store, &stop).await {
                Ok(wrote) => {
                    let note = format!("tokens wrote {wrote}");
                    observability::info!(%note);
                    report.notes.push(note);
                }
                Err(err) => {
                    let message = format!("tokens: {err:#}");
                    observability::error!(%message);
                    report.list_errors.push(message);
                }
            }
        }
    }
    Ok(report)
}

/// Bloom scan. The caller opens `headers.redb` for this and drops it before
/// [`fetch_receipts`]. The corpus stays closed so an ingest lock does not
/// block the scan.
pub async fn scan_blooms<S: ChainStore + DiscoveryStore>(
    store: &S,
    only: &Option<String>,
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<history::BloomScan> {
    let jobs = ingest::cached_logs(store)
        .await
        .into_iter()
        .filter(|job| job_wanted(only, &job.name))
        .collect::<Vec<_>>();
    history::scan_blooms(store, &jobs, stop, limits).await
}

async fn open_calls(opts: &RunOpts) -> eyre::Result<Rpc> {
    let call_urls = if opts.rpcs.is_empty() {
        default_call_rpcs()
    } else {
        opts.rpcs.clone()
    };
    tokio::task::spawn_blocking(move || Rpc::new(call_urls))
        .await
        .map_err(|err| eyre::eyre!("rpc client task: {err}"))?
}

/// Receipt fetch for a finished bloom scan. Needs the corpus and the catalog,
/// not the header file.
pub async fn fetch_receipts<S, P>(
    store: &S,
    peer: Option<&P>,
    scan: history::BloomScan,
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<Vec<String>>
where
    S: CorpusStore + DiscoveryStore,
    P: HistoryPeer + Clone + 'static,
{
    history::fetch_receipts(store, peer, scan, stop, limits).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn one_at_a_time() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct ResumeOnDrop;
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            resume_work();
        }
    }

    #[tokio::test]
    async fn pause_work_parks_until_resume() {
        let _one = one_at_a_time().await;
        let _guard = ResumeOnDrop;
        let (_stop_tx, stop) = watch::channel(false);
        pause_work();
        assert!(work_paused());
        let wait = tokio::spawn(async move { hold_for_pause(&stop).await });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert!(!wait.is_finished());
        resume_work();
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("resume wakes the stage")
            .expect("task");
        assert!(!stopped);
        assert!(!work_paused());
    }

    #[tokio::test]
    async fn pause_work_still_hears_shutdown() {
        let _one = one_at_a_time().await;
        let _guard = ResumeOnDrop;
        let (stop_tx, stop) = watch::channel(false);
        pause_work();
        let wait = tokio::spawn(async move { hold_for_pause(&stop).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        stop_tx.send(true).expect("shutdown");
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("shutdown wakes the stage")
            .expect("task");
        assert!(stopped);
    }
}
