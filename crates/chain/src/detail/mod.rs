//! Pool state for a swap and for the input-output curve.
//!
//! One creation-code `eth_call` of `contracts/lens/DetailLens.sol` reads a
//! batch of pools. Concentrated pools come back with the non-zero tick-bitmap
//! words. A second call reads the liquidity word of each initialized tick:
//! `eth_getStorageValues` when the provider implements it, otherwise one or more
//! `TickLens` calls. Tick reads stay out of the detail call so the two do not
//! share a gas limit or a response-size budget.

mod codec;
mod fixtures;
mod load;
mod quote;
mod slots;
mod types;

#[cfg(test)]
use crate::rpc::Rpc;
#[cfg(test)]
use crate::seeds::document;
#[cfg(test)]
use alloy_primitives::{Address, B256, I256, U256};
#[cfg(test)]
use store_model::{
    VenueRow, BALANCER_V2, BALANCER_V3, CURVE, METRIC, PANCAKE_V2, UNI_V2, UNI_V3, UNI_V4, V2_FORK,
};

pub use codec::decode_details;
pub use fixtures::{shapes, token0, token1, Shape};
pub use load::{call_data, load_details, load_details_at};
pub use quote::{concentrated_end, concentrated_ladder, product_fee_pips, v3_raw_balance};
pub use slots::tick_request;
pub use types::{
    BalancerPool, BitmapWord, Concentrated, ConstantProduct, CurveCoin, CurvePool, DetailQuery,
    FailedRead, MetricBin, MetricPool, PoolDetail, Quote, ScanFrom, TickWord, FLAG_CRYPTO,
    FLAG_FAILED, FLAG_NO_REFERENCE, FLAG_STABLE, FLAG_TRUNCATED,
};

