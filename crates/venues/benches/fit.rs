use criterion::{black_box, criterion_group, criterion_main, Criterion};
use num_bigint::BigInt;
use num_rational::BigRational;
use serde_json::json;

fn bi(value: i64) -> BigInt {
    BigInt::from(value)
}

fn logistic(c: &mut Criterion) {
    let slope = BigRational::from_integer(bi(2));
    let rate = BigRational::new(bi(1), bi(100));
    let knee = BigRational::from_integer(bi(100));
    let amounts: Vec<BigInt> = (1..=32).map(|step| bi(step * 25)).collect();
    c.bench_function("logistic_32", |b| {
        b.iter(|| {
            venues::logistic_outs(
                black_box(&slope),
                black_box(&rate),
                black_box(&knee),
                black_box(&amounts),
            )
        })
    });
}

fn product(c: &mut Criterion) {
    let state = json!({
        "shape": "constant_product",
        "reserve0": "1000000",
        "reserve1": "2000000",
        "fee_pips_used": 3000,
        "token0": "a",
        "token1": "b",
    });
    let direction = json!({
        "token_in": "a",
        "token_out": "b",
        "samples": [
            {"amount_in": "1000", "chain_out": "1992", "fraction": "0.1"},
            {"amount_in": "4000", "chain_out": "7944", "fraction": "0.5"},
        ],
    });
    c.bench_function("choose_product", |b| {
        b.iter(|| venues::select_fit(black_box(&state), black_box(&direction)).unwrap())
    });
}

fn softening(c: &mut Criterion) {
    let samples = [
        ("93", "406", "0.0001"),
        ("938", "4087", "0.001"),
        ("9385", "40744", "0.01"),
        ("46926", "198222", "0.05"),
        ("93852", "376915", "0.1"),
        ("234630", "813440", "0.25"),
        ("469261", "1332487", "0.5"),
        ("750818", "1757190", "0.8"),
        ("891596", "1918921", "0.95"),
        ("938523", "1967274", "1"),
    ];
    let state = json!({"shape": "curve", "coins": []});
    let direction = json!({
        "token_in": "a",
        "token_out": "b",
        "samples": samples.iter().map(|(amount, out, fraction)| json!({
            "amount_in": amount,
            "chain_out": out,
            "fraction": fraction,
        })).collect::<Vec<_>>(),
    });
    let mut group = c.benchmark_group("choose");
    group.sample_size(10);
    group.bench_function("softening_series", |b| {
        b.iter(|| venues::select_fit(black_box(&state), black_box(&direction)).unwrap())
    });
    group.finish();
}

criterion_group!(benches, logistic, product, softening);
criterion_main!(benches);
