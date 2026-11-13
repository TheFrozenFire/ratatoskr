use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use eyre::WrapErr;
use searcher::Token;

use crate::graph::{self, HubCensus, HubReach, Selection};
use crate::reader::{self, CurveCache, Reader};
use crate::scan::{self, Survey};
use crate::scenario::{self, Scenario};
use crate::source::Source;
use crate::token::{self, Known};
use crate::{HUB_LIMITS, HUB_MINIMUM, QUERY_HUBS};

pub(crate) struct Stage {
    pub(crate) survey: Survey,
    pub(crate) incoming: HashMap<Token, Vec<Token>>,
    pub(crate) known: Vec<Known>,
    pub(crate) hubs: Vec<Token>,
    pub(crate) wrapped: Token,
    pub(crate) scenarios: Vec<Scenario>,
    pub(crate) census: HubCensus,
    pub(crate) reach_rows: Vec<(usize, HubReach)>,
    pub(crate) selection: Selection,
    pub(crate) cache: CurveCache,
}

pub(crate) fn load<S: Source>(source: &S) -> eyre::Result<Stage> {
    let records = source.tokens().wrap_err("tokens")?;
    let wrapped =
        token::wrapper(&records).ok_or_else(|| eyre::eyre!("the catalog has no wrapper token"))?;
    let known = token::labels(&records);
    eprintln!("scanning conduits");
    let survey = scan::survey(source)?;
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
    crate::report::note_degrees(&survey.graph, &known);
    crate::report::note_hubs(&census, &known);
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
    let reader = Reader {
        source,
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
    let picks = crate::pick::picks(&survey, &survey.graph, &incoming, &hub_set, wrapped);
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
    let cache = reader.cache.into_inner();
    Ok(Stage {
        survey,
        incoming,
        known,
        hubs,
        wrapped,
        scenarios,
        census,
        reach_rows,
        selection,
        cache,
    })
}
