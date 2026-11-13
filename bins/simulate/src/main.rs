//! Open the entity catalog read-only and score allocated trades.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use eyre::WrapErr;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("simulate: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> eyre::Result<()> {
    let mut args = env::args().skip(1);
    let mut data_dir = PathBuf::from("data/ratatoskr");
    let mut limit = None;
    if let Some(first) = args.next() {
        if let Ok(count) = first.parse::<usize>() {
            limit = Some(count);
        } else {
            data_dir = PathBuf::from(first);
            if let Some(second) = args.next() {
                limit = Some(second.parse::<usize>().wrap_err("limit")?);
            }
        }
    }
    if args.next().is_some() {
        return Err(eyre::eyre!("usage: simulate [data-dir] [limit]"));
    }
    survey::measure(&data_dir, limit)
}
