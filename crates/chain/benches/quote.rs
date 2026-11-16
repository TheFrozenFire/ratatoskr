use std::time::Duration;

use chain::detail::{shapes, token0, token1};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn quote(c: &mut Criterion) {
    let mut group = c.benchmark_group("quote");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_millis(500));
    let token_in = token0();
    let token_out = token1();
    for shape in shapes() {
        let detail = shape.detail;
        let probe = shape.probe;
        group.bench_function(shape.name, |b| {
            b.iter(|| {
                detail
                    .quote(black_box(token_in), black_box(token_out), black_box(probe))
                    .unwrap()
            })
        });
    }
    group.finish();
}

criterion_group!(benches, quote);
criterion_main!(benches);
