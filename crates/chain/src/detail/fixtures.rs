//! Fixed snapshots for the quote benches.
//!
//! Each shape is large enough that the priced-input search and the fitter see
//! a real grid. The numbers are synthetic.

use super::types::*;
use alloy_primitives::{address, Address, B256, U256};
use store_model::{BALANCER_V2, CURVE, METRIC, UNI_V2, UNI_V3};

pub struct Shape {
    pub name: &'static str,
    pub detail: PoolDetail,
    /// An input inside the priced domain.
    pub probe: U256,
}

pub fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            name: "product",
            detail: PoolDetail::ConstantProduct(product()),
            probe: e18() / U256::from(100u8),
        },
        Shape {
            name: "concentrated",
            detail: PoolDetail::Concentrated(concentrated()),
            probe: e18() / U256::from(1_000u32),
        },
        Shape {
            name: "balancer_weighted",
            detail: PoolDetail::Balancer(balancer_weighted()),
            probe: e18() / U256::from(100u8),
        },
        Shape {
            name: "balancer_stable",
            detail: PoolDetail::Balancer(balancer_stable()),
            probe: e18() / U256::from(100u8),
        },
        Shape {
            name: "curve_stable",
            detail: PoolDetail::Curve(curve_stable()),
            probe: e18() / U256::from(100u8),
        },
        Shape {
            name: "curve_crypto2",
            detail: PoolDetail::Curve(curve_crypto2()),
            probe: U256::from_str_radix("53540425376295386", 10).expect("probe"),
        },
        Shape {
            name: "curve_crypto3",
            detail: PoolDetail::Curve(curve_crypto3()),
            probe: e18() / U256::from(1_000u32),
        },
        Shape {
            name: "metric",
            detail: PoolDetail::Metric(metric()),
            probe: e18() / U256::from(100u8),
        },
    ]
}

pub fn token0() -> Address {
    address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
}

pub fn token1() -> Address {
    address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
}

fn token2() -> Address {
    address!("cccccccccccccccccccccccccccccccccccccccc")
}

fn e18() -> U256 {
    U256::from(10u64).pow(U256::from(18u8))
}

fn product() -> ConstantProduct {
    ConstantProduct {
        kind: UNI_V2,
        id: B256::ZERO,
        pair: address!("1111111111111111111111111111111111111111"),
        token0: token0(),
        token1: token1(),
        reserve0: e18() * U256::from(1_000u32),
        reserve1: e18() * U256::from(2_000u32),
        fee_pips: Some(3000),
        timestamp: 1,
        flags: 0,
    }
}

fn concentrated() -> Concentrated {
    let liquidity = 50_000_000_000_000_000u128;
    let mut ticks = Vec::with_capacity(64);
    for step in 1..=32 {
        let span = step * 60;
        let end = step == 32;
        let net = if end { liquidity as i128 } else { 0 };
        ticks.push(TickWord {
            tick: -span,
            liquidity_gross: liquidity,
            liquidity_net: net,
        });
        ticks.push(TickWord {
            tick: span,
            liquidity_gross: liquidity,
            liquidity_net: -net,
        });
    }
    ticks.sort_by_key(|word| word.tick);
    Concentrated {
        kind: UNI_V3,
        id: B256::ZERO,
        home: Address::ZERO,
        hooks: Address::ZERO,
        token0: token0(),
        token1: token1(),
        fee_pips: Some(3000),
        protocol_fee: 0,
        tick_spacing: 60,
        sqrt_price_x96: U256::from(1u8) << 96,
        tick: 0,
        liquidity,
        next_scan: None,
        bitmaps: Vec::new(),
        ticks,
        flags: 0,
    }
}

fn balancer_weighted() -> BalancerPool {
    let one = e18();
    BalancerPool {
        kind: BALANCER_V2,
        id: B256::ZERO,
        vault: Address::ZERO,
        pool: Address::ZERO,
        fee: Some(one * U256::from(3u8) / U256::from(1_000u32)),
        amp: U256::ZERO,
        amp_precision: U256::ZERO,
        tokens: vec![token0(), token1()],
        balances: vec![one * U256::from(1_000u32), one * U256::from(4_000u32)],
        weights: vec![
            one * U256::from(80u8) / U256::from(100u8),
            one * U256::from(20u8) / U256::from(100u8),
        ],
        scaling: Vec::new(),
        rates: Vec::new(),
        flags: 0,
    }
}

fn balancer_stable() -> BalancerPool {
    let one = e18();
    BalancerPool {
        kind: BALANCER_V2,
        id: B256::ZERO,
        vault: Address::ZERO,
        pool: Address::ZERO,
        fee: Some(one / U256::from(1_000u32)),
        amp: U256::from(100_000u64),
        amp_precision: U256::from(1_000u32),
        tokens: vec![token0(), token1()],
        balances: vec![one * U256::from(1_000_000u32), one * U256::from(1_000_000u32)],
        weights: Vec::new(),
        scaling: Vec::new(),
        rates: Vec::new(),
        flags: FLAG_STABLE,
    }
}

