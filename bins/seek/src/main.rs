//! Open the entity catalog and print the search over the trades chosen from it.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("seek: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> eyre::Result<()> {
    let mut args = env::args().skip(1);
    let data_dir = match args.next() {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("data/ratatoskr"),
    };
    if args.next().is_some() {
        return Err(eyre::eyre!("usage: seek [data-dir]"));
    }
    survey::run(&data_dir)
}
