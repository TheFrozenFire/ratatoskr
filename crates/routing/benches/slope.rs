use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use num_bigint::BigInt;
use routing::{quote, segments_from_knots, Action, SlopeSegment};

fn bi(text: &str) -> BigInt {
    text.parse().unwrap()
}

/// Ten cumulative knots on one concave product-shaped curve, around 10^18.
fn knots() -> Vec<(BigInt, BigInt)> {
    let inputs = [
        "0",
        "1000000000000000000",
        "10000000000000000000",
        "100000000000000000000",
        "500000000000000000000",
        "1000000000000000000000",
        "2500000000000000000000",
        "5000000000000000000000",
        "8000000000000000000000",
        "10000000000000000000000",
    ];
    let outputs = [
        "0",
        "1994000000000000000",
        "19607843137254901960",
        "166666666666666666666",
        "666666666666666666666",
        "1000000000000000000000",
        "1428571428571428571428",
        "1666666666666666666666",
        "1777777777777777777777",
        "1818181818181818181818",
    ];
    inputs
        .into_iter()
        .zip(outputs)
        .map(|(input, output)| (bi(input), bi(output)))
        .collect()
}

fn action(pool: u8) -> Action {
    let mut bytes = [0u8; 32];
    bytes[31] = pool;
    Action {
        kind: 1,
        pool: bytes,
    }
}

fn segments(c: &mut Criterion) {
    let mut group = c.benchmark_group("segments");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_millis(400));
    let knots = knots();
    group.bench_function("from_knots", |b| {
        b.iter(|| segments_from_knots(black_box(&knots), action(1)))
    });
    let members: Vec<Vec<SlopeSegment>> = (1..=4)
        .map(|pool| segments_from_knots(&knots, action(pool)))
        .collect();
    group.bench_function("merge_four", |b| {
        b.iter(|| routing::merge(black_box(&members)))
    });
    let merged = routing::merge(&members);
    let mid = bi("3000000000000000000000");
    let small = bi("1000000000000000");
    let past = bi("99999999999999999999999");
    group.bench_function("quote_small", |b| {
        b.iter(|| quote(black_box(&merged), black_box(&small)))
    });
    group.bench_function("quote_mid", |b| {
        b.iter(|| quote(black_box(&merged), black_box(&mid)))
    });
    group.bench_function("quote_past", |b| {
        b.iter(|| quote(black_box(&merged), black_box(&past)))
    });
    let fat_members: Vec<Vec<SlopeSegment>> = (1..=16)
        .map(|pool| {
            segments_from_knots(&knots, action(pool))
                .into_iter()
                .map(|mut segment| {
                    segment.output *= BigInt::from(pool as i64);
                    segment
                })
                .collect()
        })
        .collect();
    group.bench_function("fat_merge", |b| {
        b.iter(|| routing::merge(black_box(&fat_members)))
    });
    let fat = routing::merge(&fat_members);
    let fat_width: BigInt = fat.iter().map(|segment| &segment.width).sum();
    let fat_mid = &fat_width / 2;
    let fat_past = &fat_width + 1;
    group.bench_function("fat_quote_mid", |b| {
        b.iter(|| quote(black_box(&fat), black_box(&fat_mid)))
    });
    group.bench_function("fat_quote_past", |b| {
        b.iter(|| quote(black_box(&fat), black_box(&fat_past)))
    });
    group.finish();
}

criterion_group!(benches, segments);
criterion_main!(benches);
