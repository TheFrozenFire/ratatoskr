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
use venues::{history_selected, job_wanted, kind_name, Catalog, RunOpts, RunPhase, RunReport};

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
        let corpus = import_path
            .as_deref()
            .filter(|_| import)
            .map(CorpusOpen::Open);
        let store = LocalStore::parts(None, corpus, Some(&out))?;
        let mut report = run_phase(store, None, opts.clone()).await?;
        if history {
            let headers = HeaderChain::default_path();
            let corpus_path = Corpus::default_path();
            match LocalStore::parts(
                Some(&headers),
                Some(CorpusOpen::OpenOrCreate(&corpus_path)),
                Some(&out),
            ) {
                Ok(store) => {
                    let peer = socket_path().ok().map(SocketPeer::new);
                    let mut history_opts = opts.clone();
                    history_opts.phase = RunPhase::History;
                    history_opts.corpus_available = false;
                    let history_report = run_phase(store, peer, history_opts).await?;
                    report.list_errors.extend(history_report.list_errors);
                    report.notes.extend(history_report.notes);
                }
                Err(err) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
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
