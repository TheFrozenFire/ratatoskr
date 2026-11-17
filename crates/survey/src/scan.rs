use std::collections::HashMap;

use searcher::{Token, NATIVE};

use crate::graph::Graph;
use crate::kinds::{kind_bit, only_kind};
use crate::progress;
use crate::source::Source;
use crate::token::token_of;

#[derive(Clone)]
pub(crate) struct Example {
    pub(crate) from: Token,
    pub(crate) to: Token,
    pub(crate) kinds: u32,
    pub(crate) capacity: u128,
    pub(crate) segments: u32,
}

pub(crate) struct Bag {
    pub(crate) high: Vec<Example>,
    pub(crate) low: Vec<Example>,
}

pub(crate) struct Survey {
    pub(crate) graph: Graph,
    pub(crate) conduits: u64,
    pub(crate) pure: HashMap<u8, Bag>,
    pub(crate) carrier: HashMap<u8, Example>,
    pub(crate) by_width: HashMap<u32, Example>,
}

pub(crate) fn survey(source: &impl Source) -> eyre::Result<Survey> {
    let mut graph = Graph::default();
    let mut conduits = 0_u64;
    let mut pure: HashMap<u8, Bag> = HashMap::new();
    let mut carrier: HashMap<u8, Example> = HashMap::new();
    let mut by_width: HashMap<u32, Example> = HashMap::new();
    source.visit_meta(&mut |token_in, token_out, meta| {
        if meta.segments == 0 {
            return Ok(());
        }
        conduits += 1;
        if conduits.is_multiple_of(100_000) {
            progress!("scan {conduits}");
        }
        let from = token_of(token_in);
        let to = token_of(token_out);
        graph.push(from, to, meta.kinds, meta.capacity);
        if from == NATIVE || to == NATIVE || meta.kinds == 0 {
            return Ok(());
        }
        let example = Example {
            from,
            to,
            kinds: meta.kinds,
            capacity: meta.capacity,
            segments: meta.segments,
        };
        keep_width(&mut by_width, &example);
        for kind in 1..32_u8 {
            if meta.kinds & kind_bit(kind) != 0 {
                keep_carrier(&mut carrier, kind, &example);
            }
        }
        if let Some(kind) = only_kind(meta.kinds) {
            let bag = pure.entry(kind).or_insert_with(|| Bag {
                high: Vec::new(),
                low: Vec::new(),
            });
            insert_high(&mut bag.high, example.clone());
            insert_low(&mut bag.low, example);
        }
        Ok(())
    })?;
    Ok(Survey {
        graph,
        conduits,
        pure,
        carrier,
        by_width,
    })
}

pub(crate) fn keep_width(map: &mut HashMap<u32, Example>, example: &Example) {
    let width = example.kinds.count_ones();
    if width == 0 {
        return;
    }
    let replace = map
        .get(&width)
        .map(|kept| example.capacity > kept.capacity)
        .unwrap_or(true);
    if replace {
        map.insert(width, example.clone());
    }
}

pub(crate) fn keep_carrier(map: &mut HashMap<u8, Example>, kind: u8, example: &Example) {
    let replace = match map.get(&kind) {
        None => true,
        Some(kept) => {
            let fewer = example.kinds.count_ones() < kept.kinds.count_ones();
            let richer = example.kinds.count_ones() == kept.kinds.count_ones()
                && example.capacity > kept.capacity;
            fewer || richer
        }
    };
    if replace {
        map.insert(kind, example.clone());
    }
}

pub(crate) fn insert_high(bag: &mut Vec<Example>, example: Example) {
    bag.push(example);
    bag.sort_by(|left, right| {
        right
            .capacity
            .cmp(&left.capacity)
            .then(left.from.cmp(&right.from))
            .then(left.to.cmp(&right.to))
    });
    bag.truncate(12);
}

pub(crate) fn insert_low(bag: &mut Vec<Example>, example: Example) {
    if example.capacity == 0 {
        return;
    }
    bag.push(example);
    bag.sort_by(|left, right| {
        left.capacity
            .cmp(&right.capacity)
            .then(left.from.cmp(&right.from))
            .then(left.to.cmp(&right.to))
    });
    bag.truncate(12);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256};
    use routing::SlopeSegment;
    use store::service::ConduitMeta;
    use venue::{TokenRecord, VenueRow};

    use crate::token::token_of;

    struct MetaOnly {
        rows: Vec<(Address, Address, ConduitMeta)>,
    }

    impl Source for MetaOnly {
        fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
            Ok(Vec::new())
        }

        fn visit_meta(
            &self,
            visit: &mut dyn FnMut(Address, Address, ConduitMeta) -> eyre::Result<()>,
        ) -> eyre::Result<()> {
            for (token_in, token_out, meta) in &self.rows {
                visit(*token_in, *token_out, *meta)?;
            }
            Ok(())
        }

        fn conduit_segments(
            &self,
            _token_in: Address,
            _token_out: Address,
        ) -> eyre::Result<Option<Vec<SlopeSegment>>> {
            Ok(None)
        }

        fn read_venue(&self, _kind: u8, _id: B256) -> eyre::Result<Option<VenueRow>> {
            Ok(None)
        }
    }

    #[test]
    fn the_scan_reads_the_stored_mask_and_width_sum() {
        let from = address!("1111111111111111111111111111111111111111");
        let to = address!("2222222222222222222222222222222222222222");
        let surveyed = survey(&MetaOnly {
            rows: vec![(
                from,
                to,
                ConduitMeta {
                    kinds: 1 << 9,
                    capacity: 12,
                    segments: 3,
                },
            )],
        })
        .unwrap();
        let edge = surveyed
            .graph
            .meta(token_of(from), token_of(to))
            .expect("edge");
        assert_eq!(edge.kinds, 1 << 9);
        assert_eq!(edge.capacity, 12);
        assert_eq!(surveyed.conduits, 1);
        let kept = surveyed.by_width.get(&1).expect("pure");
        assert_eq!(kept.capacity, 12);
        assert_eq!(kept.segments, 3);
    }
}
