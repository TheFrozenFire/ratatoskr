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
    let build = tracing::info_span!("build", scenario = %scenario.name);
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
        let _span = tracing::info_span!("problem").entered();
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
    tracing::info!(
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

fn allocation_lines(
    problem: &optimizer::Problem,
    plan: &optimizer::Plan,
    known: &[crate::token::Known],
) -> Vec<String> {
    let mut unwrap_in = BigInt::from(0);
    let mut swap_source = BigInt::from(0);
    let mut others = Vec::new();
    for tender in &plan.tenders {
        if matches!(tender.action, Action::Wrap(_) | Action::Unwrap(_)) {
            unwrap_in += &tender.input;
        } else {
            others.push(tender);
        }
    }
    for flow in &plan.flows {
        let lone_custodial = problem.routes.get(flow.route).is_some_and(|route| {
            route.edges.len() == 1
                && problem
                    .edges
                    .get(route.edges[0])
                    .is_some_and(|edge| matches!(edge.action, Action::Wrap(_) | Action::Unwrap(_)))
        });
        if !lone_custodial {
            swap_source += &flow.input;
        }
    }
    others.sort_by(|left, right| right.input.cmp(&left.input));
    let mut lines = vec![
        format!("unwrap_in {unwrap_in}"),
        format!("swap_source {swap_source}"),
        format!("tenders {}", plan.tenders.len()),
    ];
    for tender in others.iter().take(8) {
        lines.push(format!(
            "tender {} {} {} {}",
            action_kind(&tender.action),
            token::label(tender.token_in, known),
            token::label(tender.token_out, known),
            tender.input
        ));
    }
    let mut by_len = BTreeMap::<usize, (usize, BigInt, BigInt)>::new();
    for flow in &plan.flows {
        let len = problem
            .routes
            .get(flow.route)
            .map(|route| route.edges.len())
            .unwrap_or(0);
        let entry = by_len
            .entry(len)
            .or_insert_with(|| (0, BigInt::from(0), BigInt::from(0)));
        entry.0 += 1;
        entry.1 += &flow.input;
        entry.2 += &flow.output;
    }
    let flows = by_len
        .iter()
        .map(|(len, (count, input, output))| format!("{len}:{count}:{input}:{output}"))
        .collect::<Vec<_>>()
        .join(",");
    lines.push(format!("flows {flows}"));
    lines
}

fn token_decimals(records: &[venues::TokenRecord]) -> BTreeMap<Token, u8> {
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

/// One consumed slope segment. `output` is that segment's integer share of the edge quote.
struct ActionRow {
    kind: &'static str,
    contract: String,
    pool: String,
    token_in: Token,
    token_out: Token,
    input: BigInt,
    output: BigInt,
    width: BigInt,
    chord_output: BigInt,
}

fn action_table(
    problem: &optimizer::Problem,
    plan: &optimizer::Plan,
    known: &[crate::token::Known],
    places: &BTreeMap<Token, u8>,
) -> Vec<String> {
    let mut rows = Vec::new();
    for edge in &problem.edges {
        let (Some(token_in), Some(token_out)) = (edge.token_in(), edge.token_out()) else {
            continue;
        };
        let input = edge_input(edge, &plan.tenders, token_in, token_out);
        if input <= BigInt::from(0) {
            continue;
        }
        rows.extend(edge_rows(edge, token_in, token_out, &input));
    }
    rows.sort_by(|left, right| right.output.cmp(&left.output));
    let mut lines = vec![format!("action_rows {}", rows.len()), action_header()];
    for row in rows {
        lines.push(action_line(&row, known, places));
    }
    lines
}

fn action_header() -> String {
    [
        "action",
        "kind",
        "contract",
        "pool",
        "token_in",
        "token_out",
        "dec_in",
        "dec_out",
        "input",
        "output",
        "width",
        "chord_output",
        "in_units",
        "out_units",
        "out_per_in",
    ]
    .join("\t")
}

fn action_line(
    row: &ActionRow,
    known: &[crate::token::Known],
    places: &BTreeMap<Token, u8>,
) -> String {
    let dec_in = places.get(&row.token_in).copied();
    let dec_out = places.get(&row.token_out).copied();
    [
        "action".to_string(),
        row.kind.to_string(),
        row.contract.clone(),
        row.pool.clone(),
        token::label(row.token_in, known),
        token::label(row.token_out, known),
        dec_in
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into()),
        dec_out
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into()),
        row.input.to_string(),
        row.output.to_string(),
        row.width.to_string(),
        row.chord_output.to_string(),
        units(&row.input, dec_in),
        units(&row.output, dec_out),
        out_per_in(&row.output, &row.input, dec_out, dec_in),
    ]
    .join("\t")
}

