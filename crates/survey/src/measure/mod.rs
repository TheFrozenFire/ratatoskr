//! Allocate each prepared trade and score the plan.
//!
//! Prices are the first slope of a conduit into the wrapped token. They are
//! applied after [`optimizer::optimize`] returns. The score reports gas units
//! and does not subtract them from the basket. The action table is the same
//! quote, one consumed slope segment per row.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::Instant;

use action::Action;
use num_bigint::BigInt;
use num_traits::Zero;
use searcher::{Conduits, RouteSet, Token, NATIVE};
use simulate::{Price, Ratio};

use crate::reader::{self, NameCost, Reader, VenueMemory};
use crate::source::Source;
use crate::stage::Stage;
use crate::token;

mod gas;
mod price;
mod print;

use gas::{book_gas, GasMeta};
use price::spots;
use print::{action_table, allocation_lines};

pub(crate) fn run<S: Source>(
    source: &S,
    stage: &mut Stage,
    limit: Option<usize>,
) -> eyre::Result<()> {
    let reader = Reader {
        source,
        cache: RefCell::new(std::mem::take(&mut stage.cache)),
        graph: &stage.survey.graph,
        incoming: &stage.incoming,
    };
    let records = source.tokens()?;
    let decimals = wrapper_decimals(&records, stage.wrapped);
    let places = token_decimals(&records);
    eprintln!(
        "pricing {} scenario tokens from {}",
        token_count(stage),
        stage.scenarios.len()
    );
    let prices = spots(&reader, stage);
    let scenarios = match limit {
        Some(limit) => &stage.scenarios[..limit.min(stage.scenarios.len())],
        None => stage.scenarios.as_slice(),
    };
    eprintln!(
        "allocating {} of {} trades",
        scenarios.len(),
        stage.scenarios.len()
    );
    let mut stdout = io::stdout().lock();
    let mut stats = Stats::default();
    for scenario in scenarios {
        let started = Instant::now();
        let ctx = MeasureCtx {
            prices: &prices,
            decimals,
            places: &places,
        };
        let block = one(source, &reader, stage, scenario, &ctx, &mut stats);
        writeln!(stdout, "{block}")?;
        stdout.flush()?;
        eprintln!(
            "{} {} wall_ms={}",
            scenario.name,
            stats.last.as_deref().unwrap_or("error"),
            started.elapsed().as_millis()
        );
    }
    writeln!(stdout, "{}", stats.finish(decimals))?;
    stdout.flush()?;
    Ok(())
}

struct MeasureCtx<'a> {
    prices: &'a BTreeMap<Token, Price>,
    decimals: u32,
    places: &'a BTreeMap<Token, u8>,
}

