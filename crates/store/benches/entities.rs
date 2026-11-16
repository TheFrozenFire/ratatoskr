//! Point gets and scans a search runs on `entities.redb`.
//!
//! The file is opened read-only. `read` is the follower cache. `entities` is
//! the writer cache. A scan is one pass. A point-get batch is what one search
//! repeats: one direct hit, a second leg that is missing, three legs through
//! two intermediate tokens, at most 64 gets from the widest token, and the
//! venue rows a v4 or Balancer v2 action names.

use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use store::service::{ConduitPoint, ConduitRead, ConduitScan, EntityRead, TokenRows, VenuePoint};
use store::storage::data_dir;

const V4: u8 = 10;
const BALANCER_V2: u8 = 11;
const WIDE: usize = 64;

struct Mix {
    direct: (Address, Address),
    missing: [(Address, Address); 2],
    two_hub: [(Address, Address); 3],
    wide: Vec<(Address, Address)>,
    wide_degree: usize,
    v4: Vec<B256>,
    balancer: Vec<B256>,
    two_hub_found: bool,
    pairs: u64,
    empty: u64,
    segments: u64,
}

fn prepare(db: &EntityRead) -> Mix {
    let read = db.begin_read().expect("snapshot");
    let mut pairs = 0_u64;
    let mut empty = 0_u64;
    let mut segments = 0_u64;
    let mut direct: Option<(Address, Address)> = None;
    let mut miss_to = Address::repeat_byte(0xee);
    let mut miss_hit = false;
    let mut chase = Chase::new();
    let mut cur = Address::ZERO;
    let mut cur_n = 0_usize;
    let mut cur_pairs: Vec<(Address, Address)> = Vec::new();
    let mut wide_degree = 0_usize;
    let mut wide: Vec<(Address, Address)> = Vec::new();
    let mut on_token = false;
    let mut v4 = Vec::new();
    let mut balancer = Vec::new();

    ConduitScan::run(&read, |token_in, token_out, slopes| {
        pairs += 1;
        if slopes.is_empty() {
            empty += 1;
            return Ok(());
        }
        segments += slopes.len() as u64;
        if direct.is_none() {
            direct = Some((token_in, token_out));
        }
        if token_in == direct.map(|pair| pair.1).unwrap_or(Address::ZERO) && token_out == miss_to {
            miss_hit = true;
        }
        chase.see(token_in, token_out);
        if !on_token || token_in != cur {
            if on_token && cur_n > wide_degree {
                wide_degree = cur_n;
                wide.clone_from(&cur_pairs);
            }
            cur = token_in;
            cur_n = 0;
            cur_pairs.clear();
            on_token = true;
        }
        cur_n += 1;
        if cur_pairs.len() < WIDE {
            cur_pairs.push((token_in, token_out));
        }
        for slope in slopes {
            match slope.action.kind {
                V4 if v4.len() < 8 && !v4.contains(&B256::from(slope.action.pool)) => {
                    v4.push(B256::from(slope.action.pool));
                }
                BALANCER_V2
                    if balancer.len() < 8 && !balancer.contains(&B256::from(slope.action.pool)) =>
                {
                    balancer.push(B256::from(slope.action.pool));
                }
                _ => {}
            }
        }
        Ok(())
    })
    .expect("conduit scan");
    if on_token && cur_n > wide_degree {
        wide_degree = cur_n;
        wide.clone_from(&cur_pairs);
    }

    let direct = direct.expect("a conduit with a slope");
    if miss_hit {
        miss_to = Address::repeat_byte(0xef);
    }
    let missing = [direct, (direct.1, miss_to)];
    let two_hub_found = chase.legs().is_some();
    let two_hub = chase.legs().unwrap_or([direct, direct, direct]);
    Mix {
        direct,
        missing,
        two_hub,
        two_hub_found,
        wide,
        wide_degree,
        v4,
        balancer,
        pairs,
        empty,
        segments,
    }
}

/// Three hitting legs whose intermediate tokens sort after the previous one.
struct Chase {
    a: Option<Address>,
    h1: Option<Address>,
    h2: Option<Address>,
    legs: Option<[(Address, Address); 3]>,
}

impl Chase {
    fn new() -> Self {
        Self {
            a: None,
            h1: None,
            h2: None,
            legs: None,
        }
    }

    fn see(&mut self, token_in: Address, token_out: Address) {
        if self.legs.is_some() {
            return;
        }
        if self.h2.is_none() && self.h1.is_some_and(|h1| token_in > h1) {
            self.a = None;
            self.h1 = None;
        }
        if self.h2.is_some_and(|h2| token_in > h2) {
            self.a = None;
            self.h1 = None;
            self.h2 = None;
        }
        if self.a.is_none() && token_out > token_in {
            self.a = Some(token_in);
            self.h1 = Some(token_out);
            return;
        }
        if self.h2.is_none() && self.h1 == Some(token_in) && token_out > token_in {
            self.h2 = Some(token_out);
            return;
        }
        if self.h2 == Some(token_in) && Some(token_out) != self.h1 && Some(token_out) != self.a {
            let a = self.a.expect("first token");
            let h1 = self.h1.expect("first hub");
            let h2 = self.h2.expect("second hub");
            self.legs = Some([(a, h1), (h1, h2), (h2, token_out)]);
        }
    }

    fn legs(&self) -> Option<[(Address, Address); 3]> {
        self.legs
    }
}

fn token_scan(db: &EntityRead) -> usize {
    let read = db.begin_read().expect("snapshot");
    TokenRows::run(&read).expect("tokens").len()
}

