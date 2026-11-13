//! Open the entity catalog and run the searcher on trades chosen from it.
//!
//! The trades are parameterization scenarios. They cover each venue kind, the
//! hub pairs, native ETH through the wrap, one-hub leaf pairs, and token pairs
//! whose shortest stored path is longer than two hubs. Hubs are the tokens
//! named by the most conduits. The search returns the direct pair, the hub
//! routes, and a bounded walk from an endpoint that names few conduits.

mod check;
mod graph;
mod kinds;
mod pick;
mod reader;
mod report;
mod scan;
mod scenario;
mod token;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;

use eyre::WrapErr;
use searcher::{execute, Token};
use venues::Catalog;

pub(crate) const HUB_MINIMUM: usize = 3;
pub(crate) const QUERY_HUBS: usize = 32;
pub(crate) const HUB_LIMITS: [usize; 4] = [8, 16, 32, 64];

/// Open the entity catalog at `data_dir` and print the search.
pub fn run(data_dir: &Path) -> eyre::Result<()> {
    let known = token::known_tokens();
    let wrapped = token::known_named(&known, "WETH");
    eprintln!("opening {}", data_dir.display());
    let catalog = Catalog::open(data_dir).wrap_err("open catalog")?;
    eprintln!("scanning conduits");
    let survey = scan::survey(&catalog)?;
    eprintln!(
        "scanned {} conduits, {} tokens with an outgoing curve",
        survey.conduits,
        survey.graph.out.len()
    );
    let incoming = graph::incoming(&survey.graph);
    let census = graph::hub_census(&survey.graph, &incoming, HUB_MINIMUM, HUB_LIMITS[3]);
    let hubs: Vec<Token> = census
        .ranked
        .iter()
        .take(QUERY_HUBS)
        .map(|hub| hub.token)
        .collect();
    report::note_degrees(&survey.graph, &known);
    report::note_hubs(&census, &known);
    let mut reach_rows = Vec::new();
    for limit in HUB_LIMITS {
        let slice: Vec<Token> = census
            .ranked
            .iter()
            .take(limit)
            .map(|hub| hub.token)
            .collect();
        let reach = graph::reach_of(&survey.graph, &incoming, &slice);
        eprintln!(
            "reach K={limit} both {} toward {} from {} outside {}",
            reach.both, reach.toward, reach.from_hub, reach.outside
        );
        reach_rows.push((limit, reach));
    }
    eprintln!("choosing paths through {} hubs", hubs.len());
    let selection = graph::select(&survey.graph, &hubs);
    eprintln!(
        "candidates long {} via {} deep {} isolated {} wide-skips {}",
        selection.long.len(),
        selection.via_hub.len(),
        selection.deep_to.len() + selection.deep_from.len(),
        selection.isolated.len(),
        selection.wide_skips
    );
    let reader = reader::Reader {
        catalog: &catalog,
        cache: RefCell::new(HashMap::new()),
        graph: &survey.graph,
        incoming: &incoming,
    };
    eprintln!("checking pool identity on the candidate paths");
    let long = reader::keep_disjoint(&survey.graph, &incoming, &reader, &selection.long, 24);
    let via = reader::keep_disjoint(&survey.graph, &incoming, &reader, &selection.via_hub, 12);
    let isolated = reader::keep_disjoint(&survey.graph, &incoming, &reader, &selection.isolated, 4);
    eprintln!(
        "kept long {} via {} isolated {}",
        long.len(),
        via.len(),
        isolated.len()
    );
    let hub_set: HashSet<Token> = hubs.iter().copied().collect();
    let picks = pick::picks(&survey, &survey.graph, &incoming, &hub_set, wrapped);
    let scenarios = scenario::scenarios(&scenario::Chosen {
        survey: &survey,
        known: &known,
        hubs: &hubs,
        wrapped,
        picks: &picks,
        selection: &selection,
        long: &long,
        via: &via,
        isolated: &isolated,
    });
    let mut out = String::new();
    report::push_header(
        &mut out,
        &survey,
        &known,
        &hubs,
        &selection,
        &census,
        &reach_rows,
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
    eprintln!("running {} scenarios", scenarios.len());
    for scenario in &scenarios {
        match trade::query(&scenario.trade, &scenario.hubs) {
            Ok(query) => {
                let set = execute(&query, &reader);
                if let Err(err) = trade::problem(
                    &scenario.trade,
                    &scenario.hubs,
                    &set,
                    &BTreeMap::new(),
                    reader::catalog_swap,
                ) {
                    checks
                        .failures
                        .push(format!("{} problem {err}", scenario.name));
                }
                check::record(
                    &mut checks,
                    scenario,
                    &set,
                    &known,
                    &survey.graph,
                    &incoming,
                );
                report::push_set(
                    &mut out,
                    scenario,
                    &set,
                    &known,
                    &survey.graph,
                    &incoming,
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
    check::finish_checks(&mut checks, &survey, &selection);
    check::push_checks(&mut out, &checks);
    let mut stdout = io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    if checks.failures.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!("{} checks failed", checks.failures.len()))
    }
}