fn curve_stable() -> CurvePool {
    let one = e18();
    CurvePool {
        kind: CURVE,
        id: B256::ZERO,
        pool: Address::ZERO,
        amp: U256::from(100u8),
        fee: Some(U256::from(4_000_000u64)),
        gamma: U256::ZERO,
        d: U256::ZERO,
        mid_fee: U256::ZERO,
        out_fee: U256::ZERO,
        fee_gamma: U256::ZERO,
        coins_indexed_by_uint: true,
        coins: vec![coin(token0(), one * U256::from(1_000_000u32)), coin(token1(), one * U256::from(1_000_000u32))],
        flags: 0,
    }
}

fn curve_crypto2() -> CurvePool {
    CurvePool {
        kind: CURVE,
        id: B256::ZERO,
        pool: Address::ZERO,
        amp: U256::from(400_000u64),
        fee: None,
        gamma: U256::from_str_radix("145000000000000", 10).expect("gamma"),
        d: U256::from_str_radix("863257940499748221795", 10).expect("d"),
        mid_fee: U256::from(26_000_000u64),
        out_fee: U256::from(45_000_000u64),
        fee_gamma: U256::from_str_radix("230000000000000", 10).expect("fee gamma"),
        coins_indexed_by_uint: true,
        coins: vec![
            CurveCoin {
                token: token0(),
                balance: U256::from_str_radix("535404253762953867722", 10).expect("balance"),
                rate: U256::ZERO,
                price_scale: U256::from_str_radix("20038379687692298260", 10).expect("scale"),
                precision: U256::from(1u8),
            },
            CurveCoin {
                token: token1(),
                balance: U256::from_str_radix("17320665696279630959", 10).expect("balance"),
                rate: U256::ZERO,
                price_scale: U256::ZERO,
                precision: U256::from(1u8),
            },
        ],
        flags: FLAG_CRYPTO,
    }
}

fn curve_crypto3() -> CurvePool {
    CurvePool {
        kind: CURVE,
        id: B256::ZERO,
        pool: Address::ZERO,
        amp: U256::from(2_700_000u64),
        fee: None,
        gamma: U256::from(1_300_000_000_000u64),
        d: U256::from_str_radix("23832478032965456732327", 10).expect("d"),
        mid_fee: U256::from(2_999_999u64),
        out_fee: U256::from(80_000_000u64),
        fee_gamma: U256::from(350_000_000_000_000u64),
        coins_indexed_by_uint: true,
        coins: vec![
            CurveCoin {
                token: token0(),
                balance: U256::from(7_949_965_799u64),
                rate: U256::ZERO,
                price_scale: U256::from_str_radix("1928615248180488065580", 10).expect("scale"),
                precision: U256::from(1_000_000_000_000u64),
            },
            CurveCoin {
                token: token1(),
                balance: U256::from_str_radix("4113599969321168658", 10).expect("balance"),
                rate: U256::ZERO,
                price_scale: U256::from_str_radix("9373305418011774631", 10).expect("scale"),
                precision: U256::from(1u8),
            },
            CurveCoin {
                token: token2(),
                balance: U256::from_str_radix("848042467675018410911", 10).expect("balance"),
                rate: U256::ZERO,
                price_scale: U256::ZERO,
                precision: U256::from(1u8),
            },
        ],
        flags: FLAG_CRYPTO,
    }
}

fn metric() -> MetricPool {
    let scaled = 1_000_000_000_000_000_000u128;
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
        token0: token0(),
        token1: token1(),
        reference_x64: U256::from(1u8) << 64,
        provider: Address::ZERO,
        next_scan: None,
        bins: vec![MetricBin {
            index: 0,
            token0_scaled: scaled,
            token1_scaled: scaled,
            length_e6: 0,
            add_fee_buy_e6: 0,
            add_fee_sell_e6: 0,
        }],
        flags: 0,
    }
}

fn coin(token: Address, balance: U256) -> CurveCoin {
    CurveCoin {
        token,
        balance,
        rate: U256::ZERO,
        price_scale: U256::ZERO,
        precision: U256::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_quotes_its_probe() {
        for shape in shapes() {
            let quote = shape
                .detail
                .quote(token0(), token1(), shape.probe)
                .unwrap_or_else(|err| panic!("{}: {err}", shape.name));
            assert!(
                !quote.amount_out.is_zero(),
                "{} pays nothing",
                shape.name
            );
        }
    }
}
