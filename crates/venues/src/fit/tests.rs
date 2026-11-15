use super::domain::*;
use crate::catalog::UNI_V3;
use crate::state::CURVE_VERSION;
use alloy_primitives::{address, Address, B256, U256};
use chain::detail::{ConstantProduct, PoolDetail};
use store_model::UNI_V2;

#[test]
fn cp_99_buys_ninety_nine_percent_of_the_output() {
    let pool = ConstantProduct {
        kind: UNI_V2,
        id: B256::ZERO,
        pair: Address::ZERO,
        token0: address!("0000000000000000000000000000000000000001"),
        token1: address!("0000000000000000000000000000000000000002"),
        reserve0: U256::from(1_000u64),
        reserve1: U256::from(1_000u64),
        fee_pips: Some(0),
        timestamp: 0,
        flags: 0,
    };
    let (max_in, source) = cp_max(&pool, pool.token0, pool.token1);
    assert_eq!(source, "cp_99");
    assert_eq!(max_in, U256::from(99_000u64));
    let quote = PoolDetail::ConstantProduct(pool.clone())
        .quote(pool.token0, pool.token1, max_in)
        .unwrap();
    assert_eq!(quote.amount_out, U256::from(990u64));
}

#[test]
fn a_local_product_curve_quotes_off_the_grid() {
    let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let detail = PoolDetail::ConstantProduct(ConstantProduct {
        kind: UNI_V2,
        id: B256::ZERO,
        pair: address!("1111111111111111111111111111111111111111"),
        token0,
        token1,
        reserve0: U256::from(1_000_000u64),
        reserve1: U256::from(2_000_000u64),
        fee_pips: Some(3000),
        timestamp: 1,
        flags: 0,
    });
    let curve = crate::state::curve_for_snapshot(&detail, 7).unwrap();
    assert_eq!(curve.version, CURVE_VERSION);
    assert_eq!(curve.generation, 7);
    assert_eq!(curve.payload[0], 1);
    assert_eq!(curve.payload[1], 0);
    let off = U256::from(2_500u64);
    for (token_in, token_out) in [(token0, token1), (token1, token0)] {
        let fitted = crate::state::estimate(&curve, 7, token_in, token_out, off).unwrap();
        let direct = detail.quote(token_in, token_out, off).unwrap();
        assert_eq!(fitted.amount_out, direct.amount_out);
        assert_eq!(fitted.fee_paid, direct.fee_paid);
        assert!(!fitted.exhausted);
    }
    let zero = crate::state::estimate(&curve, 7, token0, token1, U256::ZERO).unwrap();
    assert_eq!(zero.amount_out, U256::ZERO);
    assert_eq!(zero.fee_paid, U256::ZERO);
}

#[test]
fn a_local_concentrated_curve_quotes_off_the_grid() {
    let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let liquidity = 1_000_000_i128;
    let detail = PoolDetail::Concentrated(chain::detail::Concentrated {
        kind: UNI_V3,
        id: B256::ZERO,
        home: Address::ZERO,
        hooks: Address::ZERO,
        token0,
        token1,
        fee_pips: Some(3000),
        protocol_fee: 0,
        tick_spacing: 60,
        sqrt_price_x96: U256::from(1u8) << 96,
        tick: 0,
        liquidity: liquidity as u128,
        next_scan: None,
        bitmaps: Vec::new(),
        ticks: vec![
            chain::detail::TickWord {
                tick: -60,
                liquidity_gross: liquidity as u128,
                liquidity_net: liquidity,
            },
            chain::detail::TickWord {
                tick: 60,
                liquidity_gross: liquidity as u128,
                liquidity_net: -liquidity,
            },
        ],
        flags: 0,
    });
    let curve = crate::state::curve_for_snapshot(&detail, 4).unwrap();
    let fit = fitter::decode_fit(&curve.payload).unwrap();
    assert_eq!(fit.reason, 0);
    assert!(fit
        .directions
        .iter()
        .all(|direction| direction.name == "ladder"));
    assert_eq!(fit.directions.len(), 2);
    let off = U256::from(1_000u64);
    for (token_in, token_out) in [(token0, token1), (token1, token0)] {
        let fitted = crate::state::estimate(&curve, 4, token_in, token_out, off).unwrap();
        let direct = detail.quote(token_in, token_out, off).unwrap();
        assert_eq!(fitted.amount_out, direct.amount_out);
    }
}

#[test]
fn grid_is_increasing_and_ends_at_the_domain() {
    let max_in = U256::from(1_000_000_000u64);
    let amounts = grid_amounts(max_in);
    assert_eq!(amounts.len(), GRID.len());
    assert_eq!(amounts.first().unwrap().1, max_in / U256::from(10_000u32));
    assert_eq!(amounts.last().unwrap().1, max_in);
    for pair in amounts.windows(2) {
        assert!(pair[0].1 < pair[1].1);
    }
    let tiny = grid_amounts(U256::from(50u64));
    let mut seen = tiny.clone();
    seen.dedup_by_key(|(_, amount)| *amount);
    assert_eq!(seen.len(), tiny.len());
    assert_eq!(tiny.last().unwrap().0, "1");
}
