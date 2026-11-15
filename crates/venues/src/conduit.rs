//! Pair-conduit jobs. The file service lives in [`store::service`].

pub use store_model::{ConduitFail, ConduitJob};

#[cfg(test)]
mod tests {
    use crate::catalog::UNI_V2;
    use crate::detail::{ConstantProduct, PoolDetail};
    use crate::fit::direction_knots;
    use crate::install_catalog_hooks;
    use crate::state::CurveRecord;
    use crate::state::{derive_curve, estimate, split_detail, ImmutableRow, CURVE_VERSION};
    use crate::Catalog;
    use alloy_primitives::{address, U256};
    use alloy_primitives::{Address, B256};
    use num_bigint::{BigInt, Sign};
    use routing::quote;
    use routing::{segments_from_knots, Action, SlopeSegment};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-conduit-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn product(byte: u8, reserve0: u64, reserve1: u64) -> PoolDetail {
        let mut id = B256::ZERO;
        id[31] = byte;
        PoolDetail::ConstantProduct(ConstantProduct {
            kind: UNI_V2,
            id,
            pair: address!("1111111111111111111111111111111111111111"),
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reserve0: U256::from(reserve0),
            reserve1: U256::from(reserve1),
            fee_pips: Some(3000),
            timestamp: 4,
            flags: 0,
        })
    }

    fn write_pool(catalog: &Catalog, detail: &PoolDetail, block: u64) -> CurveRecord {
        let PoolDetail::ConstantProduct(pool) = detail else {
            unreachable!();
        };
        let (body, mut write) = split_detail(detail).unwrap();
        write.block = block;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: pool.kind,
                id: pool.id,
                body,
            }])
            .unwrap();
        let stored = catalog.put_mutable(&write).unwrap();
        let curve = derive_curve(detail, stored.generation).unwrap();
        assert_eq!(curve.version, CURVE_VERSION);
        catalog.put_curve(&curve).unwrap();
        curve
    }

    #[test]
    fn a_pair_conduit_merges_two_constant_product_pools() {
        let dir = TempDir::new();
        install_catalog_hooks();
        let catalog = Catalog::open(&dir.0).unwrap();
        let deep = product(1, 1_000_000, 1_000_000);
        let thin = product(2, 2_000_000, 500_000);
        let deep_curve = write_pool(&catalog, &deep, 10);
        let thin_curve = write_pool(&catalog, &thin, 12);
        let token_in = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token_out = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        assert_eq!(catalog.state_summary().unwrap().jobs, 2);
        assert_eq!(catalog.enqueue_conduit_fill().unwrap(), 2);
        assert_eq!(catalog.state_summary().unwrap().jobs, 2);
        let first = catalog.claim_conduits(8, 1, 30).unwrap();
        assert_eq!(first.len(), 2);
        assert!(catalog.claim_conduits(8, 1, 30).unwrap().is_empty());

        let mut again = Vec::new();
        for job in &first {
            if catalog.build_conduit(job.token_in, job.token_out).unwrap() {
                again.push((job.token_in, job.token_out));
            }
        }
        let done: Vec<_> = first
            .iter()
            .map(|job| (job.token_in, job.token_out))
            .collect();
        catalog.finish_conduits(&done, &again, &[]).unwrap();
        assert!(again.is_empty());
        assert_eq!(catalog.enqueue_conduit_fill().unwrap(), 0);
        assert!(catalog.claim_conduits(8, 1, 30).unwrap().is_empty());

        let stored = catalog.read_conduit(token_in, token_out).unwrap().unwrap();
        assert_eq!(stored.block, 10);
        assert_eq!(stored.generation, 1);
        assert_eq!(stored.members, 2);
        let mut pools: Vec<[u8; 32]> = stored
            .segments
            .iter()
            .map(|segment| segment.action.pool)
            .collect();
        pools.sort();
        pools.dedup();
        assert_eq!(pools.len(), 2);

        let merged_at = |input: &BigInt| quote(&stored.segments, input);
        for (detail, curve) in [(&deep, &deep_curve), (&thin, &thin_curve)] {
            let knots = direction_knots(detail, curve, token_in, token_out);
            let (sample_in, sample_out) = &knots[1];
            assert!(merged_at(sample_in) >= *sample_out);
            let amount = u256_from(&sample_in);
            let alone = estimate(curve, curve.generation, token_in, token_out, amount).unwrap();
            let alone_amount =
                BigInt::from_bytes_be(Sign::Plus, &alone.amount_out.to_be_bytes::<32>());
            assert_eq!(alone_amount, *sample_out);
        }
        let expected = expected_merge(&deep, &deep_curve, &thin, &thin_curve, token_in, token_out);
        let probe = BigInt::from(1_000);
        assert_eq!(merged_at(&probe), quote(&expected, &probe));

        let PoolDetail::ConstantProduct(pool) = &thin else {
            unreachable!();
        };
        let (body, mut write) = split_detail(&thin).unwrap();
        write.block = 12;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: pool.kind,
                id: pool.id,
                body,
            }])
            .unwrap();
        catalog.put_mutable(&write).unwrap();
        let jobs = catalog.claim_conduits(8, 1, 30).unwrap();
        assert!(!jobs.is_empty());
        let mut again = Vec::new();
        for job in &jobs {
            if catalog.build_conduit(job.token_in, job.token_out).unwrap() {
                again.push((job.token_in, job.token_out));
            }
        }
        let done: Vec<_> = jobs
            .iter()
            .map(|job| (job.token_in, job.token_out))
            .collect();
        catalog.finish_conduits(&done, &again, &[]).unwrap();
        let stored = catalog.read_conduit(token_in, token_out).unwrap().unwrap();
        assert_eq!(stored.members, 1);
        assert_eq!(stored.block, 10);
        assert!(stored
            .segments
            .iter()
            .all(|segment| segment.action.pool == deep_curve.id.0));
    }

    fn u256_from(value: &BigInt) -> U256 {
        let (_, bytes) = value.to_bytes_be();
        U256::from_be_slice(&bytes)
    }

    fn expected_merge(
        deep: &PoolDetail,
        deep_curve: &CurveRecord,
        thin: &PoolDetail,
        thin_curve: &CurveRecord,
        token_in: Address,
        token_out: Address,
    ) -> Vec<SlopeSegment> {
        let one = segments_from_knots(
            &direction_knots(deep, deep_curve, token_in, token_out),
            Action {
                kind: deep_curve.kind,
                pool: deep_curve.id.0,
            },
        );
        let two = segments_from_knots(
            &direction_knots(thin, thin_curve, token_in, token_out),
            Action {
                kind: thin_curve.kind,
                pool: thin_curve.id.0,
            },
        );
        routing::merge(&[one, two])
    }
}
