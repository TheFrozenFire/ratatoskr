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
    swap_pair(pool, 1, 2)
}

fn swap_pair(pool: u8, token_in: u8, token_out: u8) -> Action {
    let contract = action::Address::new([pool; 20]).expect("contract");
    Action::Swap(
        action::Swap::new(
            contract,
            contract.pool_id(),
            action::Token([token_in; 20]),
            action::Token([token_out; 20]),
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

/// One fat conduit per leg. Leg `i` spends token `i + 1` and pays token `i + 2`.
fn fat_leg(leg: u8) -> Edge {
    let token_in = leg + 1;
    let token_out = leg + 2;
    let curve = fat_curve();
    let Curve::Chords(chords) = curve else {
        unreachable!("fat conduit");
    };
    let chords = chords
        .into_iter()
        .enumerate()
        .map(|(index, mut chord)| {
            chord.action = swap_pair(
                leg.saturating_mul(16).saturating_add(index as u8 + 1),
                token_in,
                token_out,
            );
            chord
        })
        .collect();
    Edge {
        action: swap_pair(
            leg.saturating_mul(16).saturating_add(1),
            token_in,
            token_out,
        ),
        curve: Curve::Chords(chords),
    }
}

fn route_problem(legs: u8) -> Problem {
    let edges: Vec<Edge> = (0..legs).map(fat_leg).collect();
    let width: BigInt = match &edges[0].curve {
        Curve::Chords(chords) => chords.iter().map(|chord| &chord.width).sum(),
        Curve::Unlimited => BigInt::from(0),
    };
    Problem {
        inputs: vec![Input {
            token: action::Token([1; 20]),
            amount: width,
        }],
        outputs: vec![Output {
            token: action::Token([legs + 1; 20]),
            weight: BigInt::from(1),
        }],
        routes: vec![Route {
            edges: (0..edges.len()).collect(),
        }],
        edges,
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}

fn quote_route(problem: &Problem, input: &BigInt) -> BigInt {
    let mut flow = input.clone();
    for index in &problem.routes[0].edges {
        flow = quote_curve(&problem.edges[*index].curve, &flow);
    }
    flow
}

fn fat_route(c: &mut Criterion) {
    let two = route_problem(2);
    let three = route_problem(3);
    let width = two.inputs[0].amount.clone();
    let mid = &width / 2;
    let mut quote_group = c.benchmark_group("route_quote");
    quote_group.sample_size(10);
    quote_group.warm_up_time(Duration::from_millis(200));
    quote_group.measurement_time(Duration::from_millis(400));
    quote_group.bench_function("two_mid", |b| {
        b.iter(|| quote_route(black_box(&two), black_box(&mid)))
    });
    quote_group.bench_function("three_mid", |b| {
        b.iter(|| quote_route(black_box(&three), black_box(&mid)))
    });
    quote_group.finish();

    let mut solve_group = c.benchmark_group("route_solve");
    solve_group.sample_size(10);
    solve_group.warm_up_time(Duration::from_millis(200));
    solve_group.measurement_time(Duration::from_secs(2));
    solve_group.bench_function("two_fat", |b| {
        b.iter(|| optimize(black_box(&two)).expect("plan"))
    });
    solve_group.bench_function("three_fat", |b| {
        b.iter(|| optimize(black_box(&three)).expect("plan"))
    });
    solve_group.finish();
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

fn retarget(curve: Curve, token_in: u8, token_out: u8, pool_base: u8) -> Curve {
    let Curve::Chords(chords) = curve else {
        return curve;
    };
    Curve::Chords(
        chords
            .into_iter()
            .map(|mut chord| {
                let pool = match &chord.action {
                    action::Action::Swap(swap) => pool_base.saturating_add(swap.contract.0[0]),
                    _ => pool_base,
                };
                chord.action = swap_pair(pool, token_in, token_out);
                chord
            })
            .collect(),
    )
}

fn curve_width(curve: &Curve) -> BigInt {
    match curve {
        Curve::Chords(chords) => chords.iter().map(|chord| &chord.width).sum(),
        Curve::Unlimited => BigInt::from(0),
    }
}

/// `routes` private one-for-one legs spend token 1 and pay the shared fat conduit.
fn shared_problem(routes: u8) -> Problem {
    let shared = retarget(fat_curve(), 2, 3, 0);
    let width = curve_width(&shared);
    let each = &width / BigInt::from(routes);
    let mut edges = vec![Edge {
        action: swap_pair(1, 2, 3),
        curve: shared,
    }];
    let mut route_list = Vec::new();
    for leg in 0..routes {
        let pool = 40 + leg;
        edges.push(Edge {
            action: swap_pair(pool, 1, 2),
            curve: Curve::Chords(vec![Chord {
                width: each.clone(),
                output: each.clone(),
                action: swap_pair(pool, 1, 2),
            }]),
        });
        route_list.push(Route {
            edges: vec![edges.len() - 1, 0],
        });
    }
    Problem {
        inputs: vec![Input {
            token: action::Token([1; 20]),
            amount: &each * BigInt::from(routes),
        }],
        outputs: vec![Output {
            token: action::Token([3; 20]),
            weight: BigInt::from(1),
        }],
        edges,
        routes: route_list,
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}

fn with_gas(mut problem: Problem, per_action: i64) -> Problem {
    let mut keys = Vec::new();
    for edge in &problem.edges {
        keys.push(edge.action.clone());
        if let Curve::Chords(chords) = &edge.curve {
            for chord in chords {
                keys.push(chord.action.clone());
            }
        }
    }
    keys.sort();
    keys.dedup();
    for key in keys {
        problem.gas.insert(key, BigInt::from(per_action));
    }
    problem
}

fn two_fat_shared() -> Problem {
    let left = retarget(fat_curve(), 1, 2, 0);
    let right = retarget(fat_curve(), 1, 2, 32);
    let shared = retarget(fat_curve(), 2, 3, 64);
    let width = curve_width(&left);
    Problem {
        inputs: vec![Input {
            token: action::Token([1; 20]),
            amount: width,
        }],
        outputs: vec![Output {
            token: action::Token([3; 20]),
            weight: BigInt::from(1),
        }],
        edges: vec![
            Edge {
                action: swap_pair(1, 1, 2),
                curve: left,
            },
            Edge {
                action: swap_pair(33, 1, 2),
                curve: right,
            },
            Edge {
                action: swap_pair(65, 2, 3),
                curve: shared,
            },
        ],
        routes: vec![Route { edges: vec![0, 2] }, Route { edges: vec![1, 2] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}

fn ledger(c: &mut Criterion) {
    let shared = shared_problem(8);
    let fat_shared = two_fat_shared();
    let mut group = c.benchmark_group("depletion_solve");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("eight_shared", |b| {
        b.iter(|| optimize(black_box(&shared)).expect("plan"))
    });
    group.bench_function("two_fat_shared", |b| {
        b.iter(|| optimize(black_box(&fat_shared)).expect("plan"))
    });
    group.finish();

    let open = with_gas(route_problem(2), 10);
    let mut loose = open.clone();
    let full: BigInt = loose.gas.values().sum();
    loose.gas_ceiling = Some(full);
    let mut tight = open.clone();
    tight.gas_ceiling = Some(BigInt::from(80));
    let mut gas_group = c.benchmark_group("gas_solve");
    gas_group.sample_size(10);
    gas_group.warm_up_time(Duration::from_millis(200));
    gas_group.measurement_time(Duration::from_secs(2));
    gas_group.bench_function("priced", |b| {
        b.iter(|| optimize(black_box(&open)).expect("plan"))
    });
    gas_group.bench_function("loose_ceiling", |b| {
        b.iter(|| optimize(black_box(&loose)).expect("plan"))
    });
    gas_group.bench_function("tight_ceiling", |b| {
        b.iter(|| optimize(black_box(&tight)).expect("plan"))
    });
    gas_group.finish();
}

criterion_group!(benches, quote, fat_solve, fat_route, ledger);
criterion_main!(benches);
