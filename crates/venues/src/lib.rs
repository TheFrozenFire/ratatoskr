//! Venue identities for the mainnet pools people trade.
//!
//! A list view (`allPairs`, `allPools`, `pool_list`, `idxToPool`) is read by
//! `eth_call`ing the creation code in `contracts/lens/IndexLens.sol` with no
//! `to`. The constructor reverts the rows. Any client that runs a creation
//! `eth_call` can serve it. Factories with no list are found in the local
//! header blooms. Hits stay in the venue catalog. Receipts come from era1
//! before the merge and from the peer daemon after it, and those payloads
//! stay in redb.

mod catalog;
mod decode;
mod entity;
mod history;
mod import;
mod lens;
mod lists;
mod logs;
mod rpc;

pub use catalog::{kind_name, Catalog, Put, ReceiptBody, VenueRow};
pub use entity::EntityStore;
pub use logs::Walk;

use ipc::HistoryPeer;

/// Which part of a venue run to perform. A standalone binary splits these so
/// a list walk does not hold the header file open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    ListsAndCorpus,
    History,
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
    logs::jobs().iter().any(|job| job_wanted(only, &job.name))
}

pub async fn execute<S, P>(
    store: &S,
    peer: Option<&P>,
    opts: &RunOpts,
    stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<RunReport>
where
    S: EntityStore + Clone + 'static,
    P: HistoryPeer + Clone + 'static,
{
    let mut report = RunReport::default();
    let lists = matches!(opts.phase, RunPhase::ListsAndCorpus | RunPhase::All);
    let history = matches!(opts.phase, RunPhase::History | RunPhase::All);
    if lists {
        let call_urls = if opts.rpcs.is_empty() {
            rpc::default_call_rpcs()
        } else {
            opts.rpcs.clone()
        };
        let calls = rpc::Rpc::new(call_urls)?;
        for job in lists::jobs() {
            if *stop.borrow() {
                return Ok(report);
            }
            if !job_wanted(&opts.only, &job.name) {
                continue;
            }
            match lists::walk(&calls, store, &job, &stop).await {
                Ok(wrote) => tracing::info!(job = %job.name, wrote, "list finished"),
                Err(err) => {
                    let message = format!("{}: {err}", job.name);
                    tracing::error!(%message, "list failed");
                    report.list_errors.push(message);
                }
            }
        }
        if !opts.no_corpus && opts.corpus_available && job_wanted(&opts.only, "corpus") {
            if *stop.borrow() {
                return Ok(report);
            }
            match import::import_corpus(store).await {
                Ok(wrote) => {
                    let note = format!("corpus wrote {wrote}");
                    tracing::info!(%note);
                    report.notes.push(note);
                }
                Err(err) => {
                    let message = format!("corpus: {err}");
                    tracing::error!(%message);
                    report.list_errors.push(message);
                }
            }
        }
    }
    if history {
        if *stop.borrow() {
            return Ok(report);
        }
        let logs_wanted = logs::jobs()
            .into_iter()
            .filter(|job| job_wanted(&opts.only, &job.name))
            .collect::<Vec<_>>();
        if !logs_wanted.is_empty() {
            match history::ingest_all(store, peer, &logs_wanted, &stop).await {
                Ok(notes) => report.notes.extend(notes),
                Err(err) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
            }
        }
    }
    Ok(report)
}
