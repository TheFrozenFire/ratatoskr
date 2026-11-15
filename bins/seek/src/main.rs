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
    let (config_dir, rest) = split_config(env::args().skip(1))?;
    if rest.len() > 1 {
        return Err(eyre::eyre!("usage: seek [--config-dir DIR] [data-dir]"));
    }
    let loaded = config::load(&config_dir)?;
    let data_dir = match rest.first() {
        Some(path) => path_arg(path)?,
        None => config::expand_home(&loaded.process.data_dir)?,
    };
    survey::run(&data_dir, &loaded.spots, &loaded.store)
}

fn split_config(args: impl Iterator<Item = String>) -> eyre::Result<(PathBuf, Vec<String>)> {
    let mut config_dir = PathBuf::from("config");
    let mut rest = Vec::new();
    let mut args = args;
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