fn edge_input(
    edge: &optimizer::Edge,
    tenders: &[optimizer::Tender],
    token_in: Token,
    token_out: Token,
) -> BigInt {
    let mut owned = BTreeSet::new();
    match &edge.curve {
        optimizer::Curve::Unlimited => {
            owned.insert(edge.action.clone());
        }
        optimizer::Curve::Chords(chords) => {
            for chord in chords {
                owned.insert(chord.action.clone());
            }
        }
    }
    let mut input = BigInt::from(0);
    for tender in tenders {
        if tender.token_in == token_in
            && tender.token_out == token_out
            && owned.contains(&tender.action)
        {
            input += &tender.input;
        }
    }
    input
}

fn edge_rows(
    edge: &optimizer::Edge,
    token_in: Token,
    token_out: Token,
    input: &BigInt,
) -> Vec<ActionRow> {
    match &edge.curve {
        optimizer::Curve::Unlimited => vec![ActionRow {
            kind: action_kind(&edge.action),
            contract: locate(&edge.action).0,
            pool: locate(&edge.action).1,
            token_in,
            token_out,
            input: input.clone(),
            output: input.clone(),
            width: BigInt::from(0),
            chord_output: BigInt::from(0),
        }],
        optimizer::Curve::Chords(chords) => {
            let mut left = input.clone();
            let mut rows = Vec::new();
            for chord in chords {
                if left.is_zero() || chord.width.is_zero() {
                    break;
                }
                let taken = if left < chord.width {
                    left.clone()
                } else {
                    chord.width.clone()
                };
                let paid = (&chord.output * &taken) / &chord.width;
                let (contract, pool) = locate(&chord.action);
                rows.push(ActionRow {
                    kind: action_kind(&chord.action),
                    contract,
                    pool,
                    token_in,
                    token_out,
                    input: taken.clone(),
                    output: paid,
                    width: chord.width.clone(),
                    chord_output: chord.output.clone(),
                });
                left -= taken;
            }
            rows
        }
    }
}

