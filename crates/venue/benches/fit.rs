use std::time::Duration;

use alloy_primitives::U256;
use chain::detail::{shapes, token0, token1};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use num_bigint::{BigInt, Sign};
use venue::{curve_for_snapshot, direction_knots, domain_max, grid_amounts, CurveRecord, GRID};

fn configure(group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>, heavy: bool) {
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(if heavy {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(400)
    });
}

fn ceiling(c: &mut Criterion) {
    let mut group = c.benchmark_group("domain");
    configure(&mut group, true);
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let detail = shape.detail;
        group.bench_with_input(
            BenchmarkId::from_parameter(shape.name),
            &detail,
            |b, detail| {
                b.iter(|| domain_max(black_box(detail), black_box(token_in), black_box(token_out)))
            },
        );
    }
    group.finish();
}

fn grid(c: &mut Criterion) {
    let mut group = c.benchmark_group("grid");
    configure(&mut group, true);
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let detail = shape.detail;
        let (max_in, _) = domain_max(&detail, token_in, token_out);
        let amounts: Vec<U256> = grid_amounts(max_in)
            .into_iter()
            .map(|(_, amount)| amount)
            .collect();
        assert_eq!(amounts.len(), GRID.len(), "{}", shape.name);
        group.bench_with_input(
            BenchmarkId::from_parameter(shape.name),
            &(&detail, &amounts),
            |b, (detail, amounts)| {
                b.iter(|| {
                    for amount in amounts.iter() {
                        black_box(
                            detail
                                .quote(token_in, token_out, black_box(*amount))
                                .unwrap(),
                        );
                    }
                })
            },
        );
    }
    group.finish();
}

fn closed_form(c: &mut Criterion) {
    let mut group = c.benchmark_group("estimate");
    configure(&mut group, false);
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let curve = curve_for_snapshot(&shape.detail, 1).unwrap();
        let probe = shape.probe;
        group.bench_with_input(
            BenchmarkId::from_parameter(shape.name),
            &curve,
            |b, curve| {
                b.iter(|| {
                    venue::estimate(
                        black_box(curve),
                        1,
                        black_box(token_in),
                        black_box(token_out),
                        black_box(probe),
                    )
                    .unwrap()
                })
            },
        );
    }
    group.finish();
}

fn knots(c: &mut Criterion) {
    let mut group = c.benchmark_group("knots");
    configure(&mut group, true);
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let detail = shape.detail;
        let curve = curve_for_snapshot(&detail, 1).unwrap();
        group.bench_with_input(
            BenchmarkId::from_parameter(shape.name),
            &(&detail, &curve),
            |b, (detail, curve)| {
                b.iter(|| {
                    direction_knots(
                        black_box(detail),
                        black_box(curve),
                        black_box(token_in),
                        black_box(token_out),
                    )
                })
            },
        );
    }
    group.finish();
}

fn through_slopes(c: &mut Criterion) {
    let mut group = c.benchmark_group("through_slopes");
    configure(&mut group, true);
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let detail = shape.detail.clone();
        let probe = bigint(shape.probe);
        group.bench_function(shape.name, |b| {
            b.iter(|| {
                let curve: CurveRecord = curve_for_snapshot(black_box(&detail), 1).unwrap();
                let knots = direction_knots(&detail, &curve, token_in, token_out);
                let segments = routing::segments_from_knots(
                    &knots,
                    routing::Action {
                        kind: curve.kind,
                        pool: curve.id.0,
                    },
                );
                routing::quote(&segments, black_box(&probe))
            })
        });
    }
    group.finish();
}

fn bigint(value: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>())
}

/// Knots, secants, and the steepest-first merge for one ordered pair.
fn conduit_slopes(c: &mut Criterion) {
    let mut group = c.benchmark_group("conduit");
    configure(&mut group, true);
    let token_in = token0();
    let token_out = token1();
    let shape = shapes()
        .into_iter()
        .find(|shape| shape.name == "product")
        .expect("product");
    let curve = curve_for_snapshot(&shape.detail, 1).unwrap();
    group.bench_function("fat_product", |b| {
        b.iter(|| {
            let mut parts = Vec::new();
            for pool in 1..=16u8 {
                let knots = direction_knots(
                    black_box(&shape.detail),
                    black_box(&curve),
                    token_in,
                    token_out,
                );
                let mut id = [0u8; 32];
                id[31] = pool;
                let mut segments = routing::segments_from_knots(
                    &knots,
                    routing::Action {
                        kind: curve.kind,
                        pool: id,
                    },
                );
                for segment in &mut segments {
                    segment.output *= BigInt::from(pool as i64);
                }
                parts.push(segments);
            }
            routing::merge(&parts)
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    ceiling,
    grid,
    closed_form,
    knots,
    through_slopes,
    conduit_slopes
);
criterion_main!(benches);
