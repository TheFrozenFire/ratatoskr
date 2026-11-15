use super::abi::*;
use super::domain::*;
use super::execute::*;
use super::extract::*;
use super::prepare::*;
use super::*;
use crate::catalog::UNI_V3;
use crate::state::CURVE_VERSION;
use alloy_primitives::{address, Address, B256, U256};
use chain::detail::{ConstantProduct, PoolDetail};
use serde_json::Value;
use std::collections::BTreeMap;
use store_model::UNI_V2;

fn hex_of(bytes: &[u8]) -> String {
    hex_prefix(bytes)
}

#[test]
fn a_transport_error_makes_the_quote_row_partial() {
    let partial = br#"{"directions":[{"samples":[{"chain_error":"Unexpected error"}]}]}"#;
    let finished = br#"{"directions":[{"samples":[{"chain_error":"TF"},{"chain_out":"1"}]}]}"#;
    assert!(payload_is_partial(partial));
    assert!(!payload_is_partial(finished));
    assert!(payload_is_partial(b"not json"));
}

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

#[test]
fn encodings_match_cast() {
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let amount = U256::from(1000u64);
    assert_eq!(
        hex_of(&encode_amounts_out(amount, weth, usdc)),
        "0xd06ca61f00000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
    );
    assert_eq!(
        hex_of(&encode_v3_quote(weth, usdc, amount, 3000)),
        "0xc6a5026a000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000003e80000000000000000000000000000000000000000000000000000000000000bb80000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(
        hex_of(&encode_v4_quote(
            address!("0000000000000000000000000000000000000001"),
            weth,
            3000,
            60,
            address!("0000000000000000000000000000000000000002"),
            true,
            amount,
        )),
        "0xaa9d21cb00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000000000000000bb8000000000000000000000000000000000000000000000000000000000000003c0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(
        hex_of(&encode_get_dy(false, 0, 1, amount)),
        "0x5e0d443f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e8"
    );
    assert_eq!(
        hex_of(&encode_get_dy(true, 0, 1, amount)),
        "0x556d6e9f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e8"
    );
    assert_eq!(
        hex_of(&encode_bal3(
            address!("bA1333333333a1BA1108E8412f11850A5C319bA9"),
            weth,
            usdc,
            amount,
        )),
        "0x3ebc54e5000000000000000000000000ba1333333333a1ba1108e8412f11850a5c319ba9000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000003e8000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000000"
    );
    let amounts = encode_amounts_out(amount, weth, usdc);
    let agg = encode_aggregate3(&[(
        address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
        amounts.as_slice(),
    )]);
    assert_eq!(
        hex_of(&agg),
        "0x82ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a4d06ca61f00000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000"
    );
    let mut id = B256::ZERO;
    id[31] = 0xab;
    assert_eq!(
        hex_of(&encode_bal2(id, weth, usdc, amount)),
        "0xf84d066e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000ab0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
    );
}

#[test]
fn aggregate_return_decodes_success_and_revert() {
    let mut data = Vec::new();
    push_u256(&mut data, U256::from(0x20u64));
    push_u256(&mut data, U256::from(2u8));
    push_u256(&mut data, U256::from(0x40u64));
    push_u256(&mut data, U256::from(0xc0u64));
    push_bool(&mut data, true);
    push_u256(&mut data, U256::from(0x40u64));
    push_u256(&mut data, U256::from(32u8));
    push_u256(&mut data, U256::from(42u8));
    push_bool(&mut data, false);
    push_u256(&mut data, U256::from(0x40u64));
    push_u256(&mut data, U256::ZERO);
    let decoded = decode_aggregate(&data).unwrap();
    assert!(decoded[0].0);
    assert_eq!(first_word(&decoded[0].1), Some(U256::from(42u8)));
    assert!(!decoded[1].0);
    assert!(decoded[1].1.is_empty());
    let amounts = {
        let mut raw = Vec::new();
        push_u256(&mut raw, U256::from(0x20u64));
        push_u256(&mut raw, U256::from(2u8));
        push_u256(&mut raw, U256::from(5u8));
        push_u256(&mut raw, U256::from(9u8));
        raw
    };
    let out = interpret(
        "getAmountsOut",
        RawOut {
            ok: true,
            bytes: amounts,
            error: None,
        },
    );
    assert_eq!(out.amount_out, Some(U256::from(9u8)));
}

