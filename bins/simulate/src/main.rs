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
    let (config_dir, rest) = split_config(env::args().skip(1))?;
    if rest.len() > 2 {
        return Err(eyre::eyre!(
            "usage: simulate [--config-dir DIR] [data-dir] [limit]"
        ));
    }
    let loaded = config::load(&config_dir)?;
    observability::init_spans(&loaded.simulate.log);
    let mut data_dir = config::expand_home(&loaded.process.data_dir)?;
    let mut limit = None;
    if let Some(first) = rest.first() {
        if let Ok(count) = first.parse::<usize>() {
            limit = Some(count);
        } else {
            data_dir = path_arg(first)?;
            if let Some(second) = rest.get(1) {
                limit = Some(second.parse::<usize>().wrap_err("limit")?);
            }
        }
    }
    survey::measure(&data_dir, limit, &loaded.spots, &loaded.store)
}

fn split_config(args: impl Iterator<Item = String>) -> eyre::Result<(PathBuf, Vec<String>)> {
    let mut config_dir = PathBuf::from("config");
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--config-dir" {
            let Some(dir) = args.next() else {
                return Err(eyre::eyre!("--config-dir needs a directory"));
            };
            config_dir = PathBuf::from(dir);
            continue;
        }
        if let Some(dir) = arg.strip_prefix("--config-dir=") {
            if dir.is_empty() {
                return Err(eyre::eyre!("--config-dir needs a directory"));
            }
            config_dir = PathBuf::from(dir);
            continue;
        }
        rest.push(arg);
    }
    Ok((config_dir, rest))
}

fn path_arg(text: &str) -> eyre::Result<PathBuf> {
    if text.starts_with('~') {
        config::expand_home(text)
    } else {
        Ok(PathBuf::from(text))
    }
}