fn one<S: Source>(
    source: &S,
    reader: &Reader<'_, S>,
    stage: &Stage,
    scenario: &crate::scenario::Scenario,
    ctx: &MeasureCtx<'_>,
    stats: &mut Stats,
) -> String {
    let prices = ctx.prices;
    let decimals = ctx.decimals;
    let places = ctx.places;
    stats.scenarios += 1;
    let mut lines = vec![format!("== {}", scenario.name), scenario.detail.clone()];
    eprintln!("{} search", scenario.name);
    let search_started = Instant::now();
    let searched = match trade::query(&scenario.trade, &scenario.hubs) {
        Ok(query) => searcher::execute(&query, reader),
        Err(err) => {
            stats.errors += 1;
            stats.search_ms += search_started.elapsed().as_millis();
            stats.last = Some(format!(
                "rejected {err} search_ms={}",
                search_started.elapsed().as_millis()
            ));
            lines.push(format!("rejected {err}"));
            return lines.join("\n") + "\n";
        }
    };
    let search_ms = search_started.elapsed().as_millis();
    stats.search_ms += search_ms;
    let build = observability::info_span!("build", scenario = %scenario.name);
    let _build = build.enter();
    let build_started = Instant::now();
    let gas_started = Instant::now();
    let mut gas_cost = NameCost::default();
    let mut gas_meta = GasMeta::default();
    let venues = VenueMemory::new(source);
    let gas = match book_gas(
        &venues,
        &scenario.trade,
        &searched,
        &mut gas_cost,
        &mut gas_meta,
    ) {
        Ok(gas) => gas,
        Err(err) => {
            stats.errors += 1;
            stats.build_ms += build_started.elapsed().as_millis();
            stats.last = Some(format!(
                "gas {err} search_ms={search_ms} build_ms={}",
                build_started.elapsed().as_millis()
            ));
            lines.push(format!("gas {err}"));
            return lines.join("\n") + "\n";
        }
    };
    let gas_us = gas_started.elapsed().as_micros();
    let problem_started = Instant::now();
    let mut name_cost = NameCost::default();
    let problem = {
        let _span = observability::info_span!("problem").entered();
        match trade::problem(
            &scenario.trade,
            &scenario.hubs,
            &searched,
            &gas,
            |token_in, token_out, key| {
                reader::catalog_swap_cost(&venues, token_in, token_out, key, &mut name_cost)
            },
        ) {
            Ok(problem) => problem,
            Err(err) => {
                stats.errors += 1;
                stats.build_ms += build_started.elapsed().as_millis();
                stats.last = Some(format!(
                    "problem {err} search_ms={search_ms} build_ms={}",
                    build_started.elapsed().as_millis()
                ));
                lines.push(format!("problem {err}"));
                return lines.join("\n") + "\n";
            }
        }
    };
    let problem_us = problem_started.elapsed().as_micros();
    let build_ms = build_started.elapsed().as_millis();
    stats.build_ms += build_ms;
    let chords = chord_count(&problem);
    let gas_scan_us = gas_us.saturating_sub(gas_cost.us);
    let problem_other_us = problem_us.saturating_sub(name_cost.us);
    drop(_build);
    observability::info!(
        target: "survey::measure",
        gas_us,
        gas_scan_us,
        gas_swap_calls = gas_cost.calls,
        gas_swap_us = gas_cost.us,
        gas_venue_calls = gas_cost.venue_calls,
        gas_venue_us = gas_cost.venue_us,
        segments = gas_meta.segments,
        unique_actions = gas_meta.unique,
        segment_kinds = %gas_meta.kinds,
        problem_us,
        name_calls = name_cost.calls,
        name_us = name_cost.us,
        name_venue_calls = name_cost.venue_calls,
        name_venue_us = name_cost.venue_us,
        problem_other_us,
        "build breakdown"
    );
    trace_shape(reader, scenario, &searched);
    eprintln!(
        "{} routes {} edges {} chords {} search_ms={search_ms} build_ms={build_ms} gas_us={gas_us} gas_scan_us={gas_scan_us} gas_swap_calls={} gas_swap_us={} gas_venue_calls={} gas_venue_us={} problem_us={problem_us} name_calls={} name_us={} name_venue_calls={} name_venue_us={} problem_other_us={problem_other_us}",
        scenario.name,
        searched.routes.len(),
        problem.edges.len(),
        chords,
        gas_cost.calls,
        gas_cost.us,
        gas_cost.venue_calls,
        gas_cost.venue_us,
        name_cost.calls,
        name_cost.us,
        name_cost.venue_calls,
        name_cost.venue_us,
    );
    let optimize_started = Instant::now();
    let plan = match optimizer::optimize(&problem) {
        Ok(plan) => plan,
        Err(err) => {
            stats.errors += 1;
            stats.optimize_ms += optimize_started.elapsed().as_millis();
            stats.last = Some(format!(
                "optimize {err} search_ms={search_ms} build_ms={build_ms} optimize_ms={}",
                optimize_started.elapsed().as_millis()
            ));
            lines.push(format!("optimize {err}"));
            return lines.join("\n") + "\n";
        }
    };
    let optimize_ms = optimize_started.elapsed().as_millis();
    stats.optimize_ms += optimize_ms;
    stats.plans += 1;
    if plan.count.is_zero() {
        stats.zero += 1;
    }
    let shared = shared_directions(&plan);
    if shared > 0 {
        stats.shared += 1;
    }
    let spent: Vec<_> = plan
        .spent
        .iter()
        .map(|(token, amount)| simulate::Leg {
            token: *token,
            amount: amount.clone(),
        })
        .collect();
    let received: Vec<_> = plan
        .received
        .iter()
        .map(|(token, amount)| simulate::Leg {
            token: *token,
            amount: amount.clone(),
        })
        .collect();
    let scored = simulate::score(&spent, &received, prices, &plan.gas);
    if scored.retained.is_none() {
        stats.unpriced += 1;
    }
    if let Some(retained) = &scored.retained {
        stats.retained.push(retained.clone());
    }
    let retained_text = match &scored.retained {
        Some(ratio) => simulate::decimal(ratio, 6),
        None => "unpriced".to_string(),
    };
    let value_in = simulate::whole_tokens(&scored.value_in, decimals, 6);
    let value_out = simulate::whole_tokens(&scored.value_out, decimals, 6);
    let gap = simulate::whole_tokens(&scored.value_in.sub(&scored.value_out), decimals, 6);
    stats.last = Some(format!(
        "search_ms={search_ms} build_ms={build_ms} optimize_ms={optimize_ms} routes={} edges={} chords={chords} count={} retained={retained_text} value_in={value_in} value_out={value_out} gap={gap} gas={}",
        searched.routes.len(),
        problem.edges.len(),
        plan.count,
        plan.gas
    ));
    lines.push(format!(
        "search_ms {search_ms}\nbuild_ms {build_ms}\noptimize_ms {optimize_ms}"
    ));
    lines.push(format!(
        "routes {} edges {} chords {chords}",
        searched.routes.len(),
        problem.edges.len()
    ));
    lines.push(format!("count {}", plan.count));
    lines.push(format!("gas {}", plan.gas));
    lines.push(format!("shared {shared}"));
    lines.extend(allocation_lines(&problem, &plan, &stage.known));
    lines.extend(action_table(&problem, &plan, &stage.known, places));
    for (token, amount) in &plan.spent {
        lines.push(format!(
            "spent {} {amount}",
            token::label(*token, &stage.known)
        ));
    }
    for (token, amount) in &plan.received {
        lines.push(format!(
            "received {} {amount}",
            token::label(*token, &stage.known)
        ));
    }
    lines.push(format!("value_in {value_in}"));
    lines.push(format!("value_out {value_out}"));
    lines.push(format!("retained {retained_text}"));
    lines.push(format!("gap {gap}"));
    if !scored.unpriced.is_empty() {
        let names = scored
            .unpriced
            .iter()
            .map(|token| token::label(*token, &stage.known))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!("unpriced {names}"));
    }
    lines.join("\n") + "\n"
}

