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
mod history;
mod import;
mod lens;
mod lists;
mod logs;
mod rpc;

pub use catalog::{kind_name, Catalog, Put, VenueRow};
pub use logs::Walk;

use std::path::PathBuf;

pub struct RunOpts {
    pub out: PathBuf,
    pub rpcs: Vec<String>,
    pub only: Option<String>,
    /// `None` skips the corpus. `Some` opens that file.
    pub corpus: Option<PathBuf>,
}

pub struct RunReport {
    pub list_errors: Vec<String>,
    pub notes: Vec<String>,
}

pub fn run(opts: RunOpts) -> eyre::Result<RunReport> {
    let catalog = Catalog::open(&opts.out)?;
    let call_urls = if opts.rpcs.is_empty() {
        rpc::default_call_rpcs()
    } else {
        opts.rpcs.clone()
    };
    let calls = rpc::Rpc::new(call_urls)?;
    let mut report = RunReport {
        list_errors: Vec::new(),
        notes: Vec::new(),
    };

    for job in lists::jobs() {
        if !wanted(&opts.only, &job.name) {
            continue;
        }
        match lists::walk(&calls, &catalog, &job) {
            Ok(wrote) => tracing::info!(job = %job.name, wrote, "list finished"),
            Err(err) => {
                let message = format!("{}: {err}", job.name);
                tracing::error!(%message, "list failed");
                report.list_errors.push(message);
            }
        }
    }

    if let Some(path) = &opts.corpus {
        if wanted(&opts.only, "corpus") {
            match import::import_corpus(&catalog, path) {
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

    let logs_wanted = logs::jobs()
        .into_iter()
        .filter(|job| wanted(&opts.only, &job.name))
        .collect::<Vec<_>>();
    if !logs_wanted.is_empty() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            match history::ingest_all(&catalog, &logs_wanted).await {
                Ok(notes) => {
                    for note in notes {
                        report.notes.push(note);
                    }
                }
                Err(err) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
            }
        });
    }
    Ok(report)
}

fn wanted(only: &Option<String>, name: &str) -> bool {
    match only {
        None => true,
        Some(part) => name.to_lowercase().contains(&part.to_lowercase()),
    }
}
