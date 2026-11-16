use std::time::Duration;

use action::Action;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use num_bigint::BigInt;
use optimizer::{quote_curve, Chord, Curve};

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
    group.finish();
}

criterion_group!(benches, quote);
criterion_main!(benches);