fn product(kind: u8, pair: Address) -> PoolDetail {
    PoolDetail::ConstantProduct(ConstantProduct {
        kind,
        id: B256::ZERO,
        pair,
        token0: address!("0000000000000000000000000000000000000001"),
        token1: address!("0000000000000000000000000000000000000002"),
        reserve0: U256::from(1u64),
        reserve1: U256::from(1u64),
        fee_pips: Some(25),
        timestamp: 0,
        flags: 0,
    })
}

fn concentrated(kind: u8, home: Address, fee: u32) -> PoolDetail {
    PoolDetail::Concentrated(chain::detail::Concentrated {
        kind,
        id: B256::ZERO,
        home,
        hooks: Address::ZERO,
        token0: address!("0000000000000000000000000000000000000001"),
        token1: address!("0000000000000000000000000000000000000002"),
        fee_pips: Some(fee),
        protocol_fee: 0,
        tick_spacing: 1,
        sqrt_price_x96: U256::ZERO,
        tick: 0,
        liquidity: 0,
        next_scan: None,
        bitmaps: Vec::new(),
        ticks: Vec::new(),
        flags: 0,
    })
}

fn metric_pool() -> PoolDetail {
    PoolDetail::Metric(chain::detail::MetricPool {
        kind: crate::catalog::METRIC,
        id: B256::ZERO,
        pool: address!("aef2ad7aBB651945e6D24276715f25BDED0ad41e"),
        pause_level: 0,
        bin_index: 0,
        position: 0,
        distance_e6: 0,
        spread_fee_e6: 0,
        notional_fee_e8: 0,
        asymmetry_x8: 0,
        scale0: U256::ZERO,
        scale1: U256::ZERO,
        lowest_bin: 0,
        highest_bin: 0,
        token0: address!("0000000000000000000000000000000000000001"),
        token1: address!("0000000000000000000000000000000000000002"),
        reference_x64: U256::ZERO,
        provider: Address::ZERO,
        next_scan: None,
        bins: Vec::new(),
        flags: 0,
    })
}

fn ready_of(prep: Prep) -> Prepared {
    match prep {
        Prep::Ready(prepared) => prepared,
        Prep::Skip(reason) => panic!("skipped: {reason}"),
    }
}

