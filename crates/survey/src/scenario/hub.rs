use searcher::{Token, NATIVE};

use super::{multi, single, Scenario};
use crate::token::{amount, label, Known};

pub(crate) fn pair_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    let ether = amount(18);
    for from in hubs {
        for to in hubs {
            if from == to {
                continue;
            }
            let name = format!("pair-{}-{}", label(*from, known), label(*to, known));
            let partner = next_hub(hubs, wrapped);
            let sweep = partner
                .filter(|token| *from == wrapped && *to == *token)
                .map(|token| sweep_key(known, wrapped, token));
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
    if let Some(partner) = next_hub(hubs, wrapped) {
        let key = sweep_key(known, wrapped, partner);
        for (suffix, exp) in [("1", 0_u32), ("gwei", 9), ("huge", 27)] {
            let mut scenario = single(
                format!(
                    "pair-{}-{}-{}",
                    label(wrapped, known),
                    label(partner, known),
                    suffix
                ),
                format!("hub pair at 10^{exp} raw units"),
                wrapped,
                partner,
                amount(exp),
                hubs,
                wrapped,
            );
            scenario.sweep = Some(key.clone());
            out.push(scenario);
        }
    }
    out
}

fn next_hub(hubs: &[Token], wrapped: Token) -> Option<Token> {
    hubs.iter()
        .copied()
        .find(|token| *token != wrapped && *token != NATIVE)
}

fn sweep_key(known: &[Known], wrapped: Token, partner: Token) -> String {
    format!("{}-{}", label(wrapped, known), label(partner, known))
}

pub(crate) fn native_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
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
        format!("{}-native", label(wrapped, known)),
        format!("{} in, native ETH out", label(wrapped, known)),
        wrapped,
        NATIVE,
        ether,
        hubs,
        wrapped,
    ));
    out
}

pub(crate) fn basket_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let ranked: Vec<Token> = hubs
        .iter()
        .copied()
        .filter(|token| *token != NATIVE)
        .collect();
    let shapes: &[(&[usize], &[usize])] = &[
        (&[0, 1], &[2, 3]),
        (&[0], &[1, 2]),
        (&[1], &[0, 2]),
        (&[0, 1, 2], &[3, 4]),
        (&[0], &[1, 2, 3, 4]),
    ];
    let ether = amount(18);
    let mut out = Vec::new();
    for (inputs, outputs) in shapes {
        let Some(inputs) = pick(&ranked, inputs) else {
            continue;
        };
        let Some(outputs) = pick(&ranked, outputs) else {
            continue;
        };
        let name = format!(
            "basket-{}-{}",
            joined(&inputs, known, "-"),
            joined(&outputs, known, "-")
        );
        let detail = format!(
            "{} in, {} out",
            joined(&inputs, known, ", "),
            joined(&outputs, known, ", ")
        );
        let amounts = inputs.iter().map(|token| (*token, ether.clone())).collect();
        out.push(multi(&name, &detail, amounts, outputs, hubs, wrapped));
    }
    out
}

fn pick(hubs: &[Token], indexes: &[usize]) -> Option<Vec<Token>> {
    indexes
        .iter()
        .map(|index| hubs.get(*index).copied())
        .collect()
}

fn joined(tokens: &[Token], known: &[Known], sep: &str) -> String {
    tokens
        .iter()
        .map(|token| label(*token, known))
        .collect::<Vec<_>>()
        .join(sep)
}
