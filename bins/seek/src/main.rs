//! Open the entity catalog and run the searcher on trades chosen from it.
//!
//! The trades are parameterization scenarios. They cover each venue kind, the
//! hub pairs, native ETH through the wrap, one-hub leaf pairs, and token pairs
//! whose shortest stored path is longer than two hubs. Hubs are the tokens
//! named by the most conduits. The search returns the direct pair, the hub
//! routes, and a bounded walk from an endpoint that names few conduits.

mod graph;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use alloy_primitives::Address;
use eyre::WrapErr;
use graph::{Deep, Graph, HubCensus, HubReach, PathEnds, Selection};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Pow;
use routing::SlopeSegment;
use searcher::{
    execute, prepare, Conduits, Curve, Route, RouteSet, Token, Trade, NATIVE, NEIGHBOR_LIMIT,
    WALK_DEPTH,
};
use venues::{kind_name, Catalog};

const HUB_MINIMUM: usize = 3;
const QUERY_HUBS: usize = 32;
const HUB_LIMITS: [usize; 4] = [8, 16, 32, 64];

const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
const WBTC: &str = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Ordinary,
    Long,
    Via,
    Deep,
    Isolated,
}

struct Known {
    name: &'static str,
    token: Token,
}

#[derive(Clone)]
struct Example {
    from: Token,
    to: Token,
    kinds: u32,
    capacity: u128,
    segments: u32,
}

struct Bag {
    high: Vec<Example>,
    low: Vec<Example>,
}

struct Survey {
    graph: Graph,
    conduits: u64,
    pure: HashMap<u8, Bag>,
    carrier: HashMap<u8, Example>,
    by_width: HashMap<u32, Example>,
}

struct Picks {
    witness: Vec<(u8, Example)>,
    rich: Vec<(u8, Example)>,
    into_weth: Vec<(u8, Example)>,
    out_of_weth: Vec<(u8, Example)>,
    mixed: Vec<(u8, Example)>,
}

struct Chosen<'a> {
    survey: &'a Survey,
    known: &'a [Known],
    hubs: &'a [Token],
    wrapped: Token,
    picks: &'a Picks,
    selection: &'a Selection,
    long: &'a [PathEnds],
    via: &'a [PathEnds],
    isolated: &'a [PathEnds],
}

struct Scenario {
    name: String,
    detail: String,
    trade: Trade,
    hubs: Vec<Token>,
    wrapped: Option<Token>,
    paths: Vec<Vec<Token>>,
    show_path: bool,
    direct_kinds: Option<u32>,
    expect_pure: Vec<(Token, Token, u32)>,
    must_empty: bool,
    expect_routes: Option<usize>,
    sweep: Option<String>,
    require_end: Option<Token>,
    forbid_ends: Vec<Token>,
    reach: Vec<Token>,
    must_wrap: bool,
    cover: bool,
    exact_path: bool,
    class: Class,
}