fn conduit_scan(db: &EntityRead) -> u64 {
    let read = db.begin_read().expect("snapshot");
    let mut segments = 0_u64;
    ConduitScan::run(&read, |_token_in, _token_out, slopes| {
        segments += slopes.len() as u64;
        Ok(())
    })
    .expect("conduits");
    segments
}

fn conduits_per_call(db: &EntityRead, pairs: &[(Address, Address)]) -> usize {
    let mut segments = 0;
    for (token_in, token_out) in pairs {
        if let Some(row) = db.read_conduit(*token_in, *token_out).expect("conduit") {
            segments += row.segments.len();
        }
    }
    segments
}

fn conduits_shared(db: &EntityRead, pairs: &[(Address, Address)]) -> usize {
    let read = db.begin_read().expect("snapshot");
    let mut segments = 0;
    for (token_in, token_out) in pairs {
        if let Some(row) = (ConduitPoint {
            token_in: *token_in,
            token_out: *token_out,
        })
        .run(&read)
        .expect("conduit")
        {
            segments += row.segments.len();
        }
    }
    segments
}

fn conduits_held(db: &EntityRead, pairs: &[(Address, Address)]) -> usize {
    let read = db.begin_read().expect("snapshot");
    let table = ConduitRead::open(&read).expect("table").expect("conduits");
    let mut segments = 0;
    for (token_in, token_out) in pairs {
        if let Some(row) = table.get(*token_in, *token_out).expect("conduit") {
            segments += row.segments.len();
        }
    }
    segments
}

fn venues_per_call(db: &EntityRead, kind: u8, ids: &[B256]) -> usize {
    let mut found = 0;
    for id in ids {
        if db.read_venue(kind, *id).expect("venue").is_some() {
            found += 1;
        }
    }
    found
}

fn venues_shared(db: &EntityRead, kind: u8, ids: &[B256]) -> usize {
    let read = db.begin_read().expect("snapshot");
    let mut found = 0;
    for id in ids {
        if (VenuePoint { kind, id: *id })
            .run(&read)
            .expect("venue")
            .is_some()
        {
            found += 1;
        }
    }
    found
}

fn entities(c: &mut Criterion) {
    let path = data_dir();
    let caches = config::Store::default();
    for (name, bytes) in [("read", caches.read), ("entities", caches.entities)] {
        let db = EntityRead::open_with(&path, bytes).expect("open entities.redb read-only");
        let started = Instant::now();
        let mix = prepare(&db);
        eprintln!(
            "{name} cache {} MiB, prepare {} ms, pairs {} (empty {}), segments {}, wide degree {}, wide gets {}, v4 {}, balancer v2 {}, two-hub path {}",
            bytes / (1024 * 1024),
            started.elapsed().as_millis(),
            mix.pairs,
            mix.empty,
            mix.segments,
            mix.wide_degree,
            mix.wide.len(),
            mix.v4.len(),
            mix.balancer.len(),
            mix.two_hub_found,
        );
        let mut group = c.benchmark_group(format!("entities_{name}"));
        group.sample_size(10);
        group.warm_up_time(Duration::from_millis(200));
        group.measurement_time(Duration::from_secs(2));
        group.bench_function("token_scan", |b| b.iter(|| black_box(token_scan(&db))));
        group.bench_function("conduit_scan", |b| b.iter(|| black_box(conduit_scan(&db))));
        let direct = [mix.direct];
        group.bench_function("direct_per_call", |b| {
            b.iter(|| black_box(conduits_per_call(&db, &direct)))
        });
        group.bench_function("direct_shared", |b| {
            b.iter(|| black_box(conduits_shared(&db, &direct)))
        });
        group.bench_function("missing_leg_per_call", |b| {
            b.iter(|| black_box(conduits_per_call(&db, &mix.missing)))
        });
        group.bench_function("missing_leg_shared", |b| {
            b.iter(|| black_box(conduits_shared(&db, &mix.missing)))
        });
        group.bench_function("two_hub_per_call", |b| {
            b.iter(|| black_box(conduits_per_call(&db, &mix.two_hub)))
        });
        group.bench_function("two_hub_shared", |b| {
            b.iter(|| black_box(conduits_shared(&db, &mix.two_hub)))
        });
        group.bench_function("wide_per_call", |b| {
            b.iter(|| black_box(conduits_per_call(&db, &mix.wide)))
        });
        group.bench_function("wide_shared", |b| {
            b.iter(|| black_box(conduits_shared(&db, &mix.wide)))
        });
        group.bench_function("wide_held", |b| {
            b.iter(|| black_box(conduits_held(&db, &mix.wide)))
        });
        if !mix.v4.is_empty() {
            group.bench_function("venue_v4_per_call", |b| {
                b.iter(|| black_box(venues_per_call(&db, V4, &mix.v4)))
            });
            group.bench_function("venue_v4_shared", |b| {
                b.iter(|| black_box(venues_shared(&db, V4, &mix.v4)))
            });
        }
        if !mix.balancer.is_empty() {
            group.bench_function("venue_balancer_v2_per_call", |b| {
                b.iter(|| black_box(venues_per_call(&db, BALANCER_V2, &mix.balancer)))
            });
            group.bench_function("venue_balancer_v2_shared", |b| {
                b.iter(|| black_box(venues_shared(&db, BALANCER_V2, &mix.balancer)))
            });
        }
        group.finish();
    }
}

fn criterion() -> Criterion {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/criterion");
    Criterion::default().output_directory(&dir)
}

criterion_group! {
    name = benches;
    config = criterion();
    targets = entities
}
criterion_main!(benches);