#[test]
fn execution_quotes_swap_through_the_lens() {
    let token_in = address!("0000000000000000000000000000000000000001");
    let token_out = address!("0000000000000000000000000000000000000002");
    let pair = address!("00000000000000000000000000000000000000aa");
    let amount = U256::from(1_000_000u64);
    let dx = ready_of(prepare(
        &product(store_model::V2_FORK, pair),
        token_in,
        token_out,
        amount,
        1,
    ));
    assert_eq!(dx.target, QUOTE_LENS);
    assert_eq!(dx.method, "swap");
    assert_eq!(dx.token_in, token_in);
    assert!(!dx.fund_eth);
    assert_eq!(
        &dx.data[..4],
        &selector("quoteV2(address,address,address,uint256,uint256)")
    );
    // fee_pips 25 is 25 bps, stored as 2500 pips.
    assert_eq!(word_at(&dx.data[4..], 4), Some(U256::from(2500u32)));
    match prepare(
        &product(store_model::V2_FORK, Address::ZERO),
        token_in,
        token_out,
        amount,
        1,
    ) {
        Prep::Skip(reason) => assert_eq!(reason, "pair"),
        Prep::Ready(_) => panic!("pair is unset"),
    }
    match prepare(
        &product(store_model::V2_FORK, pair),
        token_in,
        token_out,
        amount,
        0,
    ) {
        Prep::Skip(reason) => assert_eq!(reason, "snapshot block is 0"),
        Prep::Ready(_) => panic!("block 0"),
    }

    let home = address!("2D90ebFfC87BBE29E4d78e7627adFE9D0f3D7978");
    let aero = ready_of(prepare(
        &concentrated(crate::catalog::AERO, home, 1),
        token_in,
        token_out,
        amount,
        1,
    ));
    assert_eq!(aero.target, QUOTE_LENS);
    assert_eq!(aero.family, Family::Aero);
    assert_eq!(
        &aero.data[..4],
        &selector("quoteV3(address,address,address,bool,int256)")
    );
    let fork = ready_of(prepare(
        &concentrated(crate::catalog::V3_FORK, home, 3000),
        token_in,
        token_out,
        amount,
        1,
    ));
    assert_eq!(fork.target, QUOTE_LENS);
    assert_eq!(fork.method, "swap");
    match prepare(
        &concentrated(crate::catalog::V3_FORK, Address::ZERO, 3000),
        token_in,
        token_out,
        amount,
        1,
    ) {
        Prep::Skip(reason) => assert_eq!(reason, "pool"),
        Prep::Ready(_) => panic!("pool is unset"),
    }

    let metric = ready_of(prepare(&metric_pool(), token_in, token_out, amount, 1));
    assert_eq!(metric.target, QUOTE_LENS);
    assert_eq!(metric.family, Family::Metric);
    assert_eq!(
        &metric.data[..4],
        &selector("quoteMetric(address,address,address,bool,int128,uint128)")
    );
    assert_eq!(word_at(&metric.data[4..], 5), Some(U256::ZERO));
    let back = ready_of(prepare(&metric_pool(), token_out, token_in, amount, 1));
    assert_eq!(word_at(&back.data[4..], 5), Some(U256::from(u128::MAX)));
    let too_big = U256::from(i128::MAX as u128) + U256::from(1u8);
    match prepare(&metric_pool(), token_in, token_out, too_big, 1) {
        Prep::Skip(reason) => assert_eq!(reason, "amount exceeds int128"),
        Prep::Ready(_) => panic!("amount above int128"),
    }
    let mut words = Vec::new();
    push_u256(&mut words, U256::from(100_000_000u64));
    push_u256(&mut words, U256::from(100_099_602u64));
    let out = interpret(
        "quoteLiveExactInSingle",
        RawOut {
            ok: true,
            bytes: words,
            error: None,
        },
    );
    assert_eq!(out.amount_out, Some(U256::from(100_099_602u64)));
    let swapped = interpret(
        "swap",
        RawOut {
            ok: true,
            bytes: {
                let mut raw = Vec::new();
                push_u256(&mut raw, U256::from(42u8));
                raw
            },
            error: None,
        },
    );
    assert_eq!(swapped.amount_out, Some(U256::from(42u8)));

    let funded = execution_override(
        false,
        &BTreeMap::from([(token_in, B256::from(U256::from(7u8)))]),
    );
    let lens = funded.get(QUOTE_LENS.to_string()).unwrap();
    assert!(lens
        .get("code")
        .and_then(Value::as_str)
        .unwrap()
        .starts_with("0x"));
    assert!(lens.get("balance").is_none());
    let diff = funded
        .get(token_in.to_string())
        .unwrap()
        .get("stateDiff")
        .unwrap();
    assert!(diff.as_object().unwrap().len() == 1);
    let with_eth = execution_override(true, &BTreeMap::new());
    assert!(with_eth
        .get(QUOTE_LENS.to_string())
        .unwrap()
        .get("balance")
        .is_some());

    let header = header_value();
    assert_eq!(header["quoters"]["lens"]["method"], "swap");
    assert!(header["quoters"]["lens"]["address"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .ends_with("510e51"));
    assert!(header["gaps"].get("aero").is_none());
    assert!(header["gaps"].get("metric").is_none());
    assert_eq!(
        header["gaps"]["v2_fork"].as_str().unwrap(),
        "A fork is swapped on the pair. The fee is the pair's fee, or 30 bps (Pancake 25 bps) when the fee view was unset."
    );
    assert_eq!(
        header["gaps"]["v3_fork"].as_str().unwrap(),
        "A fork is swapped on the pool. Callbacks are uniswapV3SwapCallback, pancakeV3SwapCallback, and algebraSwapCallback."
    );
    assert_eq!(
        header["gaps"]["snapshot"].as_str().unwrap(),
        "eth_call uses the stored snapshot block. Block 0 is not quoted."
    );
}
