//! Enumerate mainnet venue identities into `venues.redb`.

use clap::Parser;
use corpus::Corpus;
use std::path::PathBuf;
use std::process::ExitCode;
use venues::{kind_name, Catalog, RunOpts};

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
    let corpus = if args.no_corpus {
        None
    } else if let Some(path) = args.corpus {
        Some(path)
    } else {
        let path = Corpus::default_path();
        path.exists().then_some(path)
    };
    let report = venues::run(RunOpts {
        out: out.clone(),
        rpcs: args.rpc,
        only: args.only,
        corpus,
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
