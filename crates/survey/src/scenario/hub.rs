use searcher::{Token, NATIVE};

use super::{multi, single, Scenario};
use crate::token::{amount, known_named, label, Known};

pub(crate) fn pair_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
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

pub(crate) fn basket_scenarios(known: &[Known], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
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
