use std::time::Duration;

use alloy_primitives::Address;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use fitter::{choose, decode_fit, encode_fit, eval_formula, labels_of, StoredFit};
use serde_json::Value;

const SHAPES: &[&str] = &[
    "product",
    "concentrated",
    "balancer_weighted",
    "balancer_stable",
    "curve_stable",
    "curve_crypto2",
    "curve_crypto3",
    "metric",
];

fn load(name: &str) -> (Value, Value) {
    let text = match name {
        "product" => include_str!("data/product.json"),
        "concentrated" => include_str!("data/concentrated.json"),
        "balancer_weighted" => include_str!("data/balancer_weighted.json"),
        "balancer_stable" => include_str!("data/balancer_stable.json"),
        "curve_stable" => include_str!("data/curve_stable.json"),
        "curve_crypto2" => include_str!("data/curve_crypto2.json"),
        "curve_crypto3" => include_str!("data/curve_crypto3.json"),
        "metric" => include_str!("data/metric.json"),
        _ => panic!("shape"),
    };
    let value: Value = serde_json::from_str(text).unwrap();
    (value["state"].clone(), value["direction"].clone())
}

fn address(value: &Value) -> Address {
    value.as_str().unwrap().parse().unwrap()
}

fn configure(group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>, heavy: bool) {
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(if heavy {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(400)
    });
}

fn selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("choose");
    configure(&mut group, true);
    for name in SHAPES {
        let (state, direction) = load(name);
        let rows = labels_of(&direction).unwrap();
        assert!(!rows.is_empty(), "{name}");
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(&state, &direction, &rows),
            |b, (state, direction, rows)| {
                b.iter(|| choose(black_box(state), black_box(direction), black_box(rows)).unwrap())
            },
        );
    }
    group.finish();
}

fn payload(c: &mut Criterion) {
    let mut group = c.benchmark_group("payload");
    configure(&mut group, false);
    for name in SHAPES {
        let (state, direction) = load(name);
        let rows = labels_of(&direction).unwrap();
        let chosen = choose(&state, &direction, &rows).unwrap().unwrap();
        let stored = StoredFit {
            token_in: address(direction.get("token_in").unwrap()),
            token_out: address(direction.get("token_out").unwrap()),
            score: chosen.score,
            name: chosen.name.to_string(),
            formula: chosen.formula,
        };
        group.bench_with_input(BenchmarkId::from_parameter(name), &stored, |b, stored| {
            b.iter(|| {
                let bytes = encode_fit(0, black_box(std::slice::from_ref(stored)));
                decode_fit(black_box(&bytes)).unwrap()
            })
        });
    }
    group.finish();
}

fn evaluate(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval");
    configure(&mut group, false);
    for name in SHAPES {
        let (state, direction) = load(name);
        let rows = labels_of(&direction).unwrap();
        let chosen = choose(&state, &direction, &rows).unwrap().unwrap();
        let amount = rows[rows.len() / 2].0.clone();
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(&chosen.formula, &amount),
            |b, (formula, amount)| {
                b.iter(|| eval_formula(black_box(formula), black_box(amount)).unwrap())
            },
        );
    }
    group.finish();
}

criterion_group!(benches, selection, payload, evaluate);
criterion_main!(benches);
