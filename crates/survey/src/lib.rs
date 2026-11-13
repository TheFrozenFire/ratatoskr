//! Open the entity catalog and run the searcher on trades chosen from it.
//!
//! The trades are parameterization scenarios. They cover each venue kind, the
//! hub pairs, native ETH through the wrap, one-hub leaf pairs, and token pairs
//! whose shortest stored path is longer than two hubs. Hubs are the tokens
//! named by the most conduits. The search returns the direct pair, the hub
//! routes, and a bounded walk from an endpoint that names few conduits.
//!
//! [`measure`] opens the entity file read-only, allocates each trade, and
//! scores the plan. Prices are applied after the allocation.

mod check;
mod graph;
mod kinds;
mod measure;
mod pick;
mod reader;
mod report;
mod scan;
mod scenario;
mod source;
mod stage;
mod token;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;

use eyre::WrapErr;
use searcher::execute;
use venues::Catalog;

use crate::reader::Reader;
use crate::source::Source;

pub(crate) const HUB_MINIMUM: usize = 3;
pub(crate) const QUERY_HUBS: usize = 32;
pub(crate) const HUB_LIMITS: [usize; 4] = [8, 16, 32, 64];

/// Open the entity catalog at `data_dir` and print the search.
pub fn run(data_dir: &Path) -> eyre::Result<()> {
    eprintln!("opening {}", data_dir.display());
    let catalog = Catalog::open(data_dir).wrap_err("open catalog")?;
    print_search(&catalog)
}

/// Open the entity file read-only, allocate the trades, and print each score.
///
/// `limit` keeps the first trades in scenario order. `None` runs all of them.
pub fn measure(data_dir: &Path, limit: Option<usize>) -> eyre::Result<()> {
    eprintln!("opening {} read-only", data_dir.display());
    let catalog = venues::EntityRead::open(data_dir).wrap_err("open catalog")?;
    let mut stage = stage::load(&catalog)?;
    measure::run(&catalog, &mut stage, limit)
}

fn print_search(catalog: &impl Source) -> eyre::Result<()> {
    let mut stage = stage::load(catalog)?;
    let reader = Reader {
        source: catalog,
        cache: RefCell::new(std::mem::take(&mut stage.cache)),
        graph: &stage.survey.graph,
        incoming: &stage.incoming,
    };
    let mut out = String::new();
    report::push_header(
        &mut out,
        &stage.survey,
        &stage.known,
        &stage.hubs,
        &stage.selection,
        &stage.census,
        &stage.reach_rows,
    );
    let mut checks = check::Checks {
        failures: Vec::new(),
        kinds: 0,
        exact_pure: HashSet::new(),
        saw_wrap: false,
        saw_one: false,
        saw_two: false,
        sweep: HashMap::new(),
        long: 0,
        via: 0,
        deep: 0,
        long_lengths: BTreeMap::new(),
    };
    eprintln!("running {} scenarios", stage.scenarios.len());
    for scenario in &stage.scenarios {
        match trade::query(&scenario.trade, &scenario.hubs) {
            Ok(query) => {
                let set = execute(&query, &reader);
                if let Err(err) = trade::problem(
                    &scenario.trade,
                    &scenario.hubs,
                    &set,
                    &BTreeMap::new(),
                    |token_in, token_out, key| {
                        reader::catalog_swap(catalog, token_in, token_out, key)
                    },
                ) {
                    checks
                        .failures
                        .push(format!("{} problem {err}", scenario.name));
                }
                check::record(
                    &mut checks,
                    scenario,
                    &set,
                    &stage.known,
                    &stage.survey.graph,
                    &stage.incoming,
                );
                report::push_set(
                    &mut out,
                    scenario,
                    &set,
                    &stage.known,
                    &stage.survey.graph,
                    &stage.incoming,
                    &reader,
                );
            }
            Err(err) => {
                checks
                    .failures
                    .push(format!("{} rejected {err}", scenario.name));
                out.push_str(&format!(
                    "== {}\n{}\nrejected {err}\n\n",
                    scenario.name, scenario.detail
                ));
            }
        }
    }
    check::finish_checks(&mut checks, &stage.survey, &stage.selection);
    check::push_checks(&mut out, &checks);
    let mut stdout = io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    if checks.failures.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!("{} checks failed", checks.failures.len()))
    }
}