struct Checks {
    failures: Vec<String>,
    kinds: u32,
    exact_pure: HashSet<u8>,
    saw_wrap: bool,
    saw_one: bool,
    saw_two: bool,
    sweep: HashMap<String, String>,
    long: usize,
    via: usize,
    deep: usize,
    long_lengths: BTreeMap<usize, usize>,
}

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
    let known = known_tokens();
    let wrapped = known_named(&known, "WETH");
    eprintln!("opening {}", data_dir.display());
    let catalog = Catalog::open(&data_dir).wrap_err("open catalog")?;
    eprintln!("scanning conduits");
    let survey = survey(&catalog)?;
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
    note_degrees(&survey.graph, &known);
    note_hubs(&census, &known);
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
        catalog: &catalog,
        cache: RefCell::new(HashMap::new()),
        graph: &survey.graph,
        incoming: &incoming,
    };
    eprintln!("checking pool identity on the candidate paths");
    let long = keep_disjoint(&survey.graph, &incoming, &reader, &selection.long, 24);
    let via = keep_disjoint(&survey.graph, &incoming, &reader, &selection.via_hub, 12);
    let isolated = keep_disjoint(&survey.graph, &incoming, &reader, &selection.isolated, 4);
    eprintln!(
        "kept long {} via {} isolated {}",
        long.len(),
        via.len(),
        isolated.len()
    );
    let hub_set: HashSet<Token> = hubs.iter().copied().collect();
    let picks = picks(&survey, &survey.graph, &incoming, &hub_set, wrapped);
    let scenarios = scenarios(&Chosen {
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
    push_header(
        &mut out,
        &survey,
        &known,
        &hubs,
        &selection,
        &census,
        &reach_rows,
    );
    let mut checks = Checks {
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
        match prepare(scenario.trade.clone(), &scenario.hubs, scenario.wrapped) {
            Ok(query) => {
                let set = execute(&query, &reader);
                record(
                    &mut checks,
                    scenario,
                    &set,
                    &known,
                    &survey.graph,
                    &incoming,
                );
                push_set(
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
    finish_checks(&mut checks, &survey, &selection);
    push_checks(&mut out, &checks);
    let mut stdout = io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    if checks.failures.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!("{} checks failed", checks.failures.len()))
    }
}

fn survey(catalog: &Catalog) -> eyre::Result<Survey> {
    let mut graph = Graph::default();
    let mut conduits = 0_u64;
    let mut pure: HashMap<u8, Bag> = HashMap::new();
    let mut carrier: HashMap<u8, Example> = HashMap::new();
    let mut by_width: HashMap<u32, Example> = HashMap::new();
    catalog.visit_conduits(|token_in, token_out, segments| {
        if segments.is_empty() {
            return Ok(());
        }
        conduits += 1;
        if conduits.is_multiple_of(100_000) {
            eprintln!("scan {conduits}");
        }
        let mut kinds = 0_u32;
        let mut capacity = 0_u128;
        for segment in segments {
            kinds |= kind_bit(segment.action.kind);
            capacity = capacity.saturating_add(as_u128(&segment.width));
        }
        let from = token_of(token_in);
        let to = token_of(token_out);
        graph.push(from, to, kinds, capacity);
        if from == NATIVE || to == NATIVE || kinds == 0 {
            return Ok(());
        }
        let example = Example {
            from,
            to,
            kinds,
            capacity,
            segments: segments.len() as u32,
        };
        keep_width(&mut by_width, &example);
        for kind in 1..32_u8 {
            if kinds & kind_bit(kind) != 0 {
                keep_carrier(&mut carrier, kind, &example);
            }
        }
        if let Some(kind) = only_kind(kinds) {
            let bag = pure.entry(kind).or_insert_with(|| Bag {
                high: Vec::new(),
                low: Vec::new(),
            });
            insert_high(&mut bag.high, example.clone());
            insert_low(&mut bag.low, example);
        }
        Ok(())
    })?;
    Ok(Survey {
        graph,
        conduits,
        pure,
        carrier,
        by_width,
    })
}

fn keep_width(map: &mut HashMap<u32, Example>, example: &Example) {
    let width = example.kinds.count_ones();
    if width == 0 {
        return;
    }
    let replace = map
        .get(&width)
        .map(|kept| example.capacity > kept.capacity)
        .unwrap_or(true);
    if replace {
        map.insert(width, example.clone());
    }
}

fn keep_carrier(map: &mut HashMap<u8, Example>, kind: u8, example: &Example) {
    let replace = match map.get(&kind) {
        None => true,
        Some(kept) => {
            let fewer = example.kinds.count_ones() < kept.kinds.count_ones();
            let richer = example.kinds.count_ones() == kept.kinds.count_ones()
                && example.capacity > kept.capacity;
            fewer || richer
        }
    };
    if replace {
        map.insert(kind, example.clone());
    }
}

fn insert_high(bag: &mut Vec<Example>, example: Example) {
    bag.push(example);
    bag.sort_by(|left, right| {
        right
            .capacity
            .cmp(&left.capacity)
            .then(left.from.cmp(&right.from))
            .then(left.to.cmp(&right.to))
    });
    bag.truncate(12);
}

fn insert_low(bag: &mut Vec<Example>, example: Example) {
    if example.capacity == 0 {
        return;
    }
    bag.push(example);
    bag.sort_by(|left, right| {
        left.capacity
            .cmp(&right.capacity)
            .then(left.from.cmp(&right.from))
            .then(left.to.cmp(&right.to))
    });
    bag.truncate(12);
}

fn picks(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Picks {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut witness = Vec::new();
    let mut rich = Vec::new();
    for kind in &kinds {
        let Some(bag) = survey.pure.get(kind) else {
            continue;
        };
        let Some(chosen) = choose_witness(bag, hubs) else {
            continue;
        };
        if let Some(largest) = bag.high.first() {
            if largest.from != chosen.from || largest.to != chosen.to {
                rich.push((*kind, largest.clone()));
            }
        }
        witness.push((*kind, chosen));
    }
    let into_weth = bundle_into(survey, graph, incoming, hubs, weth);
    let out_of_weth = bundle_out(survey, graph, incoming, hubs, weth);
    let mut mixed = Vec::new();
    for kind in 1..15_u8 {
        if survey.pure.contains_key(&kind) {
            continue;
        }
        if let Some(example) = survey.carrier.get(&kind) {
            if example.kinds & kind_bit(kind) != 0 {
                mixed.push((kind, example.clone()));
            }
        }
    }
    Picks {
        witness,
        rich,
        into_weth,
        out_of_weth,
        mixed,
    }
}

fn choose_witness(bag: &Bag, hubs: &HashSet<Token>) -> Option<Example> {
    let mut ranked: Vec<&Example> = bag.low.iter().chain(bag.high.iter()).collect();
    ranked.sort_by(|left, right| {
        let left_leaf = u32::from(!leaf(left, hubs));
        let right_leaf = u32::from(!leaf(right, hubs));
        left_leaf
            .cmp(&right_leaf)
            .then(left.capacity.cmp(&right.capacity))
            .then(left.from.cmp(&right.from))
    });
    ranked.first().map(|example| (*example).clone())
}

fn leaf(example: &Example, hubs: &HashSet<Token>) -> bool {
    example.from != NATIVE
        && example.to != NATIVE
        && !hubs.contains(&example.from)
        && !hubs.contains(&example.to)
}

fn bundle_into(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Vec<(u8, Example)> {
    let exact = gather_into(survey, weth, |example| {
        sole(graph, incoming, example.from, hubs) == Some(weth)
    });
    if !exact.is_empty() {
        return exact;
    }
    gather_into(survey, weth, |_| true)
}

fn gather_into(
    survey: &Survey,
    weth: Token,
    pred: impl Fn(&Example) -> bool,
) -> Vec<(u8, Example)> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut used = HashSet::new();
    let mut found = Vec::new();
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let Some(example) = bag.low.iter().chain(bag.high.iter()).find(|example| {
            example.to == weth
                && example.from != NATIVE
                && example.from != weth
                && !used.contains(&example.from)
                && pred(example)
        }) else {
            continue;
        };
        used.insert(example.from);
        found.push((kind, example.clone()));
    }
    found
}

fn bundle_out(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Vec<(u8, Example)> {
    let exact = gather_out(survey, graph, incoming, hubs, weth, true);
    if !exact.is_empty() {
        return exact;
    }
    gather_out(survey, graph, incoming, hubs, weth, false)
}

fn gather_out(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
    exact: bool,
) -> Vec<(u8, Example)> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut used = HashSet::new();
    let mut found = Vec::new();
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let Some(example) = bag.low.iter().chain(bag.high.iter()).find(|example| {
            example.from == weth
                && example.to != NATIVE
                && !hubs.contains(&example.to)
                && !used.contains(&example.to)
                && (!exact || sole(graph, incoming, example.to, hubs) == Some(weth))
        }) else {
            continue;
        };
        used.insert(example.to);
        found.push((kind, example.clone()));
    }
    found
}

fn sole(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    hubs: &HashSet<Token>,
) -> Option<Token> {
    graph::sole_hub(graph, incoming, token, hubs)
}

fn scenarios(chosen: &Chosen<'_>) -> Vec<Scenario> {
    let Chosen {
        survey,
        known,
        hubs,
        wrapped,
        picks,
        selection,
        long,
        via,
        isolated,
    } = chosen;
    let mut out = Vec::new();
    out.extend(pair_scenarios(known, hubs, *wrapped));
    out.extend(native_scenarios(known, hubs, *wrapped));
    out.extend(basket_scenarios(known, hubs, *wrapped));
    out.extend(width_scenarios(survey, known, hubs, *wrapped));
    out.extend(kind_scenarios(picks, known, hubs, *wrapped));
    out.extend(native_kind_scenarios(survey, known, hubs, *wrapped));
    out.extend(bundle_scenarios(picks, hubs, *wrapped));
    out.extend(via_scenarios(via, known, hubs, *wrapped));
    out.extend(split_scenarios(
        via,
        &selection.deep_from,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(deep_scenarios(
        &survey.graph,
        &selection.deep_to,
        true,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(deep_scenarios(
        &survey.graph,
        &selection.deep_from,
        false,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(long_scenarios(long, hubs, *wrapped));
    out.extend(multi_scenarios(long, known, hubs, *wrapped));
    out.extend(isolated_scenarios(isolated, hubs, *wrapped));
    out
}

fn pair_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    let ether = amount(18);
    for from in hubs {
        for to in hubs {
            if from == to {
                continue;
            }
            let name = format!("pair-{}-{}", label(*from, known), label(*to, known));
            let sweep = (*from == wrapped && *to == known_named(known, "USDC"))
                .then(|| "weth-usdc".to_string());
            let mut scenario = single(
                name,
                "hub pair at 10^18 raw units".to_string(),
                *from,
                *to,
                ether.clone(),
                hubs,
                wrapped,
            );
            scenario.sweep = sweep;
            out.push(scenario);
        }
    }
    let weth = wrapped;
    let usdc = known_named(known, "USDC");
    for (suffix, exp) in [("1", 0_u32), ("gwei", 9), ("huge", 27)] {
        let mut scenario = single(
            format!("pair-WETH-USDC-{suffix}"),
            format!("hub pair at 10^{exp} raw units"),
            weth,
            usdc,
            amount(exp),
            hubs,
            wrapped,
        );
        scenario.sweep = Some("weth-usdc".to_string());
        out.push(scenario);
    }
    out
}

fn native_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    let ether = amount(18);
    for hub in hubs {
        if *hub == NATIVE {
            continue;
        }
        out.push(single(
            format!("native-{}", label(*hub, known)),
            format!("native ETH in, {} out", label(*hub, known)),
            NATIVE,
            *hub,
            ether.clone(),
            hubs,
            wrapped,
        ));
        if *hub == wrapped {
            continue;
        }
        out.push(single(
            format!("{}-native", label(*hub, known)),
            format!("{} in, native ETH out", label(*hub, known)),
            *hub,
            NATIVE,
            ether.clone(),
            hubs,
            wrapped,
        ));
    }
    out.push(single(
        "WETH-native",
        "wrapped ETH in, native ETH out".to_string(),
        wrapped,
        NATIVE,
        ether,
        hubs,
        wrapped,
    ));
    out
}

fn basket_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let ether = amount(18);
    let weth = wrapped;
    let usdc = known_named(known, "USDC");
    let usdt = known_named(known, "USDT");
    let dai = known_named(known, "DAI");
    let wbtc = known_named(known, "WBTC");
    vec![
        multi(
            "basket-weth-wbtc",
            "WETH and WBTC in, USDC and DAI out",
            vec![(weth, ether.clone()), (wbtc, ether.clone())],
            vec![usdc, dai],
            hubs,
            wrapped,
        ),
        multi(
            "basket-usdc",
            "USDC in, USDT and DAI out",
            vec![(usdc, ether.clone())],
            vec![usdt, dai],
            hubs,
            wrapped,
        ),
        multi(
            "basket-usdt",
            "USDT in, USDC and DAI out",
            vec![(usdt, ether.clone())],
            vec![usdc, dai],
            hubs,
            wrapped,
        ),
        multi(
            "basket-dai",
            "DAI in, USDC and USDT out",
            vec![(dai, ether.clone())],
            vec![usdc, usdt],
            hubs,
            wrapped,
        ),
        multi(
            "basket-three-in",
            "WETH, WBTC, and USDT in, USDC and DAI out",
            vec![
                (weth, ether.clone()),
                (wbtc, ether.clone()),
                (usdt, ether.clone()),
            ],
            vec![usdc, dai],
            hubs,
            wrapped,
        ),
        multi(
            "basket-four-out",
            "WETH in, USDC, USDT, DAI, and WBTC out",
            vec![(weth, ether)],
            vec![usdc, usdt, dai, wbtc],
            hubs,
            wrapped,
        ),
    ]
}

fn width_scenarios(
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut widths: Vec<u32> = survey.by_width.keys().copied().collect();
    widths.sort_unstable();
    let mut out = Vec::new();
    for width in widths {
        let Some(example) = survey.by_width.get(&width) else {
            continue;
        };
        let mut scenario = single(
            format!("mix-{width:02}"),
            format!(
                "direct conduit spans {width} venue kinds, {} -> {}, width {}",
                label(example.from, known),
                label(example.to, known),
                example.capacity
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(example.kinds);
        out.push(scenario);
    }
    out
}

fn kind_scenarios(picks: &Picks, known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (kind, example) in &picks.witness {
        let mut scenario = single(
            format!("kind-{}", kind_name(*kind)),
            format!(
                "pure {} conduit {} -> {} width {} segments {}",
                kind_name(*kind),
                label(example.from, known),
                label(example.to, known),
                example.capacity,
                example.segments
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(kind_bit(*kind));
        scenario.sweep = Some(format!("kind-{}", kind_name(*kind)));
        out.push(scenario);
        let mut wei = single(
            format!("kind-{}-wei", kind_name(*kind)),
            format!("same pure {} conduit at 1 raw unit", kind_name(*kind)),
            example.from,
            example.to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        wei.direct_kinds = Some(kind_bit(*kind));
        wei.sweep = Some(format!("kind-{}", kind_name(*kind)));
        out.push(wei);
    }
    for (kind, example) in &picks.rich {
        let mut scenario = single(
            format!("kind-rich-{}", kind_name(*kind)),
            format!(
                "largest pure {} conduit {} -> {} width {} segments {}",
                kind_name(*kind),
                label(example.from, known),
                label(example.to, known),
                example.capacity,
                example.segments
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(kind_bit(*kind));
        out.push(scenario);
    }
    for (kind, example) in &picks.mixed {
        let mut scenario = single(
            format!("kind-mixed-{}", kind_name(*kind)),
            format!(
                "{} appears inside a {}-kind conduit {} -> {}",
                kind_name(*kind),
                example.kinds.count_ones(),
                label(example.from, known),
                label(example.to, known)
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(example.kinds);
        out.push(scenario);
    }
    out
}

fn native_kind_scenarios(
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut out = Vec::new();
    let ether = amount(18);
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let mut examples = bag.low.iter().chain(bag.high.iter());
        if let Some(example) = examples
            .clone()
            .find(|example| example.from == wrapped && !hubs.contains(&example.to))
        {
            let mut scenario = single(
                format!("native-out-{}", kind_name(kind)),
                format!(
                    "native ETH in, {} out, through a pure {} conduit",
                    label(example.to, known),
                    kind_name(kind)
                ),
                NATIVE,
                example.to,
                ether.clone(),
                hubs,
                wrapped,
            );
            scenario.must_wrap = true;
            scenario.expect_pure = vec![(wrapped, example.to, kind_bit(kind))];
            out.push(scenario);
        }
        if let Some(example) = examples.find(|example| {
            example.to == wrapped && example.from != wrapped && !hubs.contains(&example.from)
        }) {
            let mut scenario = single(
                format!("native-in-{}", kind_name(kind)),
                format!(
                    "{} in, native ETH out, through a pure {} conduit",
                    label(example.from, known),
                    kind_name(kind)
                ),
                example.from,
                NATIVE,
                ether.clone(),
                hubs,
                wrapped,
            );
            scenario.must_wrap = true;
            scenario.expect_pure = vec![(example.from, wrapped, kind_bit(kind))];
            out.push(scenario);
        }
    }
    out
}

fn split_scenarios(
    via: &[PathEnds],
    deep: &[Deep],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut used = HashSet::new();
    for (index, path) in via.iter().enumerate() {
        if out.len() == 8 {
            break;
        }
        let Some(from) = path.path.first().copied() else {
            continue;
        };
        let Some(hub) = path.path.get(1).copied() else {
            continue;
        };
        let far = deep.iter().find(|item| {
            item.dist >= 2
                && item.hub == hub
                && item.token != from
                && item.token != hub
                && !used.contains(&item.token)
        });
        let Some(far) = far else {
            continue;
        };
        used.insert(far.token);
        let name = format!("split-{index:02}");
        let detail = format!(
            "{} spends one input. The outputs are {} and {}, and {} is {} conduits downstream of {}.",
            label(from, known),
            label(hub, known),
            label(far.token, known),
            label(far.token, known),
            far.dist,
            label(far.hub, known)
        );
        let mut scenario = multi(
            &name,
            &detail,
            vec![(from, BigUint::from(1_u32))],
            vec![hub, far.token],
            hubs,
            wrapped,
        );
        scenario.reach = vec![hub, far.token];
        scenario.paths = vec![path.path.clone(), far.path.clone()];
        scenario.show_path = true;
        out.push(scenario);
    }
    out
}

fn bundle_scenarios(picks: &Picks, hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    if picks.into_weth.len() >= 2 {
        let inputs = picks
            .into_weth
            .iter()
            .map(|(_, example)| (example.from, amount(6)))
            .collect();
        let expect_pure = picks
            .into_weth
            .iter()
            .map(|(kind, example)| (example.from, example.to, kind_bit(*kind)))
            .collect();
        let names = picks
            .into_weth
            .iter()
            .map(|(kind, _)| kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",");
        out.push(Scenario {
            name: "kinds-into-weth".into(),
            detail: format!(
                "{} pure conduits into WETH, one input each, amount 10^6 ({names})",
                picks.into_weth.len()
            ),
            trade: Trade {
                inputs,
                outputs: vec![wrapped],
            },
            hubs: hubs.to_vec(),
            wrapped: Some(wrapped),
            paths: Vec::new(),
            show_path: false,
            direct_kinds: None,
            expect_pure,
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: false,
            exact_path: false,
            class: Class::Ordinary,
        });
    }
    if picks.out_of_weth.len() >= 2 {
        let outputs = picks
            .out_of_weth
            .iter()
            .map(|(_, example)| example.to)
            .collect();
        let expect_pure = picks
            .out_of_weth
            .iter()
            .map(|(kind, example)| (example.from, example.to, kind_bit(*kind)))
            .collect();
        let names = picks
            .out_of_weth
            .iter()
            .map(|(kind, _)| kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",");
        out.push(Scenario {
            name: "kinds-out-of-weth".into(),
            detail: format!(
                "WETH in, {} pure conduit outputs ({names})",
                picks.out_of_weth.len()
            ),
            trade: Trade {
                inputs: vec![(wrapped, amount(18))],
                outputs,
            },
            hubs: hubs.to_vec(),
            wrapped: Some(wrapped),
            paths: Vec::new(),
            show_path: false,
            direct_kinds: None,
            expect_pure,
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: false,
            exact_path: false,
            class: Class::Ordinary,
        });
    }
    out
}

fn via_scenarios(
    via: &[PathEnds],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in via.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let mut scenario = single(
            format!("via-{index:02}"),
            format!(
                "shortest catalog path is 2 conduits through {}, kinds {}, min width {}",
                path.path
                    .get(1)
                    .map(|hub| label(*hub, known))
                    .unwrap_or_else(|| "-".into()),
                kind_list(path.kinds),
                path.min_capacity
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.sweep = (index == 0).then(|| "via-0".to_string());
        scenario.class = Class::Via;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.exact_path = true;
        out.push(scenario);
    }
    if let Some(path) = via.first() {
        let Some((from, to)) = path_ends(&path.path) else {
            return out;
        };
        let mut scenario = single(
            "via-00-ether",
            "same one-hub pair at 10^18 raw units".to_string(),
            from,
            to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.sweep = Some("via-0".to_string());
        scenario.class = Class::Via;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.exact_path = true;
        out.push(scenario);
    }
    out
}

fn deep_scenarios(
    graph: &Graph,
    deep: &[Deep],
    toward_hub: bool,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut native_at = HashSet::new();
    for item in deep {
        let hub_name = label(item.hub, known);
        let (name, detail, from, to) = if toward_hub {
            (
                format!("deep-to-{hub_name}-d{:02}", item.dist),
                format!(
                    "shortest catalog path to {hub_name} is {} conduits, min width {}, kinds {}",
                    item.dist,
                    item.min_capacity,
                    kind_list(item.kinds)
                ),
                item.token,
                item.hub,
            )
        } else {
            (
                format!("deep-from-{hub_name}-d{:02}", item.dist),
                format!(
                    "shortest catalog path from {hub_name} is {} conduits, min width {}, kinds {}",
                    item.dist,
                    item.min_capacity,
                    kind_list(item.kinds)
                ),
                item.hub,
                item.token,
            )
        };
        let mut scenario = single(name, detail, from, to, BigUint::from(1_u32), hubs, wrapped);
        scenario.class = Class::Deep;
        scenario.paths = vec![item.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
        if item.hub == wrapped && native_at.insert(item.dist) {
            let (name, from, to, detail) = if toward_hub {
                (
                    format!("deep-to-native-d{:02}", item.dist),
                    item.token,
                    NATIVE,
                    format!(
                        "native ETH out, catalog path to WETH is {} conduits",
                        item.dist
                    ),
                )
            } else {
                (
                    format!("native-deep-d{:02}", item.dist),
                    NATIVE,
                    item.token,
                    format!(
                        "native ETH in, catalog path from WETH is {} conduits",
                        item.dist
                    ),
                )
            };
            if graph.meta(from, to).is_some() {
                continue;
            }
            let mut scenario = single(name, detail, from, to, amount(18), hubs, wrapped);
            scenario.class = Class::Deep;
            scenario.paths = vec![item.path.clone()];
            scenario.show_path = true;
            scenario.cover = true;
            out.push(scenario);
        }
    }
    out
}

fn long_scenarios(long: &[PathEnds], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in long.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let conduits = path.path.len().saturating_sub(1);
        let cross = if path.cross_hub {
            " The path uses a conduit between two hubs."
        } else {
            ""
        };
        let through_native = if path.path.contains(&NATIVE) {
            " The shortest path uses native ETH."
        } else {
            ""
        };
        let mut scenario = single(
            format!("long-{conduits:02}-{index:02}"),
            format!(
                "shortest catalog path is {conduits} conduits. Minimum conduit width is {}. Tendril kinds are {}. Endpoint pools are disjoint.{cross}{through_native}",
                path.min_capacity,
                kind_list(path.tendril_kinds)
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.class = Class::Long;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
        if index < 3 && path.min_capacity > 1 {
            let mut spent = single(
                format!("long-spend-{conduits:02}-{index:02}"),
                format!(
                    "same ends, input amount is the minimum conduit width {}",
                    path.min_capacity
                ),
                from,
                to,
                BigUint::from(path.min_capacity),
                hubs,
                wrapped,
            );
            spent.paths = vec![path.path.clone()];
            spent.show_path = true;
            spent.cover = true;
            out.push(spent);
        }
    }
    out
}

fn multi_scenarios(
    long: &[PathEnds],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, group) in group_by(long, true).into_iter().take(4).enumerate() {
        push_multi(&mut out, &group, true, index, known, hubs, wrapped);
    }
    for (index, group) in group_by(long, false).into_iter().take(4).enumerate() {
        push_multi(&mut out, &group, false, index, known, hubs, wrapped);
    }
    out
}

fn push_multi(
    out: &mut Vec<Scenario>,
    group: &[&PathEnds],
    shared_dest: bool,
    index: usize,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) {
    let paths: Vec<Vec<Token>> = group.iter().take(3).map(|path| path.path.clone()).collect();
    if paths.len() < 2 {
        return;
    }
    let amounts = [
        BigUint::from(1_u32),
        BigUint::from(1_000_000_u32),
        amount(18),
    ];
    let scenario = if shared_dest {
        let dest = *paths[0].last().expect("dest");
        let inputs = paths
            .iter()
            .enumerate()
            .map(|(nth, path)| (path[0], amounts[nth].clone()))
            .collect();
        Scenario {
            name: format!("long-in-{}-{index:02}", paths.len()),
            detail: format!(
                "{} low-liquidity inputs, one output {}, each catalog path is longer than two hubs",
                paths.len(),
                label(dest, known)
            ),
            trade: Trade {
                inputs,
                outputs: vec![dest],
            },
            hubs: hubs.to_vec(),
            wrapped: Some(wrapped),
            paths,
            show_path: true,
            direct_kinds: None,
            expect_pure: Vec::new(),
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: true,
            exact_path: false,
            class: Class::Long,
        }
    } else {
        let source = paths[0][0];
        let outputs = paths
            .iter()
            .filter_map(|path| path.last().copied())
            .collect();
        Scenario {
            name: format!("long-out-{}-{index:02}", paths.len()),
            detail: format!(
                "{} in, {} low-liquidity outputs, each catalog path is longer than two hubs",
                label(source, known),
                paths.len()
            ),
            trade: Trade {
                inputs: vec![(source, amount(18))],
                outputs,
            },
            hubs: hubs.to_vec(),
            wrapped: Some(wrapped),
            paths,
            show_path: true,
            direct_kinds: None,
            expect_pure: Vec::new(),
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: true,
            exact_path: false,
            class: Class::Long,
        }
    };
    out.push(scenario);
}

fn group_by(paths: &[PathEnds], by_dest: bool) -> Vec<Vec<&PathEnds>> {
    let mut order = Vec::new();
    let mut groups: HashMap<Token, Vec<&PathEnds>> = HashMap::new();
    for path in paths {
        let Some(key) = (if by_dest {
            path.path.last()
        } else {
            path.path.first()
        })
        .copied() else {
            continue;
        };
        if !groups.contains_key(&key) {
            order.push(key);
        }
        groups.entry(key).or_default().push(path);
    }
    let mut found = Vec::new();
    for key in order {
        let Some(group) = groups.remove(&key) else {
            continue;
        };
        let mut seen = HashSet::new();
        let distinct: Vec<&PathEnds> = group
            .into_iter()
            .filter(|path| {
                let mark = if by_dest {
                    path.path.first().copied()
                } else {
                    path.path.last().copied()
                };
                mark.map(|token| seen.insert(token)).unwrap_or(false)
            })
            .collect();
        if distinct.len() >= 2 {
            found.push(distinct);
        }
    }
    found
}

fn isolated_scenarios(isolated: &[PathEnds], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in isolated.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let conduits = path.path.len().saturating_sub(1);
        let mut scenario = single(
            format!("isolated-{index:02}"),
            format!(
                "component apart from the hubs, shortest path is {conduits} conduits, kinds {}",
                kind_list(path.kinds)
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.class = Class::Isolated;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
    }
    out
}

fn single(
    name: impl Into<String>,
    detail: impl Into<String>,
    from: Token,
    to: Token,
    value: BigUint,
    hubs: &[Token],
    wrapped: Token,
) -> Scenario {
    Scenario {
        name: name.into(),
        detail: detail.into(),
        trade: Trade {
            inputs: vec![(from, value)],
            outputs: vec![to],
        },
        hubs: hubs.to_vec(),
        wrapped: Some(wrapped),
        paths: Vec::new(),
        show_path: false,
        direct_kinds: None,
        expect_pure: Vec::new(),
        must_empty: false,
        expect_routes: None,
        sweep: None,
        require_end: None,
        forbid_ends: Vec::new(),
        reach: Vec::new(),
        must_wrap: false,
        cover: false,
        exact_path: false,
        class: Class::Ordinary,
    }
}

fn multi(
    name: &str,
    detail: &str,
    inputs: Vec<(Token, BigUint)>,
    outputs: Vec<Token>,
    hubs: &[Token],
    wrapped: Token,
) -> Scenario {
    Scenario {
        name: name.into(),
        detail: detail.into(),
        trade: Trade { inputs, outputs },
        hubs: hubs.to_vec(),
        wrapped: Some(wrapped),
        paths: Vec::new(),
        show_path: false,
        direct_kinds: None,
        expect_pure: Vec::new(),
        must_empty: false,
        expect_routes: None,
        sweep: None,
        require_end: None,
        forbid_ends: Vec::new(),
        reach: Vec::new(),
        must_wrap: false,
        cover: false,
        exact_path: false,
        class: Class::Ordinary,
    }
}

fn keep_disjoint(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_>,
    paths: &[PathEnds],
    limit: usize,
) -> Vec<PathEnds> {
    let mut kept = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if kept.len() == limit {
            break;
        }
        if index.is_multiple_of(10) {
            eprintln!("pool check {index}/{}", paths.len());
        }
        if path_pools_ok(graph, incoming, reader, &path.path) {
            kept.push(path.clone());
        }
    }
    kept
}

fn path_pools_ok(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_>,
    path: &[Token],
) -> bool {
    if path.len() < 2 {
        return false;
    }
    let mut hops = Vec::new();
    for pair in path.windows(2) {
        let Some(pools) = pools_of(reader, pair[0], pair[1]) else {
            return false;
        };
        hops.push(pools);
    }
    for (index, hop) in hops.iter().enumerate() {
        for later in &hops[index + 1..] {
            if !hop.is_disjoint(later) {
                return false;
            }
        }
    }
    let Some(left) = incident_pools(graph, incoming, reader, path[0]) else {
        return false;
    };
    let Some(right) = incident_pools(graph, incoming, reader, path[path.len() - 1]) else {
        return false;
    };
    if !left.is_disjoint(&right) {
        return false;
    }
    let last = hops.len() - 1;
    !shares_foreign(&left, &hops, 0) && !shares_foreign(&right, &hops, last)
}

fn shares_foreign(side: &HashSet<[u8; 32]>, hops: &[HashSet<[u8; 32]>], own: usize) -> bool {
    hops.iter()
        .enumerate()
        .any(|(index, hop)| index != own && !side.is_disjoint(hop))
}

fn incident_pools(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_>,
    token: Token,
) -> Option<HashSet<[u8; 32]>> {
    let indeg = incoming.get(&token).map(|preds| preds.len()).unwrap_or(0);
    if graph.degree(token) + indeg > 12 {
        return None;
    }
    let mut pools = HashSet::new();
    if let Some(links) = graph.out.get(&token) {
        for link in links {
            pools.extend(pools_of(reader, token, link.to)?);
        }
    }
    if let Some(preds) = incoming.get(&token) {
        for pred in preds {
            pools.extend(pools_of(reader, *pred, token)?);
        }
    }
    Some(pools)
}

fn pools_of(reader: &Reader<'_>, from: Token, to: Token) -> Option<HashSet<[u8; 32]>> {
    let segments = reader.conduit(from, to)?;
    let pools: HashSet<_> = segments.iter().map(|segment| segment.action.pool).collect();
    if pools.is_empty() {
        None
    } else {
        Some(pools)
    }
}

type CurveCache = HashMap<(Token, Token), Option<Vec<SlopeSegment>>>;

struct Reader<'a> {
    catalog: &'a Catalog,
    cache: RefCell<CurveCache>,
    graph: &'a Graph,
    incoming: &'a HashMap<Token, Vec<Token>>,
}

impl Conduits for Reader<'_> {
    fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>> {
        let key = (token_in, token_out);
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let loaded = self
            .catalog
            .read_conduit(
                Address::from_slice(&token_in.0),
                Address::from_slice(&token_out.0),
            )
            .ok()
            .flatten()
            .map(|stored| stored.segments)
            .filter(|segments| !segments.is_empty());
        self.cache.borrow_mut().insert(key, loaded.clone());
        loaded
    }

    fn outgoing(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
        if self.graph.degree(token) > limit {
            return None;
        }
        let mut tokens: Vec<Token> = self.graph.neighbors(token).into_iter().collect();
        tokens.sort();
        if tokens.len() > limit {
            None
        } else {
            Some(tokens)
        }
    }

    fn incoming(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
        let Some(preds) = self.incoming.get(&token) else {
            return Some(Vec::new());
        };
        if preds.len() > limit {
            None
        } else {
            Some(preds.clone())
        }
    }
}

fn push_header(
    out: &mut String,
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    selection: &Selection,
    census: &HubCensus,
    reach_rows: &[(usize, HubReach)],
) {
    out.push_str(&format!(
        "conduits {}\ntokens with an outgoing curve {}\ntokens named by a conduit {}\nhub minimum {HUB_MINIMUM}\nquery hubs {QUERY_HUBS}\nneighbor limit {NEIGHBOR_LIMIT}\nwalk depth {WALK_DEPTH}\n",
        survey.conduits,
        survey.graph.out.len(),
        census.tokens
    ));
    for (floor, count) in &census.at_least {
        out.push_str(&format!("tokens with at least {floor} conduits: {count}\n"));
    }
    for limit in HUB_LIMITS {
        let Some(hub) = census.ranked.get(limit - 1) else {
            continue;
        };
        out.push_str(&format!(
            "hub rank {limit} {} conduits {}\n",
            label(hub.token, known),
            hub.conduits
        ));
    }
    out.push_str("hubs");
    for hub in hubs {
        let conduits = census
            .ranked
            .iter()
            .find(|score| score.token == *hub)
            .map(|score| score.conduits)
            .unwrap_or(0);
        out.push_str(&format!(
            " {} conduits={conduits} out={}",
            label(*hub, known),
            survey.graph.degree(*hub)
        ));
    }
    out.push('\n');
    for (limit, reach) in reach_rows {
        out.push_str(&format!(
            "reach K={limit} both {} toward {} from {} outside {}\n",
            reach.both, reach.toward, reach.from_hub, reach.outside
        ));
    }
    let mut pure: Vec<u8> = survey.pure.keys().copied().collect();
    pure.sort_unstable();
    out.push_str("pure kinds");
    for kind in pure {
        out.push(' ');
        out.push_str(kind_name(kind));
    }
    out.push('\n');
    let mut widths: Vec<u32> = survey.by_width.keys().copied().collect();
    widths.sort_unstable();
    out.push_str("mix widths");
    for width in widths {
        out.push_str(&format!(" {width}"));
    }
    out.push('\n');
    out.push_str(&format!(
        "reach both {} toward {} from {} outside {}\n",
        selection.reached_both,
        selection.only_toward_hub,
        selection.only_from_hub,
        selection.outside
    ));
    out.push_str(&format!(
        "outside component edges {} longest directed distance {}\n",
        selection.outside_edges, selection.outside_span
    ));
    out.push_str(&format!(
        "farthest token toward a hub {} (small {})\nfarthest token from a hub {} (small {})\n",
        selection.max_to_hub,
        selection.max_to_hub_small,
        selection.max_from_hub,
        selection.max_from_hub_small
    ));
    out.push_str(&format!(
        "small toward hub {}\nsmall from hub {}\n",
        hist_line(&selection.to_hist),
        hist_line(&selection.from_hist)
    ));
    out.push_str(&format!(
        "kinds on small tendrils of 4 or more conduits toward a hub: {}\n",
        kind_list(selection.far_to_kinds)
    ));
    out.push_str(&format!(
        "kinds on small tendrils of 4 or more conduits from a hub: {}\n\n",
        kind_list(selection.far_from_kinds)
    ));
}

fn hist_line(hist: &BTreeMap<u32, u64>) -> String {
    if hist.is_empty() {
        return "-".into();
    }
    hist.iter()
        .map(|(dist, count)| format!("{dist}:{count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_set(
    out: &mut String,
    scenario: &Scenario,
    set: &RouteSet,
    known: &[Known],
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_>,
) {
    out.push_str(&format!("== {}\n{}\n", scenario.name, scenario.detail));
    for (token, value) in &scenario.trade.inputs {
        out.push_str(&format!("in {} {}\n", label(*token, known), value));
    }
    out.push_str("out");
    for token in &scenario.trade.outputs {
        out.push(' ');
        out.push_str(&label(*token, known));
    }
    out.push('\n');
    if scenario.show_path {
        for (nth, path) in scenario.paths.iter().enumerate() {
            out.push_str(&format!("catalog path {nth}\n"));
            if let Some(first) = path.first() {
                out.push_str(&format!("  {}\n", end_line(graph, incoming, *first, known)));
            }
            if let Some(last) = path.last() {
                out.push_str(&format!("  {}\n", end_line(graph, incoming, *last, known)));
            }
            for (index, pair) in path.windows(2).enumerate() {
                out.push_str(&format!(
                    "  {index} {}\n",
                    hop_line(graph, reader, pair[0], pair[1], known)
                ));
            }
        }
    }
    let shapes = shapes(set);
    out.push_str(&format!(
        "routes {} edges {}\nshapes direct {} one {} two {} longer {} wrap {}\n",
        set.routes.len(),
        set.edges.len(),
        shapes.direct,
        shapes.one,
        shapes.two,
        shapes.longer,
        shapes.wrap
    ));
    let mut kinds = 0_u32;
    for (index, edge) in set.edges.iter().enumerate() {
        let edge_kinds = curve_kinds(&edge.curve);
        kinds |= edge_kinds;
        out.push_str(&format!(
            "  edge {index} {} -> {} {}\n",
            label(edge.token_in, known),
            label(edge.token_out, known),
            curve_label(&edge.curve)
        ));
    }
    for (index, route) in set.routes.iter().enumerate() {
        out.push_str(&format!(
            "  route {index} {}\n",
            route_line(set, route, known)
        ));
    }
    out.push_str(&format!("kinds {}\n\n", kind_list(kinds)));
}

fn end_line(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    known: &[Known],
) -> String {
    let indeg = incoming.get(&token).map(|preds| preds.len()).unwrap_or(0);
    format!(
        "{} out {} in {} width {}",
        label(token, known),
        graph.degree(token),
        indeg,
        graph.capacity(token)
    )
}

fn hop_line(graph: &Graph, reader: &Reader<'_>, from: Token, to: Token, known: &[Known]) -> String {
    let meta = graph.meta(from, to);
    let loaded = reader.conduit(from, to);
    let segments = loaded.as_ref().map(|items| items.len()).unwrap_or(0);
    let pools = loaded
        .as_ref()
        .map(|items| {
            items
                .iter()
                .map(|segment| segment.action.pool)
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0);
    let (kinds, capacity) = meta
        .map(|meta| (kind_list(meta.kinds), meta.capacity.to_string()))
        .unwrap_or_else(|| ("-".into(), "-".into()));
    format!(
        "{} -> {} width {capacity} kinds {kinds} segments {segments} pools {pools}",
        label(from, known),
        label(to, known)
    )
}

struct Shapes {
    direct: usize,
    one: usize,
    two: usize,
    longer: usize,
    wrap: usize,
}

fn shapes(set: &RouteSet) -> Shapes {
    let mut shapes = Shapes {
        direct: 0,
        one: 0,
        two: 0,
        longer: 0,
        wrap: 0,
    };
    for route in &set.routes {
        let mut content = 0_usize;
        let mut wrap = false;
        for index in &route.edges {
            if matches!(set.edges[*index].curve, Curve::Wrap) {
                wrap = true;
            } else {
                content += 1;
            }
        }
        if wrap {
            shapes.wrap += 1;
        }
        match content {
            1 => shapes.direct += 1,
            2 => shapes.one += 1,
            3 => shapes.two += 1,
            n if n > 3 => shapes.longer += 1,
            _ => {}
        }
    }
    shapes
}

fn route_end(set: &RouteSet, route: &Route) -> Option<Token> {
    let index = *route.edges.last()?;
    set.edges.get(index).map(|edge| edge.token_out)
}

fn route_line(set: &RouteSet, route: &Route, known: &[Known]) -> String {
    let Some(first) = route.edges.first() else {
        return "-".into();
    };
    let mut parts = vec![label(set.edges[*first].token_in, known)];
    for index in &route.edges {
        parts.push(label(set.edges[*index].token_out, known));
    }
    parts.join(" -> ")
}

fn record(
    checks: &mut Checks,
    scenario: &Scenario,
    set: &RouteSet,
    known: &[Known],
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
) {
    for edge in &set.edges {
        let bits = curve_kinds(&edge.curve);
        checks.kinds |= bits;
        if let Some(kind) = only_kind(bits) {
            checks.exact_pure.insert(kind);
        }
        if matches!(edge.curve, Curve::Wrap) {
            checks.saw_wrap = true;
        }
    }
    let shapes = shapes(set);
    checks.saw_one |= shapes.one > 0;
    checks.saw_two |= shapes.two > 0;
    if scenario.must_empty && !set.routes.is_empty() {
        checks.failures.push(format!(
            "{} returned {} routes",
            scenario.name,
            set.routes.len()
        ));
    }
    if let Some(want) = scenario.expect_routes {
        if set.routes.len() != want {
            checks.failures.push(format!(
                "{} routes {} want {want}",
                scenario.name,
                set.routes.len()
            ));
        }
    }
    if scenario.cover {
        for path in &scenario.paths {
            if !path_covered(scenario, set, path) {
                checks.failures.push(format!(
                    "{} has no route across its catalog path of {} conduits ({})",
                    scenario.name,
                    path.len().saturating_sub(1),
                    path_degrees(graph, incoming, &scenario.hubs, path, known)
                ));
            }
        }
    }
    if scenario.exact_path {
        let matched = scenario.paths.iter().any(|path| {
            set.routes
                .iter()
                .any(|route| route_tokens(set, route) == path.as_slice())
        });
        if !matched {
            checks.failures.push(format!(
                "{} does not contain its catalog path",
                scenario.name
            ));
        }
    }
    for token in &scenario.reach {
        let reached = set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(*token));
        if !reached {
            checks.failures.push(format!(
                "{} never reaches {}",
                scenario.name,
                label(*token, known)
            ));
        }
    }
    if let Some(expected) = scenario.direct_kinds {
        let from = scenario.trade.inputs[0].0;
        let to = scenario.trade.outputs[0];
        match edge_kinds(set, from, to) {
            Some(found) if found == expected => {}
            Some(found) => checks.failures.push(format!(
                "{} direct kinds {} want {}",
                scenario.name,
                kind_list(found),
                kind_list(expected)
            )),
            None => checks
                .failures
                .push(format!("{} missing direct conduit", scenario.name)),
        }
    }
    for (from, to, kinds) in &scenario.expect_pure {
        match edge_kinds(set, *from, *to) {
            Some(found) if found == *kinds => {}
            Some(found) => checks.failures.push(format!(
                "{} {} -> {} kinds {} want {}",
                scenario.name,
                label(*from, known),
                label(*to, known),
                kind_list(found),
                kind_list(*kinds)
            )),
            None => checks.failures.push(format!(
                "{} missing {} -> {}",
                scenario.name,
                label(*from, known),
                label(*to, known)
            )),
        }
    }
    if let Some(token) = scenario.require_end {
        let reached = set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(token));
        if !reached {
            checks.failures.push(format!(
                "{} never reaches {}",
                scenario.name,
                label(token, known)
            ));
        }
    }
    for token in &scenario.forbid_ends {
        if set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(*token))
        {
            checks.failures.push(format!(
                "{} reaches {}",
                scenario.name,
                label(*token, known)
            ));
        }
    }
    if scenario.must_wrap
        && !set
            .edges
            .iter()
            .any(|edge| matches!(edge.curve, Curve::Wrap))
    {
        checks
            .failures
            .push(format!("{} has no wrap", scenario.name));
    }
    if let Some(group) = &scenario.sweep {
        let signature = topology(set);
        if let Some(previous) = checks.sweep.get(group) {
            if previous != &signature {
                checks
                    .failures
                    .push(format!("{} amount changed the route set", scenario.name));
            }
        } else {
            checks.sweep.insert(group.clone(), signature);
        }
    }
    match scenario.class {
        Class::Long if scenario.paths.len() == 1 => {
            checks.long += 1;
            let conduits = scenario.paths[0].len().saturating_sub(1);
            *checks.long_lengths.entry(conduits).or_default() += 1;
        }
        Class::Via => checks.via += 1,
        Class::Deep => checks.deep += 1,
        _ => {}
    }
}

fn finish_checks(checks: &mut Checks, survey: &Survey, selection: &Selection) {
    for kind in 1..15_u8 {
        if checks.kinds & kind_bit(kind) == 0 {
            checks.failures.push(format!(
                "venue kind {} is absent from every route set",
                kind_name(kind)
            ));
        }
    }
    let mut pure: Vec<u8> = survey.pure.keys().copied().collect();
    pure.sort_unstable();
    for kind in pure {
        if !checks.exact_pure.contains(&kind) {
            checks.failures.push(format!(
                "pure {} never appears as its own conduit",
                kind_name(kind)
            ));
        }
    }
    if !checks.saw_wrap {
        checks.failures.push("no route uses the wrap".into());
    }
    if !checks.saw_one {
        checks.failures.push("no one-hub route".into());
    }
    if !checks.saw_two {
        checks.failures.push("no two-hub route".into());
    }
    if checks.via == 0 {
        checks.failures.push("no one-hub leaf pair".into());
    }
    if checks.long == 0 {
        checks
            .failures
            .push("no long memecoin pair survived the pool check".into());
    }
    if selection.max_to_hub_small >= 2 && selection.deep_to.is_empty() {
        checks.failures.push(
            "small tokens sit two or more conduits from a hub, and no deep scenario was built"
                .into(),
        );
    }
    if selection.max_from_hub_small >= 2 && selection.deep_from.is_empty() {
        checks.failures.push(
            "small tokens sit two or more conduits out from a hub, and no deep scenario was built"
                .into(),
        );
    }
}

fn push_checks(out: &mut String, checks: &Checks) {
    out.push_str("-- checks\n");
    out.push_str(&format!(
        "kinds {}\nexact pure {}\nlong paths {} lengths {}\nvia {} deep {}\nwrap {} one-hub {} two-hub {}\n",
        kind_list(checks.kinds),
        checks.exact_pure.len(),
        checks.long,
        checks
            .long_lengths
            .iter()
            .map(|(len, count)| format!("{len}:{count}"))
            .collect::<Vec<_>>()
            .join(" "),
        checks.via,
        checks.deep,
        checks.saw_wrap,
        checks.saw_one,
        checks.saw_two
    ));
    if checks.failures.is_empty() {
        out.push_str("ok\n");
    } else {
        for failure in &checks.failures {
            out.push_str("fail ");
            out.push_str(failure);
            out.push('\n');
        }
    }
}

fn path_degrees(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &[Token],
    path: &[Token],
    known: &[Known],
) -> String {
    path.iter()
        .map(|token| {
            let indeg = incoming.get(token).map(|preds| preds.len()).unwrap_or(0);
            let mark = if hubs.contains(token) { "h" } else { "" };
            format!(
                "{}:{}/{}{mark}",
                label(*token, known),
                graph.degree(*token),
                indeg
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn route_tokens(set: &RouteSet, route: &Route) -> Vec<Token> {
    let Some(first) = route.edges.first() else {
        return Vec::new();
    };
    let mut tokens = vec![set.edges[*first].token_in];
    for index in &route.edges {
        if let Some(edge) = set.edges.get(*index) {
            tokens.push(edge.token_out);
        }
    }
    tokens
}

fn path_covered(scenario: &Scenario, set: &RouteSet, path: &[Token]) -> bool {
    let (Some(&catalog_start), Some(&catalog_end)) = (path.first(), path.last()) else {
        return false;
    };
    let mut start = catalog_start;
    let mut end = catalog_end;
    let mut budget = path.len().saturating_sub(1);
    if scenario
        .trade
        .inputs
        .iter()
        .any(|(token, _)| *token == NATIVE)
        && start != NATIVE
        && scenario.wrapped == Some(start)
    {
        start = NATIVE;
        budget += 1;
    }
    if scenario.trade.outputs.contains(&NATIVE) && end != NATIVE && scenario.wrapped == Some(end) {
        end = NATIVE;
        budget += 1;
    }
    set.routes.iter().any(|route| {
        if route.edges.len() > budget {
            return false;
        }
        let tokens = route_tokens(set, route);
        tokens.first().copied() == Some(start) && tokens.last().copied() == Some(end)
    })
}

fn edge_kinds(set: &RouteSet, from: Token, to: Token) -> Option<u32> {
    set.edges
        .iter()
        .find(|edge| edge.token_in == from && edge.token_out == to)
        .map(|edge| curve_kinds(&edge.curve))
}

fn topology(set: &RouteSet) -> String {
    let mut signature = String::new();
    for edge in &set.edges {
        signature.push_str(&hex(edge.token_in));
        signature.push('>');
        signature.push_str(&hex(edge.token_out));
        signature.push('|');
    }
    signature.push(';');
    for route in &set.routes {
        for index in &route.edges {
            signature.push_str(&index.to_string());
            signature.push(',');
        }
        signature.push(';');
    }
    signature
}

fn path_ends(path: &[Token]) -> Option<(Token, Token)> {
    Some((*path.first()?, *path.last()?))
}

fn curve_label(curve: &Curve) -> String {
    match curve {
        Curve::Wrap => "wrap".into(),
        Curve::Conduit(segments) => format!(
            "conduit segments {} kinds {}",
            segments.len(),
            kind_list(curve_kinds(curve))
        ),
    }
}

fn curve_kinds(curve: &Curve) -> u32 {
    match curve {
        Curve::Wrap => 0,
        Curve::Conduit(segments) => segments
            .iter()
            .fold(0_u32, |bits, segment| bits | kind_bit(segment.action.kind)),
    }
}

fn kind_bit(kind: u8) -> u32 {
    if kind == 0 || kind >= 32 {
        return 0;
    }
    1_u32 << (kind - 1)
}

fn only_kind(bits: u32) -> Option<u8> {
    if bits.count_ones() == 1 {
        Some(bits.trailing_zeros() as u8 + 1)
    } else {
        None
    }
}

fn kind_list(bits: u32) -> String {
    if bits == 0 {
        return "-".into();
    }
    (1..32_u8)
        .filter(|kind| bits & kind_bit(*kind) != 0)
        .map(kind_name)
        .collect::<Vec<_>>()
        .join(",")
}

fn as_u128(value: &BigInt) -> u128 {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 16 {
        return if sign == Sign::Minus { 0 } else { u128::MAX };
    }
    let mut buf = [0_u8; 16];
    buf[16 - bytes.len()..].copy_from_slice(&bytes);
    u128::from_be_bytes(buf)
}

fn token_of(address: Address) -> Token {
    let mut raw = [0_u8; 20];
    raw.copy_from_slice(address.as_slice());
    Token(raw)
}

fn known_tokens() -> Vec<Known> {
    vec![
        Known {
            name: "ETH",
            token: NATIVE,
        },
        named("WETH", WETH),
        named("USDC", USDC),
        named("USDT", USDT),
        named("DAI", DAI),
        named("WBTC", WBTC),
    ]
}

fn named(name: &'static str, hex_addr: &str) -> Known {
    let address = Address::from_str(hex_addr).expect("address");
    Known {
        name,
        token: token_of(address),
    }
}

fn known_named(known: &[Known], name: &str) -> Token {
    known
        .iter()
        .find(|item| item.name == name)
        .map(|item| item.token)
        .expect(name)
}

fn label(token: Token, known: &[Known]) -> String {
    if let Some(item) = known.iter().find(|item| item.token == token) {
        return item.name.into();
    }
    hex(token)
}

fn hex(token: Token) -> String {
    let mut out = String::from("0x");
    for byte in token.0 {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn amount(exp: u32) -> BigUint {
    BigUint::from(10_u32).pow(exp)
}

fn note_hubs(census: &HubCensus, known: &[Known]) {
    eprint!("ranked");
    for hub in census.ranked.iter().take(QUERY_HUBS) {
        eprint!(" {}={}", label(hub.token, known), hub.conduits);
    }
    eprintln!();
    for (floor, count) in &census.at_least {
        eprintln!("tokens with at least {floor} conduits: {count}");
    }
}

fn note_degrees(graph: &Graph, known: &[Known]) {
    let mut ranks: Vec<(usize, Token)> = graph
        .out
        .iter()
        .map(|(token, links)| (links.len(), *token))
        .collect();
    ranks.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    ranks.truncate(12);
    eprint!("widest");
    for (degree, token) in ranks {
        eprint!(" {}={degree}", label(token, known));
    }
    eprintln!();
}
