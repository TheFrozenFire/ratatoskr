//! Allocate each prepared trade and score the plan.
//!
//! Prices are the first slope of a conduit into the wrapped token. They are
//! applied after [`optimizer::optimize`] returns. Gas units are charged on the
//! problem so equal basket counts keep the cheaper plan. The score reports
//! those units and does not subtract them from the basket.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::Instant;

use action::Action;
use num_bigint::BigInt;
use num_traits::Zero;
use searcher::{Conduits, RouteSet, Token, NATIVE};
use simulate::{Price, Ratio};

use crate::reader::{self, Reader};
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
    eprintln!("pricing {} scenario tokens", token_count(stage));
    let prices = spots(&reader, stage);
    let scenarios = match limit {
        Some(limit) => &stage.scenarios[..limit.min(stage.scenarios.len())],
        None => stage.scenarios.as_slice(),
    };
    eprintln!("allocating {} trades", scenarios.len());
    let mut stdout = io::stdout().lock();
    let mut stats = Stats::default();
    for scenario in scenarios {
        let started = Instant::now();
        let block = one(
            source, &reader, stage, scenario, &prices, decimals, &mut stats,
        );
        writeln!(stdout, "{block}")?;
        stdout.flush()?;
        eprintln!(
            "{} {} ms={}",
            scenario.name,
            stats.last.as_deref().unwrap_or("error"),
            started.elapsed().as_millis()
        );
    }
    writeln!(stdout, "{}", stats.finish(decimals))?;
    stdout.flush()?;
    Ok(())
}

fn one<S: Source>(
    source: &S,
    reader: &Reader<'_, S>,
    stage: &Stage,
    scenario: &crate::scenario::Scenario,
    prices: &BTreeMap<Token, Price>,
    decimals: u32,
    stats: &mut Stats,
) -> String {
    stats.scenarios += 1;
    let mut lines = vec![format!("== {}", scenario.name), scenario.detail.clone()];
    eprintln!("{} search", scenario.name);
    let searched = match trade::query(&scenario.trade, &scenario.hubs) {
        Ok(query) => searcher::execute(&query, reader),
        Err(err) => {
            stats.errors += 1;
            stats.last = Some(format!("rejected {err}"));
            lines.push(format!("rejected {err}"));
            return lines.join("\n") + "\n";
        }
    };
    let gas = match book_gas(source, &scenario.trade, &searched) {
        Ok(gas) => gas,
        Err(err) => {
            stats.errors += 1;
            stats.last = Some(format!("gas {err}"));
            lines.push(format!("gas {err}"));
            return lines.join("\n") + "\n";
        }
    };
    let problem = match trade::problem(
        &scenario.trade,
        &scenario.hubs,
        &searched,
        &gas,
        |token_in, token_out, key| reader::catalog_swap(source, token_in, token_out, key),
    ) {
        Ok(problem) => problem,
        Err(err) => {
            stats.errors += 1;
            stats.last = Some(format!("problem {err}"));
            lines.push(format!("problem {err}"));
            return lines.join("\n") + "\n";
        }
    };
    eprintln!(
        "{} routes {} edges {}",
        scenario.name,
        searched.routes.len(),
        problem.edges.len()
    );
    let plan = match optimizer::optimize(&problem) {
        Ok(plan) => plan,
        Err(err) => {
            stats.errors += 1;
            stats.last = Some(format!("optimize {err}"));
            lines.push(format!("optimize {err}"));
            return lines.join("\n") + "\n";
        }
    };
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
    stats.last = Some(format!(
        "count {} retained {retained_text} gas {}",
        plan.count, plan.gas
    ));
    lines.push(format!(
        "routes {} edges {}",
        searched.routes.len(),
        problem.edges.len()
    ));
    lines.push(format!("count {}", plan.count));
    lines.push(format!("gas {}", plan.gas));
    lines.push(format!("shared {shared}"));
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
    lines.push(format!(
        "value_in {}",
        simulate::whole_tokens(&scored.value_in, decimals, 6)
    ));
    lines.push(format!(
        "value_out {}",
        simulate::whole_tokens(&scored.value_out, decimals, 6)
    ));
    lines.push(format!("retained {retained_text}"));
    lines.push(format!(
        "gap {}",
        simulate::whole_tokens(&scored.value_in.sub(&scored.value_out), decimals, 6)
    ));
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

fn book_gas<S: Source>(
    source: &S,
    trade: &trade::Trade,
    set: &RouteSet,
) -> Result<BTreeMap<Action, BigInt>, trade::TradeError> {
    let mut gas = BTreeMap::new();
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
                    if !seen.insert(segment.action) {
                        continue;
                    }
                    let action = reader::catalog_swap(
                        source,
                        edge.token_in,
                        edge.token_out,
                        &segment.action,
                    )?;
                    let units = gas_units(&action);
                    gas.insert(action, units);
                }
            }
        }
    }
    Ok(gas)
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
            "scenarios {}\nplans {}\nzero {}\nerrors {}\nunpriced {}\nshared {}\nretained {retained}\nnumeraire_decimals {decimals}\n",
            self.scenarios, self.plans, self.zero, self.errors, self.unpriced, self.shared
        )
    }
}