fn locate(action: &Action) -> (String, String) {
    match action {
        Action::Swap(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        Action::MintLiquidity(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        Action::BurnLiquidity(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        _ => ("-".into(), "-".into()),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn units(amount: &BigInt, decimals: Option<u8>) -> String {
    let Some(decimals) = decimals else {
        return "-".into();
    };
    let scale = ten(decimals);
    let whole = amount / &scale;
    let frac = (amount % &scale).to_string();
    if decimals == 0 {
        return whole.to_string();
    }
    format!("{whole}.{:0>width$}", frac, width = decimals as usize)
}

/// Output token units paid for one input token unit.
fn out_per_in(output: &BigInt, input: &BigInt, dec_out: Option<u8>, dec_in: Option<u8>) -> String {
    let (Some(dec_out), Some(dec_in)) = (dec_out, dec_in) else {
        return "-".into();
    };
    if input.is_zero() {
        return "-".into();
    }
    let numer = output * ten(dec_in);
    let denom = input * ten(dec_out);
    let places = 8_u8;
    let scale = ten(places);
    let scaled = &numer * &scale / &denom;
    let whole = &scaled / &scale;
    let frac = (&scaled % &scale).to_string();
    format!("{whole}.{:0>width$}", frac, width = places as usize)
}

fn ten(decimals: u8) -> BigInt {
    let mut value = BigInt::from(1);
    for _ in 0..decimals {
        value *= 10;
    }
    value
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Wrap(_) => "wrap",
        Action::Unwrap(_) => "unwrap",
        Action::RateWrap(_) => "rate-wrap",
        Action::RateUnwrap(_) => "rate-unwrap",
        Action::Swap(_) => "swap",
        Action::MintLiquidity(_) => "mint",
        Action::BurnLiquidity(_) => "burn",
        Action::DepositVault(_) => "deposit",
        Action::RedeemVault(_) => "redeem",
        Action::Supply(_) => "supply",
        Action::Withdraw(_) => "withdraw",
        Action::Stake(_) => "stake",
        Action::Unstake(_) => "unstake",
        Action::Convert(_) => "convert",
        Action::Fill(_) => "fill",
        Action::Split(_) => "split",
        Action::Merge(_) => "merge",
    }
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

fn spots<S: Source>(reader: &Reader<'_, S>, stage: &Stage) -> BTreeMap<Token, Price> {
    let mut tokens = BTreeSet::new();
    for scenario in &stage.scenarios {
        for (token, _) in &scenario.trade.inputs {
            tokens.insert(*token);
        }
        for (token, _) in &scenario.trade.outputs {
            tokens.insert(*token);
        }
    }
    let mut prices = BTreeMap::new();
    for token in tokens {
        if let Some(price) = spot(reader, token, stage.wrapped, &stage.hubs) {
            prices.insert(token, price);
        }
    }
    prices
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

fn spot<S: Source>(
    reader: &Reader<'_, S>,
    token: Token,
    numeraire: Token,
    hubs: &[Token],
) -> Option<Price> {
    if token == numeraire || token == NATIVE {
        return Some(Price::one());
    }
    if let Some(price) = marginal(reader, token, numeraire) {
        return Some(price);
    }
    let mut best = None;
    for hub in hubs {
        if *hub == token || *hub == numeraire {
            continue;
        }
        let Some(to_hub) = marginal(reader, token, *hub) else {
            continue;
        };
        let to_num = if *hub == NATIVE {
            Price::one()
        } else {
            let Some(price) = marginal(reader, *hub, numeraire) else {
                continue;
            };
            price
        };
        let price = to_hub.mul(&to_num);
        if best.as_ref().map(|held| price.beats(held)).unwrap_or(true) {
            best = Some(price);
        }
    }
    best
}

fn marginal<S: Source>(reader: &Reader<'_, S>, from: Token, to: Token) -> Option<Price> {
    let segments = reader.conduit(from, to)?;
    let first = segments.first()?;
    if first.width <= BigInt::from(0) || first.output <= BigInt::from(0) {
        return None;
    }
    Some(Price {
        numer: first.output.clone(),
        denom: first.width.clone(),
    })
}

fn wrapper_decimals(records: &[venues::TokenRecord], wrapped: Token) -> u32 {
    records
        .iter()
        .find(|row| token::token_of(row.address) == wrapped)
        .and_then(|row| row.decimals)
        .unwrap_or(18)
        .into()
}

#[derive(Default)]
struct GasMeta {
    segments: u64,
    unique: u64,
    kinds: String,
}

fn book_gas<S: Source>(
    source: &S,
    trade: &trade::Trade,
    set: &RouteSet,
    cost: &mut NameCost,
    meta: &mut GasMeta,
) -> Result<BTreeMap<Action, BigInt>, trade::TradeError> {
    let _span = tracing::info_span!("book_gas").entered();
    let mut gas = BTreeMap::new();
    let mut kinds = BTreeMap::<u8, u64>::new();
    for edge in &set.edges {
        match &edge.curve {
            searcher::Curve::Wrap => {
                if let Some(action) = wrap_action(trade, edge.token_in, edge.token_out) {
                    let units = gas_units(&action);
                    gas.insert(action, units);
                }
            }
            searcher::Curve::Conduit(segments) => {
                let mut seen = BTreeSet::new();
                for segment in segments {
                    if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                        continue;
                    }
                    meta.segments += 1;
                    *kinds.entry(segment.action.kind).or_default() += 1;
                    if !seen.insert(segment.action) {
                        continue;
                    }
                    meta.unique += 1;
                    let action = reader::catalog_swap_cost(
                        source,
                        edge.token_in,
                        edge.token_out,
                        &segment.action,
                        cost,
                    )?;
                    let units = gas_units(&action);
                    gas.insert(action, units);
                }
            }
        }
    }
    meta.kinds = kinds
        .iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(gas)
}

fn trace_shape<S: Source>(
    reader: &Reader<'_, S>,
    scenario: &crate::scenario::Scenario,
    set: &RouteSet,
) {
    let _span = tracing::info_span!("shape").entered();
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
    tracing::info!(
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

fn wrap_action(trade: &trade::Trade, token_in: Token, token_out: Token) -> Option<Action> {
    let wrapped = trade.wrapped?;
    if wrapped == NATIVE {
        return None;
    }
    if token_in == NATIVE && token_out == wrapped {
        action::Wrap::new(wrapped, NATIVE).ok().map(Action::Wrap)
    } else if token_out == NATIVE && token_in == wrapped {
        action::Unwrap::new(wrapped, NATIVE)
            .ok()
            .map(Action::Unwrap)
    } else {
        None
    }
}

fn gas_units(action: &Action) -> BigInt {
    let units: i64 = match action {
        Action::Wrap(_) => 45_000,
        Action::Unwrap(_) => 35_000,
        Action::RateWrap(_) | Action::RateUnwrap(_) => 60_000,
        Action::Swap(swap) => match swap.invariant {
            action::Invariant::Product => 110_000,
            action::Invariant::Concentrated => 180_000,
            action::Invariant::Stable => 250_000,
            action::Invariant::Weighted => 200_000,
            action::Invariant::Crypto => 280_000,
            action::Invariant::Bin => 160_000,
            action::Invariant::Pmm => 150_000,
        },
        Action::MintLiquidity(_) | Action::BurnLiquidity(_) => 200_000,
        Action::DepositVault(_) | Action::RedeemVault(_) => 80_000,
        Action::Supply(_) | Action::Withdraw(_) => 120_000,
        Action::Stake(_) | Action::Unstake(_) => 120_000,
        Action::Convert(_) => 80_000,
        Action::Fill(_) => 150_000,
        Action::Split(_) | Action::Merge(_) => 100_000,
    };
    BigInt::from(units)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use action::{Action, Address, Invariant, Swap, Token};
    use num_bigint::BigInt;
    use optimizer::{Chord, Curve, Edge, Input, Output, Plan, Problem, Route, Tender};

    use super::action_table;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn swap(pool: u8, token_in: Token, token_out: Token) -> Action {
        let contract = Address::new([pool; 20]).expect("contract");
        Action::Swap(
            Swap::new(
                contract,
                contract.pool_id(),
                token_in,
                token_out,
                Invariant::Product,
            )
            .expect("swap"),
        )
    }

    #[test]
    fn each_consumed_segment_keeps_its_share_of_the_quote() {
        let token_in = token(1);
        let token_out = token(2);
        let steep = swap(1, token_in, token_out);
        let flat = swap(2, token_in, token_out);
        let problem = Problem {
            inputs: vec![Input {
                token: token_in,
                amount: BigInt::from(6),
            }],
            outputs: vec![Output {
                token: token_out,
                weight: BigInt::from(1),
            }],
            edges: vec![Edge {
                action: steep.clone(),
                curve: Curve::Chords(vec![
                    Chord {
                        width: BigInt::from(4),
                        output: BigInt::from(10),
                        action: steep.clone(),
                    },
                    Chord {
                        width: BigInt::from(4),
                        output: BigInt::from(2),
                        action: flat.clone(),
                    },
                ]),
            }],
            routes: vec![Route { edges: vec![0] }],
            hubs: Vec::new(),
            gas: BTreeMap::new(),
            gas_ceiling: None,
        };
        let plan = Plan {
            count: BigInt::from(11),
            flows: Vec::new(),
            spent: Vec::new(),
            received: Vec::new(),
            tenders: vec![
                Tender {
                    action: steep,
                    token_in,
                    token_out,
                    input: BigInt::from(4),
                },
                Tender {
                    action: flat,
                    token_in,
                    token_out,
                    input: BigInt::from(2),
                },
            ],
            gas: BigInt::from(0),
        };
        let mut places = BTreeMap::new();
        places.insert(token_in, 0);
        places.insert(token_out, 0);
        let lines = action_table(&problem, &plan, &[], &places);
        assert_eq!(lines[0], "action_rows 2");
        assert!(lines[1].starts_with("action\tkind\t"));
        assert!(lines[2].contains("\t4\t10\t4\t10\t4\t10\t2.50000000"));
        assert!(lines[3].contains("\t2\t1\t4\t2\t2\t1\t0.50000000"));
    }
}
