use std::time::Duration;

use std::collections::BTreeMap;

use action::Action;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use num_bigint::BigInt;
use optimizer::{optimize, quote_curve, Chord, Curve, Edge, Input, Output, Problem, Route};

fn bi(text: &str) -> BigInt {
    text.parse().unwrap()
}

fn chords() -> Vec<Chord> {
    let widths = [
        "1000000000000000000",
        "9000000000000000000",
        "90000000000000000000",
        "400000000000000000000",
        "500000000000000000000",
        "1500000000000000000000",
        "2500000000000000000000",
        "3000000000000000000000",
        "2000000000000000000000",
    ];
    let outputs = [
        "1994000000000000000",
        "17613843137254901960",
        "147058823529411764706",
        "500000000000000000000",
        "333333333333333333334",
        "428571428571428571428",
        "238095238095238095238",
        "111111111111111111111",
        "40404040404040404041",
    ];
    widths
        .into_iter()
        .zip(outputs)
        .enumerate()
        .map(|(index, (width, output))| Chord {
            width: bi(width),
            output: bi(output),
            action: swap(index as u8 + 1),
        })
        .collect()
}

fn swap(pool: u8) -> Action {
    let contract = action::Address::new([pool; 20]).expect("contract");
    Action::Swap(
        action::Swap::new(
            contract,
            contract.pool_id(),
            action::Token([1; 20]),
            action::Token([2; 20]),
            action::Invariant::Product,
        )
        .expect("swap"),
    )
}

fn quote(c: &mut Criterion) {
    let curve = Curve::Chords(chords());
    let mut group = c.benchmark_group("optimizer_quote");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_millis(400));
    let small = bi("1000000000000000");
    let mid = bi("3000000000000000000000");
    let past = bi("99999999999999999999999");
    group.bench_function("small", |b| {
        b.iter(|| quote_curve(black_box(&curve), black_box(&small)))
    });
    group.bench_function("mid", |b| {
        b.iter(|| quote_curve(black_box(&curve), black_box(&mid)))
    });
    group.bench_function("past", |b| {
        b.iter(|| quote_curve(black_box(&curve), black_box(&past)))
    });
    let fat = fat_curve();
    let fat_width: BigInt = match &fat {
        Curve::Chords(chords) => chords.iter().map(|chord| &chord.width).sum(),
        Curve::Unlimited => BigInt::from(0),
    };
    let fat_mid = &fat_width / 2;
    let fat_past = &fat_width + 1;
    group.bench_function("fat_mid", |b| {
        b.iter(|| quote_curve(black_box(&fat), black_box(&fat_mid)))
    });
    group.bench_function("fat_past", |b| {
        b.iter(|| quote_curve(black_box(&fat), black_box(&fat_past)))
    });
    group.finish();
}

/// Sixteen pools, each with the nine product chords, slopes scaled by the pool.
fn fat_curve() -> Curve {
    let widths = [
        "1000000000000000000",
        "9000000000000000000",
        "90000000000000000000",
        "400000000000000000000",
        "500000000000000000000",
        "1500000000000000000000",
        "2500000000000000000000",
        "3000000000000000000000",
        "2000000000000000000000",
    ];
    let outputs = [
        "1994000000000000000",
        "17613843137254901960",
        "147058823529411764706",
        "500000000000000000000",
        "333333333333333333334",
        "428571428571428571428",
        "238095238095238095238",
        "111111111111111111111",
        "40404040404040404041",
    ];
    let mut segments = Vec::new();
    for pool in 1..=16u8 {
        for (width, output) in widths.into_iter().zip(outputs) {
            let mut id = [0u8; 32];
            id[31] = pool;
            let mut output = bi(output);
            output *= BigInt::from(pool as i64);
            segments.push(routing::SlopeSegment {
                width: bi(width),
                output,
                action: routing::Action { kind: 1, pool: id },
            });
        }
    }
    let merged = routing::merge(std::slice::from_ref(&segments));
    Curve::Chords(
        merged
            .into_iter()
            .map(|segment| Chord {
                action: swap(segment.action.pool[31]),
                width: segment.width,
                output: segment.output,
            })
            .collect(),
    )
}

fn fat_solve(c: &mut Criterion) {
    let curve = fat_curve();
    let width: BigInt = match &curve {
        Curve::Chords(chords) => chords.iter().map(|chord| &chord.width).sum(),
        Curve::Unlimited => BigInt::from(0),
    };
    let edge = Edge {
        action: swap(1),
        curve,
    };
    let problem = Problem {
        inputs: vec![Input {
            token: action::Token([1; 20]),
            amount: width,
        }],
        outputs: vec![Output {
            token: action::Token([2; 20]),
            weight: BigInt::from(1),
        }],
        edges: vec![edge],
        routes: vec![Route { edges: vec![0] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    let mut group = c.benchmark_group("optimizer_solve");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("fat_conduit", |b| {
        b.iter(|| optimize(black_box(&problem)).expect("plan"))
    });
    group.finish();
}

criterion_group!(benches, quote, fat_solve);
criterion_main!(benches);