fn token_decimals(records: &[venue::TokenRecord]) -> BTreeMap<Token, u8> {
    let mut places = BTreeMap::new();
    for row in records {
        let Some(decimals) = row.decimals else {
            continue;
        };
        places.insert(token::token_of(row.address), decimals);
    }
    places.entry(NATIVE).or_insert(18);
    places
}

fn chord_count(problem: &optimizer::Problem) -> usize {
    problem
        .edges
        .iter()
        .map(|edge| match &edge.curve {
            optimizer::Curve::Unlimited => 0,
            optimizer::Curve::Chords(chords) => chords.len(),
        })
        .sum()
}

fn token_count(stage: &Stage) -> usize {
    let mut tokens = BTreeSet::new();
    for scenario in &stage.scenarios {
        for (token, _) in &scenario.trade.inputs {
            tokens.insert(*token);
        }
        for (token, _) in &scenario.trade.outputs {
            tokens.insert(*token);
        }
    }
    tokens.len()
}

fn wrapper_decimals(records: &[venue::TokenRecord], wrapped: Token) -> u32 {
    records
        .iter()
        .find(|row| token::token_of(row.address) == wrapped)
        .and_then(|row| row.decimals)
        .unwrap_or(18)
        .into()
}

fn trace_shape<S: Source>(
    reader: &Reader<'_, S>,
    scenario: &crate::scenario::Scenario,
    set: &RouteSet,
) {
    let _span = observability::info_span!("shape").entered();
    let mut by_len = BTreeMap::<usize, usize>::new();
    let mut wrap_edges = 0_usize;
    let mut direct_unwrap = false;
    for edge in &set.edges {
        if matches!(edge.curve, searcher::Curve::Wrap) {
            wrap_edges += 1;
        }
    }
    for route in &set.routes {
        *by_len.entry(route.edges.len()).or_default() += 1;
        if route.edges.len() == 1
            && matches!(
                set.edges.get(route.edges[0]).map(|edge| &edge.curve),
                Some(searcher::Curve::Wrap)
            )
        {
            direct_unwrap = true;
        }
    }
    let lengths = by_len
        .iter()
        .map(|(len, count)| format!("{len}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    let one = BigInt::from(1);
    let mut beats = 0_usize;
    let mut ties = 0_usize;
    let mut below = 0_usize;
    let mut zeros = 0_usize;
    let mut beat_unit = 0_usize;
    let mut best_out = BigInt::from(0);
    let mut best_route = 0_usize;
    for (index, route) in set.routes.iter().enumerate() {
        let out = quote_route(set, route, &one);
        if out > one {
            beats += 1;
            if unit_head(set, route) {
                beat_unit += 1;
            }
        } else if out == one {
            ties += 1;
        } else if out.is_zero() {
            zeros += 1;
        } else {
            below += 1;
        }
        if out > best_out {
            best_out = out;
            best_route = index;
        }
    }
    let best_heads = set
        .routes
        .get(best_route)
        .map(|route| heads(set, route))
        .unwrap_or_default();
    let best_path = set
        .routes
        .get(best_route)
        .map(|route| route_tokens(set, route))
        .unwrap_or_default();
    let best_edges = set
        .routes
        .get(best_route)
        .map(|route| route.edges.len())
        .unwrap_or(0);
    let best_fit = set
        .routes
        .get(best_route)
        .map(|route| head_fit(set, route, &one))
        .unwrap_or_default();
    let mut unit_stays = 0_usize;
    let mut unit_leaves = 0_usize;
    for route in &set.routes {
        if route.edges.len() < 2 {
            continue;
        }
        if stays_on_opening(set, route, &one) {
            unit_stays += 1;
        } else {
            unit_leaves += 1;
        }
    }
    let mut segments = 0_usize;
    let mut unit_width = 0_usize;
    let mut unit_steep = 0_usize;
    let mut steep = 0_usize;
    let mut opening_unit = 0_usize;
    let mut steepest_width = BigInt::from(1);
    let mut steepest_output = BigInt::from(0);
    for edge in &set.edges {
        let searcher::Curve::Conduit(parts) = &edge.curve else {
            continue;
        };
        if parts
            .first()
            .is_some_and(|segment| segment.width == BigInt::from(1))
        {
            opening_unit += 1;
        }
        for segment in parts {
            if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                continue;
            }
            segments += 1;
            if segment.width == BigInt::from(1) {
                unit_width += 1;
                if segment.output > segment.width {
                    unit_steep += 1;
                }
            }
            if segment.output > segment.width {
                steep += 1;
            }
            if &segment.output * &steepest_width > &steepest_output * &segment.width {
                steepest_width.clone_from(&segment.width);
                steepest_output.clone_from(&segment.output);
            }
        }
    }
    let (stored_segments, stored_first) = stored_direct(reader, scenario);
    observability::info!(
        target: "survey::measure",
        hubs = scenario.hubs.len(),
        routes = set.routes.len(),
        edges = set.edges.len(),
        wrap_edges,
        direct_unwrap,
        route_lengths = %lengths,
        quote_at_1_beats_unwrap = beats,
        quote_at_1_ties = ties,
        quote_at_1_below = below,
        quote_at_1_zero = zeros,
        beating_routes_with_unit_head = beat_unit,
        best_route,
        best_edges,
        best_out = %best_out,
        best_heads = %best_heads,
        best_path = %best_path,
        best_fit = %best_fit,
        multi_edge_unit_stays = unit_stays,
        multi_edge_unit_leaves = unit_leaves,
        segments,
        unit_width,
        unit_steep,
        steep_segments = steep,
        opening_unit_edges = opening_unit,
        steepest_width = %steepest_width,
        steepest_output = %steepest_output,
        stored_direct_segments = stored_segments,
        stored_direct_first = %stored_first,
        "scenario shape"
    );
}

fn quote_route(set: &RouteSet, route: &searcher::Route, input: &BigInt) -> BigInt {
    let mut flow = input.clone();
    for index in &route.edges {
        let Some(edge) = set.edges.get(*index) else {
            return BigInt::from(0);
        };
        flow = match &edge.curve {
            searcher::Curve::Wrap => flow,
            searcher::Curve::Conduit(segments) => routing::quote(segments, &flow),
        };
        if flow <= BigInt::from(0) {
            return BigInt::from(0);
        }
    }
    flow
}

fn unit_head(set: &RouteSet, route: &searcher::Route) -> bool {
    route.edges.iter().any(|index| {
        let Some(edge) = set.edges.get(*index) else {
            return false;
        };
        match &edge.curve {
            searcher::Curve::Wrap => false,
            searcher::Curve::Conduit(segments) => segments
                .first()
                .is_some_and(|segment| segment.width == BigInt::from(1)),
        }
    })
}

fn route_tokens(set: &RouteSet, route: &searcher::Route) -> String {
    let mut tokens = Vec::new();
    for (index, edge_index) in route.edges.iter().enumerate() {
        let Some(edge) = set.edges.get(*edge_index) else {
            continue;
        };
        if index == 0 {
            tokens.push(token_hex(edge.token_in));
        }
        tokens.push(token_hex(edge.token_out));
    }
    tokens.join(" -> ")
}

fn token_hex(token: Token) -> String {
    if token == NATIVE {
        return "ETH".to_string();
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::from("0x");
    for byte in token.0 {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// Input that arrives at each edge when `source` is spent, and whether it
/// still sits inside that edge's first slope segment.
fn head_fit(set: &RouteSet, route: &searcher::Route, source: &BigInt) -> String {
    let mut flow = source.clone();
    let mut parts = Vec::new();
    for index in &route.edges {
        let Some(edge) = set.edges.get(*index) else {
            parts.push("missing".to_string());
            break;
        };
        match &edge.curve {
            searcher::Curve::Wrap => {
                parts.push(format!("unwrap in={flow}"));
            }
            searcher::Curve::Conduit(segments) => {
                let Some(head) = segments.first() else {
                    parts.push("empty".to_string());
                    break;
                };
                let fits = flow <= head.width;
                parts.push(format!("in={flow} width={} fits={fits}", head.width));
                if !fits {
                    break;
                }
                flow = routing::quote(segments, &flow);
            }
        }
    }
    parts.join(" | ")
}

fn stays_on_opening(set: &RouteSet, route: &searcher::Route, source: &BigInt) -> bool {
    let mut flow = source.clone();
    for index in &route.edges {
        let Some(edge) = set.edges.get(*index) else {
            return false;
        };
        match &edge.curve {
            searcher::Curve::Wrap => {}
            searcher::Curve::Conduit(segments) => {
                let Some(head) = segments.first() else {
                    return false;
                };
                if flow > head.width {
                    return false;
                }
                flow = routing::quote(segments, &flow);
            }
        }
    }
    true
}

fn heads(set: &RouteSet, route: &searcher::Route) -> String {
    route
        .edges
        .iter()
        .map(
            |index| match set.edges.get(*index).map(|edge| &edge.curve) {
                Some(searcher::Curve::Wrap) => "unwrap".to_string(),
                Some(searcher::Curve::Conduit(segments)) => segments
                    .first()
                    .map(|segment| format!("w={} o={}", segment.width, segment.output))
                    .unwrap_or_else(|| "empty".to_string()),
                None => "missing".to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join(" | ")
}

fn stored_direct<S: Source>(
    reader: &Reader<'_, S>,
    scenario: &crate::scenario::Scenario,
) -> (usize, String) {
    let Some((token_in, _)) = scenario.trade.inputs.first() else {
        return (0, "no input".to_string());
    };
    let Some((token_out, _)) = scenario.trade.outputs.first() else {
        return (0, "no output".to_string());
    };
    let Some(segments) = reader.conduit(*token_in, *token_out) else {
        return (0, "absent".to_string());
    };
    let first = segments
        .first()
        .map(|segment| format!("w={} o={}", segment.width, segment.output))
        .unwrap_or_else(|| "empty".to_string());
    (segments.len(), first)
}

fn shared_directions(plan: &optimizer::Plan) -> usize {
    let mut pairs: BTreeMap<[u8; 32], BTreeSet<(Token, Token)>> = BTreeMap::new();
    for tender in &plan.tenders {
        let Some(pool) = pool_of(&tender.action) else {
            continue;
        };
        if tender.input.is_zero() {
            continue;
        }
        pairs
            .entry(pool)
            .or_default()
            .insert((tender.token_in, tender.token_out));
    }
    pairs.values().filter(|sides| sides.len() > 1).count()
}

fn pool_of(action: &Action) -> Option<[u8; 32]> {
    match action {
        Action::Swap(swap) => Some(swap.pool),
        Action::MintLiquidity(mint) => Some(mint.pool),
        Action::BurnLiquidity(burn) => Some(burn.pool),
        Action::Wrap(_)
        | Action::Unwrap(_)
        | Action::RateWrap(_)
        | Action::RateUnwrap(_)
        | Action::DepositVault(_)
        | Action::RedeemVault(_)
        | Action::Supply(_)
        | Action::Withdraw(_)
        | Action::Stake(_)
        | Action::Unstake(_)
        | Action::Convert(_)
        | Action::Fill(_)
        | Action::Split(_)
        | Action::Merge(_) => None,
    }
}

#[derive(Default)]
struct Stats {
    scenarios: u64,
    plans: u64,
    zero: u64,
    errors: u64,
    unpriced: u64,
    shared: u64,
    search_ms: u128,
    build_ms: u128,
    optimize_ms: u128,
    retained: Vec<Ratio>,
    last: Option<String>,
}

impl Stats {
    fn finish(&self, decimals: u32) -> String {
        let retained = if self.retained.is_empty() {
            "none".to_string()
        } else {
            let mut ordered = self.retained.clone();
            ordered.sort_by(|left, right| left.cmp_value(right));
            let mid = ordered.len() / 2;
            let median = if ordered.len().is_multiple_of(2) {
                let lower = &ordered[mid - 1];
                let upper = &ordered[mid];
                lower
                    .add(upper)
                    .div(&Ratio::new(BigInt::from(2), BigInt::from(1)))
            } else {
                ordered[mid].clone()
            };
            format!(
                "min {} median {}",
                simulate::decimal(&ordered[0], 6),
                simulate::decimal(&median, 6)
            )
        };
        format!(
            "scenarios {}\nplans {}\nzero {}\nerrors {}\nunpriced {}\nshared {}\nsearch_ms {}\nbuild_ms {}\noptimize_ms {}\nretained {retained}\nnumeraire_decimals {decimals}\n",
            self.scenarios,
            self.plans,
            self.zero,
            self.errors,
            self.unpriced,
            self.shared,
            self.search_ms,
            self.build_ms,
            self.optimize_ms
        )
    }
}
