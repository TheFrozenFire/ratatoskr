//! Enumerate mainnet venue identities through the entity socket.
//!
//! This process does not open `venues.redb` or a local corpus file. The
//! database server owns both. A standalone source needs `ratatoskr-db` or
//! `ratatoskr` on the entity socket.

use clap::Parser;
use ipc::{socket_path, SocketPeer};
use pipeline::{Source, StoreSink, VenueSource};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use store::RemoteStore;
use venues::{RunOpts, RunPhase, RunReport};

/// Enumerate mainnet venue identities. Requires `ratatoskr-db` or `ratatoskr`.
///
/// Writes go through the entity socket. This process does not open `venues.redb`.
#[derive(Debug, Parser)]
#[command(name = "ratatoskr-venues")]
struct Args {
    /// JSON-RPC URL. Repeat to rotate. Default is a short public list.
    #[arg(long)]
    rpc: Vec<String>,
    /// Run only jobs whose name contains this string.
    #[arg(long)]
    only: Option<String>,
    /// Do not fold the corpus in.
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
    let store = RemoteStore::new(store::db_socket()?);
    let peer = Some(SocketPeer::new(socket_path()?));
    let opts = RunOpts {
        rpcs: args.rpc,
        only: args.only,
        no_corpus: args.no_corpus,
        corpus_available: !args.no_corpus,
        phase: RunPhase::All,
        pin_block: None,
        until_idle: false,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let report = runtime.block_on(async {
        let source = VenueSource::new(store.clone(), peer, opts);
        let handle = source.report();
        let (_tx, stop) = tokio::sync::watch::channel(false);
        let mut sink = StoreSink::new(store);
        source.run(&mut sink, stop).await?;
        Ok::<_, eyre::Report>(snapshot(&handle))
    })?;

    println!("catalog is the database server's");
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

fn snapshot(report: &Arc<Mutex<RunReport>>) -> RunReport {
    report.lock().expect("report").clone()
}
