//! Enumerate mainnet venue identities into `venues.redb`.

use chain::HeaderChain;
use clap::Parser;
use corpus::Corpus;
use ipc::{socket_path, SocketPeer};
use pipeline::{Source, StoreSink, VenueSource};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use store::{CorpusOpen, LocalStore};
use venues::{
    fetch_receipts, history_selected, job_wanted, kind_name, scan_blooms, Catalog, RunOpts,
    RunPhase, RunReport,
};

#[derive(Debug, Parser)]
#[command(name = "ratatoskr-venues")]
struct Args {
    /// Venue catalog. Created if missing.
    #[arg(long)]
    out: Option<PathBuf>,
    /// JSON-RPC URL. Repeat to rotate. Default is a short public list.
    #[arg(long)]
    rpc: Vec<String>,
    /// Run only jobs whose name contains this string.
    #[arg(long)]
    only: Option<String>,
    /// Receipt corpus. Default is the ingest catalog when that file exists.
    #[arg(long)]
    corpus: Option<PathBuf>,
    /// Do not fold the local corpus in.
    #[arg(long)]
    no_corpus: bool,
}

fn main() -> eyre::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "venues=info".into()),
        )
        .init();
    let args = Args::parse();
    let out = args.out.unwrap_or_else(Catalog::default_path);
    let import_path = if args.no_corpus {
        None
    } else if let Some(path) = args.corpus {
        Some(path)
    } else {
        let path = Corpus::default_path();
        path.exists().then_some(path)
    };
    let import = import_path.is_some() && job_wanted(&args.only, "corpus");
    let history = history_selected(&args.only);
    let opts = RunOpts {
        rpcs: args.rpc,
        only: args.only,
        no_corpus: args.no_corpus,
        corpus_available: import,
        phase: RunPhase::ListsAndCorpus,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let report = runtime.block_on(async {
        let mut list_opts = opts.clone();
        list_opts.phase = RunPhase::Lists;
        list_opts.corpus_available = false;
        let store = LocalStore::parts(None, None, Some(&out))?;
        let mut report = run_phase(store, None, list_opts).await?;
        if import {
            if let Some(path) = import_path {
                match LocalStore::parts(None, Some(CorpusOpen::Open(&path)), Some(&out)) {
                    Ok(store) => {
                        let mut corpus_opts = opts.clone();
                        corpus_opts.phase = RunPhase::Corpus;
                        corpus_opts.corpus_available = true;
                        let imported = run_phase(store, None, corpus_opts).await?;
                        report.list_errors.extend(imported.list_errors);
                        report.notes.extend(imported.notes);
                    }
                    Err(err) => {
                        let note = format!("corpus: {err:#}");
                        tracing::error!(%note);
                        report.notes.push(note);
                    }
                }
            }
        }
        if history {
            let headers = HeaderChain::default_path();
            let (_tx, stop) = tokio::sync::watch::channel(false);
            // The catalog stores bloom hits. The corpus stays closed so an
            // ingest process can keep corpus.redb.
            let scanned = match LocalStore::parts(Some(&headers), None, Some(&out)) {
                Ok(store) => {
                    let scanned = scan_blooms(&store, &opts.only, &stop).await;
                    drop(store);
                    tracing::info!("released header chain");
                    Some(scanned)
                }
                Err(err) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                    None
                }
            };
            match scanned {
                Some(Ok(scan)) => {
                    let corpus_path = Corpus::default_path();
                    match LocalStore::parts(
                        None,
                        Some(CorpusOpen::OpenOrCreate(&corpus_path)),
                        Some(&out),
                    ) {
                        Ok(store) => {
                            let peer = socket_path().ok().map(SocketPeer::new);
                            match fetch_receipts(&store, peer.as_ref(), scan, &stop).await {
                                Ok(notes) => report.notes.extend(notes),
                                Err(err) => {
                                    let note = format!("history: {err:#}");
                                    tracing::error!(%note);
                                    report.notes.push(note);
                                }
                            }
                        }
                        Err(err) => {
                            let note = format!("history: {err:#}");
                            tracing::error!(%note);
                            report.notes.push(note);
                        }
                    }
                }
                Some(Err(err)) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
                None => {}
            }
        }
        Ok::<_, eyre::Report>(report)
    })?;

    let catalog = Catalog::open(&out)?;
    let tally = catalog.tally_by_kind()?;
    let rows: u64 = tally.values().sum();
    println!("venues {rows} catalog {}", out.display());
    for ((kind, factory), count) in tally {
        println!("{} {factory} {count}", kind_name(kind));
    }
    for note in &report.notes {
        println!("note {note}");
    }
    for error in &report.list_errors {
        println!("error {error}");
    }
    if report.list_errors.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

async fn run_phase(
    store: LocalStore,
    peer: Option<SocketPeer>,
    opts: RunOpts,
) -> eyre::Result<RunReport> {
    let source = VenueSource::new(store.clone(), peer, opts);
    let handle = source.report();
    let (_tx, stop) = tokio::sync::watch::channel(false);
    let mut sink = StoreSink::new(store);
    source.run(&mut sink, stop).await?;
    Ok(snapshot(&handle))
}

fn snapshot(report: &Arc<Mutex<RunReport>>) -> RunReport {
    report.lock().expect("report").clone()
}