#[cfg(test)]
pub(crate) use codec::{scan_all, MAGIC};
#[cfg(test)]
pub(crate) use load::{address_word, creation_code, u256_from_i32};
#[cfg(test)]
pub(crate) use quote::{e18, quote_ladder, sqrt_at_tick};
#[cfg(test)]
pub(crate) use slots::{attach_ticks, tick_storage, v4_state_slot};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use std::str::FromStr;

    fn query(kind: u8) -> DetailQuery {
        DetailQuery {
            kind,
            id: B256::repeat_byte(0x11),
            home: address!("1111111111111111111111111111111111111111"),
            aux: Address::ZERO,
            scan_from: ScanFrom::Start,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            tick_spacing: 60,
        }
    }

    fn push(buf: &mut Vec<u8>, word: U256) {
        buf.extend_from_slice(&word.to_be_bytes::<32>());
    }

    #[test]
    fn bytecode_carries_the_detail_magic() {
        assert_eq!(keccak256("ratatoskr.detail.v1").0, MAGIC);
        let code = creation_code();
        assert!(code.len() < 49_152);
        assert!(code
            .windows(33)
            .any(|window| window[0] == 0x7f && window[1..] == MAGIC));
    }

    #[test]
    fn constructor_argument_is_a_dynamic_array() {
        let data = call_data(&[query(UNI_V2)]);
        let args = &data[creation_code().len()..];
        assert_eq!(args.len(), 32 * (2 + 8));
        assert_eq!(args[31], 0x20);
        assert_eq!(args[63], 8);
        assert_eq!(args[95], UNI_V2);
    }

    #[test]
    fn constant_product_fee_scales_hundredths_and_defaults() {
        let mut pool = ConstantProduct {
            kind: V2_FORK,
            id: B256::ZERO,
            pair: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(1_000_000u64),
            fee_pips: Some(30),
            timestamp: 0,
            flags: 0,
        };
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(30u8));
        pool.kind = PANCAKE_V2;
        pool.fee_pips = None;
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(25u8));
        pool.kind = UNI_V2;
        pool.fee_pips = None;
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(30u8));
        assert_eq!(quote.amount_out, U256::from(9871u64));
    }

    fn pool_quote(pool: &ConstantProduct, amount: u64) -> Quote {
        PoolDetail::ConstantProduct(pool.clone())
            .quote(pool.token0, pool.token1, U256::from(amount))
            .unwrap()
    }

    #[test]
    fn in_range_concentrated_step_matches_the_fee() {
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            fee_pips: Some(0),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::from(1u8) << 96,
            tick: 0,
            liquidity: 1_000_000,
            next_scan: None,
            bitmaps: Vec::new(),
            ticks: Vec::new(),
            flags: 0,
        };
        let quote = PoolDetail::Concentrated(pool.clone())
            .quote(pool.token0, pool.token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(999u64));
        assert!(!quote.exhausted);
        let mut charged = pool;
        charged.fee_pips = Some(3000);
        let quote = PoolDetail::Concentrated(charged.clone())
            .quote(charged.token0, charged.token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.fee_paid, U256::from(3u8));
        assert_eq!(quote.amount_out, U256::from(996u64));
    }

    #[test]
    fn tick_zero_is_q96_and_the_usdc_weth_slot_matches() {
        assert_eq!(sqrt_at_tick(0).unwrap(), U256::from(1u8) << 96);
        assert!(sqrt_at_tick(1).unwrap() > U256::from(1u8) << 96);
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"),
            hooks: Address::ZERO,
            token0: Address::ZERO,
            token1: Address::ZERO,
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
            next_scan: None,
            bitmaps: vec![BitmapWord {
                word_pos: 12,
                bits: U256::from(1u8),
            }],
            ticks: Vec::new(),
            flags: 0,
        };
        assert_eq!(pool.initialized_ticks(), vec![184_320]);
        let (_, slot) = tick_storage(&pool, 184_320);
        assert_eq!(
            format!("{slot:064x}"),
            "57c9d69987b7546eb9e8011ddacb32024fac00fff6db7eee60be3d96e6c26961"
        );
        let id = B256::repeat_byte(0x11);
        assert_eq!(
            format!("{:064x}", v4_state_slot(id)),
            "260963edbfdc77b27c31140516df0b13cbfaf61421578f81765a4f60f048b151"
        );
    }

    #[test]
    fn product_payload_round_trips() {
        let query = query(UNI_V2);
        let mut payload = Vec::new();
        push(&mut payload, U256::from_be_bytes(MAGIC));
        push(&mut payload, U256::from(1u8));
        push(&mut payload, U256::from(10u8));
        push(&mut payload, U256::from(UNI_V2));
        push(&mut payload, U256::from_be_bytes(query.id.0));
        push(&mut payload, U256::ZERO);
        push(&mut payload, address_word(query.token0));
        push(&mut payload, address_word(query.token1));
        push(&mut payload, U256::from(5u8));
        push(&mut payload, U256::from(7u8));
        push(&mut payload, U256::MAX);
        push(&mut payload, U256::from(9u8));
        let rows = decode_details(&payload, std::slice::from_ref(&query)).unwrap();
        let PoolDetail::ConstantProduct(pool) = &rows[0] else {
            panic!("product");
        };
        assert_eq!(pool.pair, query.home);
        assert_eq!(pool.reserve0, U256::from(5u8));
        assert_eq!(pool.fee_pips, None);
        assert_eq!(pool.timestamp, 9);
    }

    #[test]
    fn stableswap_two_coin_quote_matches_the_integer_invariant() {
        let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let coin = |token| CurveCoin {
            token,
            balance: e18(),
            rate: U256::ZERO,
            price_scale: U256::ZERO,
            precision: U256::ZERO,
        };
        let pool = CurvePool {
            kind: CURVE,
            id: B256::ZERO,
            pool: Address::ZERO,
            amp: U256::from(100u8),
            fee: Some(U256::ZERO),
            gamma: U256::ZERO,
            d: U256::ZERO,
            mid_fee: U256::ZERO,
            out_fee: U256::ZERO,
            fee_gamma: U256::ZERO,
            coins_indexed_by_uint: true,
            coins: vec![coin(token0), coin(token1)],
            flags: 0,
        };
        let quote = PoolDetail::Curve(pool)
            .quote(token0, token1, U256::from(10_000_000_000_000_000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(9_999_009_901_970_393u64));
        let again = |amount: u64| {
            let pool = CurvePool {
                kind: CURVE,
                id: B256::ZERO,
                pool: Address::ZERO,
                amp: U256::from(100u8),
                fee: Some(U256::ZERO),
                gamma: U256::ZERO,
                d: U256::ZERO,
                mid_fee: U256::ZERO,
                out_fee: U256::ZERO,
                fee_gamma: U256::ZERO,
                coins_indexed_by_uint: true,
                coins: vec![coin(token0), coin(token1)],
                flags: 0,
            };
            PoolDetail::Curve(pool)
                .quote(token0, token1, U256::from(amount))
                .unwrap()
                .amount_out
        };
        assert_eq!(again(1), U256::ZERO);
        assert_eq!(again(1_000_000_000_000), U256::from(999_999_990_099u64));
        assert_eq!(
            again(100_000_000_000_000_000),
            U256::from(99_900_110_864_758_514u64)
        );
        assert_eq!(
            again(500_000_000_000_000_000),
            U256::from(496_752_702_498_812_842u64)
        );
    }

    #[test]
    fn equal_weight_balancer_is_constant_product() {
        let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let pool = BalancerPool {
            kind: BALANCER_V2,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::ZERO),
            amp: U256::ZERO,
            amp_precision: U256::ZERO,
            tokens: vec![token0, token1],
            balances: vec![U256::from(1_000_000u64), U256::from(1_000_000u64)],
            weights: vec![e18() / U256::from(2u8), e18() / U256::from(2u8)],
            scaling: Vec::new(),
            rates: Vec::new(),
            flags: 0,
        };
        let quote = PoolDetail::Balancer(pool)
            .quote(token0, token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(999u64));
    }

    #[test]
    fn balancer_v3_scales_live_balances_back_to_raw() {
        let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let scale0 = U256::from(1_000_000_000_000u64);
        let mut pool = BalancerPool {
            kind: BALANCER_V3,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::ZERO),
            amp: U256::ZERO,
            amp_precision: U256::ZERO,
            tokens: vec![token0, token1],
            balances: vec![scale0 * U256::from(1_000_000u64), U256::from(1_000_000u64)],
            weights: vec![e18() / U256::from(2u8), e18() / U256::from(2u8)],
            scaling: Vec::new(),
            rates: Vec::new(),
            flags: 0,
        };
        assert!(PoolDetail::Balancer(pool.clone())
            .quote(token0, token1, U256::from(1000u64))
            .is_err());
        pool.scaling = vec![scale0, U256::from(1u8)];
        pool.rates = vec![e18(), e18()];
        let quote = PoolDetail::Balancer(pool)
            .quote(token0, token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_in, U256::from(1000u64));
        assert_eq!(quote.amount_out, U256::from(999u64));
    }

    #[test]
    fn balancer_v3_live_balance_already_includes_the_rate() {
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let rate_in = e18() * U256::from(2u8);
        let rate_out = e18() * U256::from(4u8);
        let scale_out = U256::from(10_000_000_000u64);
        let pool = BalancerPool {
            kind: BALANCER_V3,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::ZERO),
            amp: U256::ZERO,
            amp_precision: U256::ZERO,
            tokens: vec![weth, wbtc],
            balances: vec![
                U256::from(1000u64) * rate_in / e18(),
                U256::from(1000u64) * scale_out * rate_out / e18(),
            ],
            weights: vec![e18() / U256::from(2u8), e18() / U256::from(2u8)],
            scaling: vec![U256::from(1u8), scale_out],
            rates: vec![rate_in, rate_out],
            flags: 0,
        };
        let quote = PoolDetail::Balancer(pool)
            .quote(weth, wbtc, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(500u64));
    }

    #[test]
    fn balancer_v3_dust_stable_pays_at_most_the_raw_balance() {
        let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let scale_btc = U256::from(10_000_000_000u64);
        let scale_usdc = U256::from(1_000_000_000_000u64);
        let rate_btc = U256::from_str("67295400000000000000000").unwrap();
        let rate_eth = U256::from_str("1904291978630000000000").unwrap();
        let pool = BalancerPool {
            kind: BALANCER_V3,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::from(1_000_000_000_000_000u64)),
            amp: U256::from(500_000u64),
            amp_precision: U256::from(1_000u64),
            tokens: vec![wbtc, usdc, weth],
            balances: vec![
                U256::from_str("672954000000000").unwrap(),
                U256::from(1_000_000_000_000u64),
                U256::from(199_950u64),
            ],
            weights: Vec::new(),
            scaling: vec![scale_btc, scale_usdc, U256::from(1u8)],
            rates: vec![rate_btc, e18(), rate_eth],
            flags: FLAG_STABLE,
        };
        let quote = PoolDetail::Balancer(pool).quote(weth, wbtc, U256::from(199_950_000u64));
        // Raw WBTC is 1 sat. A quote that still applies the rate twice pays tens of thousands.
        // These live balances do not converge, so the direction is left unquoted.
        match quote {
            Ok(quote) => assert!(quote.amount_out <= U256::from(1u8)),
            Err(err) => assert_eq!(err, "invariant"),
        }
    }

    #[test]
    fn balancer_v3_stable_rate_quote_stays_inside_the_raw_balance() {
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
        let live = U256::from(10u64).pow(U256::from(20u8));
        let rate_eth = e18() * U256::from(2u8);
        let pool = BalancerPool {
            kind: BALANCER_V3,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::ZERO),
            amp: U256::from(500_000u64),
            amp_precision: U256::from(1_000u64),
            tokens: vec![weth, dai],
            balances: vec![live, live],
            weights: Vec::new(),
            scaling: vec![U256::from(1u8), U256::from(1u8)],
            rates: vec![rate_eth, e18()],
            flags: FLAG_STABLE,
        };
        let raw_in = U256::from(10u64).pow(U256::from(18u8));
        let quote = PoolDetail::Balancer(pool).quote(weth, dai, raw_in).unwrap();
        assert_eq!(
            quote.amount_out,
            U256::from_str("1999920131050144065").unwrap()
        );
    }

    #[test]
    fn tricrypto_quote_matches_get_dy_and_rejects_a_false_amp() {
        let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let other = address!("41d5d79431a913c4ae7d69a668ecdfe5ff9dfb68");
        let mut pool = CurvePool {
            kind: CURVE,
            id: B256::ZERO,
            pool: Address::ZERO,
            amp: U256::from(2_700_000u64),
            fee: None,
            gamma: U256::from(1_300_000_000_000u64),
            d: U256::from_str("23832478032965456732327").unwrap(),
            mid_fee: U256::from(2_999_999u64),
            out_fee: U256::from(80_000_000u64),
            fee_gamma: U256::from(350_000_000_000_000u64),
            coins_indexed_by_uint: true,
            coins: vec![
                CurveCoin {
                    token: usdc,
                    balance: U256::from(7_949_965_799u64),
                    rate: U256::ZERO,
                    price_scale: U256::from_str("1928615248180488065580").unwrap(),
                    precision: U256::from(1_000_000_000_000u64),
                },
                CurveCoin {
                    token: weth,
                    balance: U256::from_str("4113599969321168658").unwrap(),
                    rate: U256::ZERO,
                    price_scale: U256::from_str("9373305418011774631").unwrap(),
                    precision: U256::from(1u8),
                },
                CurveCoin {
                    token: other,
                    balance: U256::from_str("848042467675018410911").unwrap(),
                    rate: U256::ZERO,
                    price_scale: U256::ZERO,
                    precision: U256::from(1u8),
                },
            ],
            flags: FLAG_CRYPTO,
        };
        let dx = U256::from(1_000_000_000_000_000u64);
        let quote = PoolDetail::Curve(pool.clone())
            .quote(weth, usdc, dx)
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(1_928_233u64));
        pool.coins[0].precision = U256::ZERO;
        assert!(PoolDetail::Curve(pool.clone())
            .quote(weth, usdc, dx)
            .is_err());
        pool.coins[0].precision = U256::from(1_000_000_000_000u64);
        pool.amp = U256::from(1_000_000_000_000u64) + U256::from(1u8);
        assert!(PoolDetail::Curve(pool).quote(weth, usdc, dx).is_err());
    }

    #[test]
    fn metric_price_walks_the_bin_and_caps_at_the_balance() {
        let mut pool = sample_metric();
        let token_in = pool.token0;
        let token_out = pool.token1;
        assert_eq!(pool.marginal_price_x64().unwrap(), U256::from(1u8) << 64);
        pool.distance_e6 = 0;
        pool.bins[0].length_e6 = 65_535;
        pool.position = (1u128 << 104) - 1;
        let upper = ((U256::from(1u8) << 64) * U256::from(1_000_000u64 + 65_535))
            / U256::from(1_000_000u64);
        assert_eq!(pool.marginal_price_x64().unwrap(), upper);
        pool.position = 0;
        pool.bins[0].length_e6 = 0;
        let quote = PoolDetail::Metric(pool.clone())
            .quote(token_in, token_out, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(1000u64));
        let mut thin = pool;
        thin.bins[0].token1_scaled = 40;
        let quote = PoolDetail::Metric(thin)
            .quote(token_in, token_out, U256::from(1000u64))
            .unwrap();
        assert!(quote.exhausted);
        assert_eq!(quote.amount_out, U256::from(40u8));
    }

    #[test]
    fn metric_whole_token_price_scales_raw_output_and_an_empty_bin_pays_nothing() {
        let mut pool = sample_metric();
        pool.scale0 = U256::from(1u8);
        pool.scale1 = U256::from(1_000_000_000_000u64);
        pool.reference_x64 = U256::from(1u8) << 64;
        pool.bins[0].token1_scaled = 1_000_000_000_000_000_000;
        let quote = PoolDetail::Metric(pool.clone())
            .quote(pool.token0, pool.token1, e18())
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(1_000_000u64));
        assert!(!quote.exhausted);
        pool.bins[0].token1_scaled = 0;
        let quote = PoolDetail::Metric(pool.clone())
            .quote(pool.token0, pool.token1, e18())
            .unwrap();
        assert!(quote.exhausted);
        assert_eq!(quote.amount_out, U256::ZERO);
    }

    fn sample_metric() -> MetricPool {
        MetricPool {
            kind: METRIC,
            id: B256::ZERO,
            pool: Address::ZERO,
            pause_level: 0,
            bin_index: 0,
            position: 0,
            distance_e6: 0,
            spread_fee_e6: 0,
            notional_fee_e8: 0,
            asymmetry_x8: 0,
            scale0: U256::from(1u8),
            scale1: U256::from(1u8),
            lowest_bin: 0,
            highest_bin: 0,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reference_x64: U256::from(1u8) << 64,
            provider: Address::ZERO,
            next_scan: None,
            bins: vec![MetricBin {
                index: 0,
                token0_scaled: 1_000,
                token1_scaled: 1_000,
                length_e6: 0,
                add_fee_buy_e6: 0,
                add_fee_sell_e6: 0,
            }],
            flags: 0,
        }
    }

    #[test]
    fn v4_query_uses_the_manager_and_the_hooks() {
        let hooks = address!("5555555555555555555555555555555555555555");
        let row = VenueRow {
            kind: UNI_V4,
            id: B256::repeat_byte(0x22),
            factory: document().by_name("uni-v4").address,
            tokens: vec![
                address!("0000000000000000000000000000000000000000"),
                address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: None,
            extra: Some(hooks),
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let query = DetailQuery::from_row(&row);
        assert_eq!(query.home, document().by_name("uni-v4").address);
        assert_eq!(query.aux, hooks);
        assert_eq!(query.tick_spacing, 60);
    }

    #[test]
    #[ignore = "Alchemy detail call plus tick lens for the USDC/WETH 0.3% pool"]
    fn live_uni_v3_usdc_weth_tick_word_matches_ticks() {
        let rpc = Rpc::new(vec![alchemy_url()])
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        let pool = address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8");
        let row = VenueRow {
            kind: UNI_V3,
            id: VenueRow::address_id(pool),
            factory: document().by_name("uni-v3").address,
            tokens: vec![
                address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let mut details = load_details(&rpc, &[row])
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        let PoolDetail::Concentrated(detail) = &details[0] else {
            panic!("uniswap v3 detail");
        };
        assert_eq!(detail.fee_pips, Some(3000));
        assert_eq!(detail.tick_spacing, 60);
        assert_eq!(detail.home, pool);
        assert!(!detail.sqrt_price_x96.is_zero());
        assert!(detail.liquidity > 0);
        assert!(
            detail.ticks.is_empty(),
            "detail lens must not read tick words"
        );
        attach_ticks(&rpc, &mut details, None)
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        let PoolDetail::Concentrated(detail) = &details[0] else {
            panic!("uniswap v3 detail");
        };
        assert!(!detail.ticks.is_empty(), "bitmap produced no ticks");
        assert_eq!(detail.ticks.len(), detail.initialized_ticks().len());
        assert_eq!(detail.flags & FLAG_FAILED, 0);
        for sample in [
            detail.ticks.first(),
            detail.ticks.get(detail.ticks.len() / 2),
        ]
        .into_iter()
        .flatten()
        {
            assert_tick_view(&rpc, pool, sample);
        }
    }

    fn assert_tick_view(rpc: &Rpc, pool: Address, sample: &TickWord) {
        let mut data = vec![0xf3, 0x0d, 0xba, 0x93];
        data.extend_from_slice(&u256_from_i32(sample.tick).to_be_bytes::<32>());
        let call = rpc
            .eth_call(&data, Some(pool), 1_000_000)
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        assert!(!call.reverted, "{}", redact(&call.message));
        assert!(call.bytes.len() >= 64, "tick {}", sample.tick);
        // `ticks()` ABI-encodes gross and net as separate words. The lens packs them into one.
        let gross = U256::from_be_slice(&call.bytes[..32]);
        let net = I256::from_be_bytes::<32>(call.bytes[32..64].try_into().unwrap());
        assert_eq!(
            gross,
            U256::from(sample.liquidity_gross),
            "tick {}",
            sample.tick
        );
        assert_eq!(
            net,
            I256::try_from(sample.liquidity_net).unwrap(),
            "tick {}",
            sample.tick
        );
    }

    fn alchemy_url() -> String {
        let text = std::fs::read_to_string("/home/jmart/git/TheFrozenFire/ratatoskr/.env")
            .expect("env file");
        for line in text.lines() {
            let Some(key) = line.strip_prefix("ALCHEMY_API_KEY=") else {
                continue;
            };
            let key = key.trim().trim_matches('"');
            assert!(!key.is_empty());
            return format!("https://eth-mainnet.g.alchemy.com/v2/{key}");
        }
        panic!("ALCHEMY_API_KEY");
    }

    fn redact(text: &str) -> String {
        let marker = "eth-mainnet.g.alchemy.com/v2/";
        let Some(start) = text.find(marker) else {
            return text.to_string();
        };
        let from = start + marker.len();
        let end = text[from..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .map(|index| from + index)
            .unwrap_or(text.len());
        let mut out = text.to_string();
        out.replace_range(from..end, "redacted");
        out
    }

    #[test]
    fn ladder_matches_quote_across_a_tick() {
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::from(1u8) << 96,
            tick: 0,
            liquidity: 1_000_000,
            next_scan: None,
            bitmaps: Vec::new(),
            ticks: vec![
                TickWord {
                    tick: -120,
                    liquidity_gross: 5,
                    liquidity_net: -400_000,
                },
                TickWord {
                    tick: 120,
                    liquidity_gross: 5,
                    liquidity_net: 400_000,
                },
            ],
            flags: 0,
        };
        let ladder = concentrated_ladder(&pool).unwrap();
        assert!(ladder.zero_for_one.len() >= 2);
        let mut amounts = vec![
            U256::from(1u8),
            U256::from(1_000u64),
            U256::from(1_000_000u64),
        ];
        if let Some(step) = ladder.zero_for_one.first() {
            amounts.push(step.input);
            if !step.input.is_zero() {
                amounts.push(step.input - U256::from(1u8));
            }
        }
        amounts.push(U256::from(10u64).pow(U256::from(30u8)));
        for amount in amounts {
            for (token_in, token_out) in [(pool.token0, pool.token1), (pool.token1, pool.token0)] {
                let direct = PoolDetail::Concentrated(pool.clone())
                    .quote(token_in, token_out, amount)
                    .unwrap();
                let via = quote_ladder(&pool, &ladder, token_in, token_out, amount).unwrap();
                assert_eq!(via, direct, "{amount}");
            }
        }
    }

    #[test]
    fn ladder_crosses_the_initialized_tick_under_the_price() {
        let sqrt = sqrt_at_tick(200).unwrap();
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: sqrt,
            tick: 200,
            liquidity: 0,
            next_scan: None,
            bitmaps: Vec::new(),
            ticks: vec![
                TickWord {
                    tick: 0,
                    liquidity_gross: 1_000,
                    liquidity_net: 1_000,
                },
                TickWord {
                    tick: 200,
                    liquidity_gross: 1_000,
                    liquidity_net: -1_000,
                },
            ],
            flags: 0,
        };
        let ladder = concentrated_ladder(&pool).unwrap();
        let step = ladder.zero_for_one.first().expect("down ladder");
        assert_eq!(step.liquidity, 1_000);
        assert_eq!(step.sqrt_start, sqrt);
        // Price sits above tick 200, so that tick is already crossed and the
        // in-range liquidity is zero. The downward walk has to uncross it
        // before quoting the range underneath.
        let above = sqrt_at_tick(201).unwrap();
        let mut mid = pool.clone();
        mid.sqrt_price_x96 = sqrt + (above - sqrt) / U256::from(2u8);
        let ladder = concentrated_ladder(&mid).unwrap();
        assert_eq!(ladder.zero_for_one.len(), 1);
        assert_eq!(ladder.zero_for_one[0].liquidity, 1_000);
        assert_eq!(ladder.zero_for_one[0].sqrt_start, sqrt);
    }

    #[test]
    fn load_details_does_not_fetch_ticks() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let mut answered = 0_usize;
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(5) {
                if shutdown_rx.try_recv().is_ok() && answered > 0 {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        answered += 1;
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut collected = Vec::new();
                        let mut buf = [0u8; 8192];
                        while collected.len() < 1024 * 1024 {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    collected.extend_from_slice(&buf[..n]);
                                    if http_complete(&collected) {
                                        break;
                                    }
                                }
                                Err(err)
                                    if err.kind() == std::io::ErrorKind::WouldBlock
                                        || err.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    if http_complete(&collected) {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&collected);
                        assert!(text.contains("\"method\":\"eth_call\""), "{text}");
                        assert!(!text.contains("eth_getStorageValues"), "{text}");
                        let body = format!(
                            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{{\"code\":3,\"message\":\"revert\",\"data\":\"0x{}\"}}}}",
                            alloy_primitives::hex::encode(detail_with_bitmap())
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            answered
        });
        let pool = address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8");
        let row = VenueRow {
            kind: UNI_V3,
            id: VenueRow::address_id(pool),
            factory: Address::ZERO,
            tokens: vec![
                address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let rpc = Rpc::new(vec![format!("http://127.0.0.1:{port}")]).unwrap();
        let details = load_details(&rpc, &[row]).unwrap();
        let _ = shutdown_tx.send(());
        let answered = server.join().unwrap();
        assert_eq!(answered, 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let PoolDetail::Concentrated(detail) = &details[0] else {
            panic!("concentrated");
        };
        assert!(detail.ticks.is_empty());
        assert_eq!(detail.initialized_ticks(), vec![0]);
    }

    fn http_complete(bytes: &[u8]) -> bool {
        let Some(split) = bytes.windows(4).position(|mark| mark == b"\r\n\r\n") else {
            return false;
        };
        let header = String::from_utf8_lossy(&bytes[..split]);
        let length = header.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        match length {
            Some(length) => bytes.len() >= split + 4 + length,
            None => true,
        }
    }

    fn detail_with_bitmap() -> Vec<u8> {
        let id = VenueRow::address_id(address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"));
        let mut body = Vec::new();
        push(
            &mut body,
            address_word(address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
        );
        push(
            &mut body,
            address_word(address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
        );
        push(&mut body, U256::from(3000u64));
        push(&mut body, U256::from(60u8));
        push(&mut body, U256::from(1u8) << 96);
        push(&mut body, U256::ZERO);
        push(&mut body, U256::from(1_000u64));
        push(&mut body, U256::from_be_bytes(scan_all()));
        push(&mut body, U256::from(1u8));
        push(&mut body, U256::ZERO);
        push(&mut body, U256::from(1u8));
        let mut payload = Vec::new();
        push(&mut payload, U256::from_be_bytes(MAGIC));
        push(&mut payload, U256::from(1u8));
        push(&mut payload, U256::from(4 + body.len() / 32));
        push(&mut payload, U256::from(UNI_V3));
        push(&mut payload, U256::from_be_bytes(id.0));
        push(&mut payload, U256::ZERO);
        payload.extend_from_slice(&body);
        payload
    }
}
