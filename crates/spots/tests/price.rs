//! The spot is the smallest sized round trip that clears both ways on one route.

use alloy_primitives::Address;
use num_bigint::BigInt;
use routing::{Action, SlopeSegment};
use spots::{price, rank_hubs, CustodialWrap, Edge, Numeraire, Spot};

fn addr(byte: u8) -> Address {
    let mut raw = [0_u8; 20];
    raw[19] = byte;
    Address::from(raw)
}

fn segment(width: i64, output: i64) -> SlopeSegment {
    SlopeSegment {
        width: BigInt::from(width),
        output: BigInt::from(output),
        action: Action {
            kind: 1,
            pool: [1; 32],
        },
    }
}

fn edge(from: Address, to: Address, width: i64, output: i64, block: u64) -> Edge {
    Edge {
        token_in: from,
        token_out: to,
        block: Some(block),
        segments: vec![segment(width, output)],
    }
}

fn sized(token: Address, sizes: &[i64]) -> Numeraire {
    Numeraire {
        token,
        sizes: sizes.iter().copied().map(BigInt::from).collect(),
    }
}

fn of(spots: &[Spot], token: Address, numeraire: Address) -> Option<&Spot> {
    spots
        .iter()
        .find(|spot| spot.token == token && spot.numeraire == numeraire)
}

#[test]
fn the_smallest_clearing_size_is_the_spot() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![
        edge(numeraire, token, 100_000, 200_000, 50),
        edge(token, numeraire, 200_000, 100_000, 40),
    ];
    let spots = price(&edges, 3, 32, &[sized(numeraire, &[100, 1_000])], None);
    let spot = of(&spots, token, numeraire).expect("direct spot");
    assert_eq!(spot.size, BigInt::from(100));
    assert_eq!(spot.bought, BigInt::from(200));
    assert_eq!(spot.sold, BigInt::from(100));
    assert_eq!(spot.hops, 1);
    assert_eq!(spot.block, 40);
}

#[test]
fn one_hub_is_the_route_when_the_direct_pair_is_missing() {
    let numeraire = addr(1);
    let hub = addr(2);
    let token = addr(3);
    let edges = vec![
        edge(numeraire, hub, 10_000, 10_000, 10),
        edge(hub, numeraire, 10_000, 10_000, 11),
        edge(hub, token, 10_000, 20_000, 12),
        edge(token, hub, 20_000, 10_000, 13),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("one hub");
    assert_eq!(spot.hops, 2);
    assert_eq!(spot.bought, BigInt::from(200));
    assert_eq!(spot.sold, BigInt::from(100));
    assert_eq!(spot.block, 10);
}

#[test]
fn a_path_through_a_token_that_is_not_a_hub_is_not_a_spot() {
    let numeraire = addr(1);
    let through = addr(2);
    let token = addr(3);
    let hub = addr(4);
    let mut edges = vec![
        edge(numeraire, through, 10_000, 10_000, 1),
        edge(through, numeraire, 10_000, 10_000, 1),
        edge(through, token, 10_000, 10_000, 1),
        edge(token, through, 10_000, 10_000, 1),
    ];
    for byte in 10..15 {
        edges.push(edge(hub, addr(byte), 10_000, 10_000, 1));
        edges.push(edge(addr(byte), hub, 10_000, 10_000, 1));
    }
    let spots = price(&edges, 5, 32, &[sized(numeraire, &[100])], None);
    assert!(of(&spots, token, numeraire).is_none());
    assert!(rank_hubs(&edges, 5, 32).contains(&hub));
    assert!(!rank_hubs(&edges, 5, 32).contains(&through));
}

#[test]
fn one_missing_direction_stores_nothing() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![edge(numeraire, token, 10_000, 10_000, 1)];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    assert!(of(&spots, token, numeraire).is_none());
}

#[test]
fn a_round_trip_under_half_stores_nothing() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![
        edge(numeraire, token, 10_000, 10_000, 1),
        edge(token, numeraire, 10_000, 4_000, 1),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100, 1_000])], None);
    assert!(spots.is_empty());
}

#[test]
fn a_round_trip_of_half_the_size_is_kept() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![
        edge(numeraire, token, 10_000, 10_000, 1),
        edge(token, numeraire, 10_000, 5_000, 1),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("half");
    assert_eq!(spot.sold, BigInt::from(50));
}

#[test]
fn a_size_that_pays_nothing_is_skipped_for_the_next_size() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![
        edge(numeraire, token, 10_000, 10_000, 7),
        edge(token, numeraire, 1_000, 900, 8),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[1, 10])], None);
    let spot = of(&spots, token, numeraire).expect("next size");
    assert_eq!(spot.size, BigInt::from(10));
    assert_eq!(spot.bought, BigInt::from(10));
    assert_eq!(spot.sold, BigInt::from(9));
}

#[test]
fn a_conduit_narrower_than_every_size_stores_nothing() {
    let numeraire = addr(1);
    let token = addr(2);
    let edges = vec![
        edge(numeraire, token, 50, 50, 1),
        edge(token, numeraire, 50, 50, 1),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100, 1_000])], None);
    assert!(spots.is_empty());
}

#[test]
fn two_hubs_are_three_conduits() {
    let numeraire = addr(1);
    let left = addr(2);
    let right = addr(3);
    let token = addr(4);
    let edges = vec![
        edge(numeraire, left, 10_000, 10_000, 10),
        edge(left, right, 10_000, 10_000, 40),
        edge(right, token, 10_000, 10_000, 25),
        edge(token, right, 10_000, 10_000, 12),
        edge(right, left, 10_000, 10_000, 8),
        edge(left, numeraire, 10_000, 10_000, 30),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("two hubs");
    assert_eq!(spot.hops, 3);
    assert_eq!(spot.bought, BigInt::from(100));
    assert_eq!(spot.sold, BigInt::from(100));
    assert_eq!(spot.block, 8);
}

#[test]
fn the_route_that_buys_more_tokens_wins() {
    let numeraire = addr(1);
    let hub = addr(2);
    let token = addr(3);
    let edges = vec![
        edge(numeraire, token, 10_000, 10_000, 1),
        edge(token, numeraire, 10_000, 10_000, 1),
        edge(numeraire, hub, 10_000, 10_000, 2),
        edge(hub, numeraire, 10_000, 10_000, 2),
        edge(hub, token, 10_000, 20_000, 3),
        edge(token, hub, 20_000, 10_000, 3),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("deeper buy");
    assert_eq!(spot.bought, BigInt::from(200));
    assert_eq!(spot.hops, 2);
}

#[test]
fn equal_buys_keep_fewer_conduits() {
    let numeraire = addr(1);
    let hub = addr(2);
    let token = addr(3);
    let edges = vec![
        edge(numeraire, token, 10_000, 10_000, 4),
        edge(token, numeraire, 10_000, 10_000, 5),
        edge(numeraire, hub, 10_000, 10_000, 6),
        edge(hub, numeraire, 10_000, 10_000, 7),
        edge(hub, token, 10_000, 10_000, 8),
        edge(token, hub, 10_000, 10_000, 9),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("direct");
    assert_eq!(spot.hops, 1);
    assert_eq!(spot.bought, BigInt::from(100));
    assert_eq!(spot.block, 4);
}

#[test]
fn the_buy_and_the_sell_stay_on_one_route() {
    let numeraire = addr(1);
    let hub = addr(2);
    let token = addr(3);
    let edges = vec![
        edge(numeraire, token, 10_000, 30_000, 1),
        edge(token, numeraire, 30_000, 3_000, 1),
        edge(numeraire, hub, 10_000, 10_000, 2),
        edge(hub, numeraire, 10_000, 10_000, 2),
        edge(hub, token, 10_000, 20_000, 3),
        edge(token, hub, 20_000, 10_000, 3),
    ];
    let spots = price(&edges, 1, 32, &[sized(numeraire, &[100])], None);
    let spot = of(&spots, token, numeraire).expect("hub route");
    assert_eq!(spot.bought, BigInt::from(200));
    assert_eq!(spot.sold, BigInt::from(100));
    assert_eq!(spot.hops, 2);
}

#[test]
fn a_numeraire_is_a_hub_below_the_floor() {
    let wrapped = addr(1);
    let usdc = addr(2);
    let token = addr(3);
    let edges = vec![
        edge(token, wrapped, 10_000, 10_000, 15),
        edge(wrapped, token, 10_000, 10_000, 16),
        edge(wrapped, usdc, 10_000, 10_000, 17),
        edge(usdc, wrapped, 10_000, 10_000, 18),
    ];
    let spots = price(
        &edges,
        100,
        32,
        &[sized(wrapped, &[100]), sized(usdc, &[100])],
        None,
    );
    let spot = of(&spots, token, usdc).expect("through the other numeraire");
    assert_eq!(spot.hops, 2);
    assert_eq!(spot.bought, BigInt::from(100));
    assert_eq!(spot.sold, BigInt::from(100));
    assert_eq!(spot.block, 15);
    assert!(of(&spots, wrapped, wrapped).is_none());
    assert!(of(&spots, usdc, usdc).is_none());
}

#[test]
fn native_eth_takes_the_custodial_wrap() {
    let native = addr(0);
    let wrapped = addr(1);
    let usdc = addr(2);
    let edges = vec![
        edge(wrapped, usdc, 10_000, 10_000, 15),
        edge(usdc, wrapped, 10_000, 10_000, 21),
    ];
    let spots = price(
        &edges,
        1,
        32,
        &[sized(wrapped, &[100]), sized(usdc, &[100])],
        Some(CustodialWrap { native, wrapped }),
    );
    let in_wrapped = of(&spots, native, wrapped).expect("wrap");
    assert_eq!(in_wrapped.size, BigInt::from(100));
    assert_eq!(in_wrapped.bought, BigInt::from(100));
    assert_eq!(in_wrapped.sold, BigInt::from(100));
    assert_eq!(in_wrapped.hops, 1);
    assert_eq!(in_wrapped.block, 0);
    let in_usdc = of(&spots, native, usdc).expect("wrap then conduit");
    assert_eq!(in_usdc.hops, 2);
    assert_eq!(in_usdc.bought, BigInt::from(100));
    assert_eq!(in_usdc.sold, BigInt::from(100));
    assert_eq!(in_usdc.block, 15);
}

#[test]
fn hubs_keep_the_lower_token_when_the_counts_match() {
    let low = addr(1);
    let high = addr(2);
    let edges = vec![
        edge(low, addr(3), 10, 10, 1),
        edge(high, addr(4), 10, 10, 1),
    ];
    assert_eq!(rank_hubs(&edges, 1, 1), vec![low]);
}

#[test]
fn the_mainnet_sizes_are_a_hundredth_a_tenth_and_one_of_each_numeraire() {
    let numeraires = spots::mainnet_numeraires();
    assert_eq!(numeraires[0].token, spots::WETH);
    assert_eq!(
        numeraires[0].sizes,
        vec![
            BigInt::from(10u64.pow(16)),
            BigInt::from(10u64.pow(17)),
            BigInt::from(10u64.pow(18))
        ]
    );
    assert_eq!(numeraires[1].token, spots::USDC);
    assert_eq!(
        numeraires[1].sizes,
        vec![
            BigInt::from(10u64.pow(8)),
            BigInt::from(10u64.pow(9)),
            BigInt::from(10u64.pow(10))
        ]
    );
}
