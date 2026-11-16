//! Route set for one trade.
//!
//! A trade names the tokens it spends, the amount of each, and the tokens it
//! wants back. [`prepare`] checks that trade and records the hubs, the wrapped
//! token, and the bounds of the endpoint walk. [`execute`] reads a pair conduit
//! for each ordered pair the routes need and returns those curves. The routes
//! are the direct pair, a path through one hub, a path through two hubs, and a
//! bounded walk from an endpoint that names few conduits. That walk keeps a
//! hub-free path, and it joins a tendril into a hub with a tendril leaving a hub.
//! A wider token is not expanded. The walk asks whether that token has one
//! conduit to the other endpoint, and whether it has one conduit to a hub.
//! Native ETH is the zero address. The wrap pays one wrapped token per native
//! token at any size.
//!
//! The caller supplies the conduits. [`Graph`] records those conduits so a
//! caller can rank hubs and list neighbors. This crate does not open the
//! catalog and does not allocate the flow. A curve is kept only when a route
//! uses it.

mod graph;
mod query;
mod route;
mod walk;

pub use action::{Token, NATIVE};
pub use graph::{hub_census, incoming, Graph, HubCensus, HubScore, Link, Meta};
pub use query::{prepare, Query, SearchError, Trade, NEIGHBOR_LIMIT, WALK_DEPTH};
pub use route::{execute, Conduits, Curve, Edge, Route, RouteSet};

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::{BigInt, BigUint};
    use routing::{Action, SlopeSegment};
    use std::collections::HashMap;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn amount(value: u64) -> BigUint {
        BigUint::from(value)
    }

    fn segment(width: i64, output: i64, pool: u8) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: Action {
                kind: 1,
                pool: [pool; 32],
            },
        }
    }

    struct Catalog(HashMap<(Token, Token), Vec<SlopeSegment>>);

    impl Conduits for Catalog {
        fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>> {
            self.0.get(&(token_in, token_out)).cloned()
        }

        fn outgoing(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
            listed(&self.0, token, limit, true)
        }

        fn incoming(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
            listed(&self.0, token, limit, false)
        }
    }

    fn listed(
        edges: &HashMap<(Token, Token), Vec<SlopeSegment>>,
        token: Token,
        limit: usize,
        forward: bool,
    ) -> Option<Vec<Token>> {
        let mut tokens: Vec<Token> = edges
            .iter()
            .filter(|((src, dst), segments)| {
                !segments.is_empty()
                    && if forward {
                        *src == token
                    } else {
                        *dst == token
                    }
            })
            .map(|((src, dst), _)| if forward { *dst } else { *src })
            .collect();
        tokens.sort();
        tokens.dedup();
        if tokens.len() > limit {
            None
        } else {
            Some(tokens)
        }
    }

    fn route_tokens(set: &RouteSet, route: &Route) -> Vec<Token> {
        let mut tokens = vec![set.edges[route.edges[0]].token_in];
        for index in &route.edges {
            tokens.push(set.edges[*index].token_out);
        }
        tokens
    }

    fn trade(inputs: &[(u8, u64)], outputs: &[u8]) -> Trade {
        Trade {
            inputs: inputs
                .iter()
                .map(|(byte, value)| (token(*byte), amount(*value)))
                .collect(),
            outputs: outputs.iter().copied().map(token).collect(),
        }
    }

    #[test]
    fn direct_conduit_is_one_route() {
        let query = prepare(trade(&[(1, 50)], &[2]), &[], None).unwrap();
        let curve = vec![segment(10, 9, 7)];
        let catalog = Catalog(HashMap::from([((token(1), token(2)), curve.clone())]));
        let set = execute(&query, &catalog);
        assert_eq!(set.inputs, vec![(token(1), amount(50))]);
        assert_eq!(set.outputs, vec![token(2)]);
        assert_eq!(set.routes, vec![Route { edges: vec![0] }]);
        assert_eq!(set.edges[0].curve, Curve::Conduit(curve));
    }

    #[test]
    fn one_hub_joins_two_conduits() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), hub), vec![segment(4, 4, 1)]),
            ((hub, token(2)), vec![segment(4, 3, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges[0].token_out, hub);
        assert_eq!(set.edges[1].token_in, hub);
    }

    #[test]
    fn two_hubs_insert_the_conduit_between_them() {
        let left = token(8);
        let right = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[left, right], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), left), vec![segment(1, 1, 1)]),
            ((left, right), vec![segment(1, 1, 2)]),
            ((right, token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(
            set.routes,
            vec![Route {
                edges: vec![0, 1, 2]
            }]
        );
    }

    #[test]
    fn a_missing_leg_drops_the_hub_route() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([((token(1), hub), vec![segment(1, 1, 1)])]));
        let set = execute(&query, &catalog);
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn an_empty_conduit_is_absent() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([((token(1), token(2)), vec![])]));
        let set = execute(&query, &catalog);
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn shared_leg_is_one_edge() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2, 3]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), hub), vec![segment(5, 5, 1)]),
            ((hub, token(2)), vec![segment(5, 4, 2)]),
            ((hub, token(3)), vec![segment(5, 3, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 2);
        assert_eq!(set.routes[0].edges[0], set.routes[1].edges[0]);
        assert_eq!(set.edges.len(), 3);
    }

    #[test]
    fn native_input_wraps_then_uses_the_wrapped_conduits() {
        let wrapped = token(1);
        let out = token(2);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(10))],
                outputs: vec![out],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let catalog = Catalog(HashMap::from([((wrapped, out), vec![segment(8, 7, 4)])]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(set.edges[0].token_in, NATIVE);
        assert_eq!(set.edges[0].token_out, wrapped);
        assert!(matches!(set.edges[1].curve, Curve::Conduit(_)));
    }

    #[test]
    fn wrapped_input_unwraps_to_native() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(wrapped, amount(4))],
                outputs: vec![NATIVE],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let set = execute(&query, &Catalog(HashMap::new()));
        assert_eq!(set.routes, vec![Route { edges: vec![0] }]);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(set.edges[0].token_out, NATIVE);
    }

    #[test]
    fn a_leg_that_no_route_uses_is_dropped() {
        let dropped = token(8);
        let kept = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[dropped, kept], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), dropped), vec![segment(1, 1, 1)]),
            ((token(1), kept), vec![segment(1, 1, 2)]),
            ((kept, token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges.len(), 2);
        assert!(set
            .edges
            .iter()
            .all(|edge| edge.token_in != dropped && edge.token_out != dropped));
    }

    #[test]
    fn native_with_no_onward_conduit_keeps_no_edge() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(1))],
                outputs: vec![token(2)],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let set = execute(&query, &Catalog(HashMap::new()));
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn prepare_rejects_a_token_on_both_sides() {
        let err = prepare(trade(&[(1, 10)], &[1]), &[], None).unwrap_err();
        assert_eq!(err, SearchError::Overlap);
    }

    #[test]
    fn prepare_rejects_a_zero_amount() {
        let err = prepare(trade(&[(1, 0)], &[2]), &[], None).unwrap_err();
        assert_eq!(err, SearchError::ZeroAmount);
    }

    #[test]
    fn a_common_neighbor_is_a_route_without_a_hub() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(5)), vec![segment(1, 1, 1)]),
            ((token(5), token(2)), vec![segment(1, 1, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(5), token(2)]
        );
    }

    #[test]
    fn a_chain_of_small_tokens_is_one_route() {
        let query = prepare(trade(&[(1, 10)], &[4]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), token(3)), vec![segment(1, 1, 2)]),
            ((token(3), token(4)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), token(3), token(4)]
        );
    }

    #[test]
    fn a_tendril_into_a_hub_joins_a_tendril_out() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[4]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), hub), vec![segment(1, 1, 2)]),
            ((hub, token(3)), vec![segment(1, 1, 3)]),
            ((token(3), token(4)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), hub, token(3), token(4)]
        );
    }

    #[test]
    fn tendrils_around_two_hubs_include_the_hub_conduit() {
        let left = token(8);
        let right = token(9);
        let query = prepare(trade(&[(1, 10)], &[4]), &[left, right], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), left), vec![segment(1, 1, 2)]),
            ((left, right), vec![segment(1, 1, 3)]),
            ((right, token(3)), vec![segment(1, 1, 4)]),
            ((token(3), token(4)), vec![segment(1, 1, 5)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), left, right, token(3), token(4)]
        );
    }

    #[test]
    fn a_fat_bridge_is_probed_without_expanding_it() {
        let mut query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        query.neighbor_limit = 1;
        let fat = token(7);
        let catalog = Catalog(HashMap::from([
            ((token(1), fat), vec![segment(1, 1, 1)]),
            ((token(6), fat), vec![segment(1, 1, 2)]),
            ((fat, token(2)), vec![segment(1, 1, 3)]),
            ((fat, token(3)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), fat, token(2)]
        );
        assert!(set.edges.iter().all(|edge| {
            edge.token_in != token(6)
                && edge.token_out != token(6)
                && edge.token_in != token(3)
                && edge.token_out != token(3)
        }));
    }

    #[test]
    fn two_local_bridges_are_both_routes() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(5)), vec![segment(1, 1, 1)]),
            ((token(5), token(2)), vec![segment(1, 1, 2)]),
            ((token(1), token(6)), vec![segment(1, 1, 3)]),
            ((token(6), token(2)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        let mut found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                vec![token(1), token(5), token(2)],
                vec![token(1), token(6), token(2)]
            ]
        );
    }

    #[test]
    fn the_direct_pair_stays_beside_a_local_bridge() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(1), token(5)), vec![segment(1, 1, 2)]),
            ((token(5), token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 2);
        assert_eq!(route_tokens(&set, &set.routes[0]), vec![token(1), token(2)]);
    }

    #[test]
    fn a_wide_hub_joins_a_tendril_without_listing_its_conduits() {
        let hub = token(9);
        let mut edges = HashMap::new();
        edges.insert((token(1), token(2)), vec![segment(1, 1, 1)]);
        edges.insert((token(2), hub), vec![segment(1, 1, 2)]);
        edges.insert((hub, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..80_u8 {
            edges.insert((hub, token(byte)), vec![segment(1, 1, byte)]);
        }
        let query = prepare(trade(&[(1, 10)], &[4]), &[hub], None).unwrap();
        let set = execute(&query, &Catalog(edges));
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), hub, token(4)]
        );
    }

    #[test]
    fn a_fat_neighbor_reaches_a_hub_one_conduit_past_it() {
        let fat = token(7);
        let hub = token(9);
        let mut query = prepare(trade(&[(1, 1)], &[4]), &[hub], None).unwrap();
        query.neighbor_limit = 1;
        let mut edges = HashMap::new();
        edges.insert((token(1), fat), vec![segment(1, 1, 1)]);
        edges.insert((fat, hub), vec![segment(1, 1, 2)]);
        edges.insert((hub, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..20_u8 {
            edges.insert((fat, token(byte)), vec![segment(1, 1, byte)]);
        }
        let set = execute(&query, &Catalog(edges));
        assert!(set
            .routes
            .iter()
            .any(|route| { route_tokens(&set, route) == vec![token(1), fat, hub, token(4)] }));
    }

    #[test]
    fn a_fat_predecessor_reaches_a_hub_one_conduit_before_it() {
        let fat = token(7);
        let hub = token(9);
        let mut query = prepare(trade(&[(1, 1)], &[4]), &[hub], None).unwrap();
        query.neighbor_limit = 1;
        let mut edges = HashMap::new();
        edges.insert((token(1), hub), vec![segment(1, 1, 1)]);
        edges.insert((hub, fat), vec![segment(1, 1, 2)]);
        edges.insert((fat, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..20_u8 {
            edges.insert((token(byte), fat), vec![segment(1, 1, byte)]);
        }
        let set = execute(&query, &Catalog(edges));
        assert!(set
            .routes
            .iter()
            .any(|route| { route_tokens(&set, route) == vec![token(1), hub, fat, token(4)] }));
    }

    #[test]
    fn a_native_hub_on_a_short_tendril_is_joined() {
        let mid = token(2);
        let faba = token(3);
        let end = token(4);
        let long_a = token(5);
        let long_b = token(6);
        let long_c = token(7);
        let query = prepare(trade(&[(1, 1)], &[4]), &[NATIVE, faba], Some(token(8))).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), mid), vec![segment(1, 1, 1)]),
            ((mid, NATIVE), vec![segment(1, 1, 2)]),
            ((NATIVE, faba), vec![segment(1, 1, 3)]),
            ((faba, end), vec![segment(1, 1, 4)]),
            ((mid, long_a), vec![segment(1, 1, 5)]),
            ((long_a, long_b), vec![segment(1, 1, 6)]),
            ((long_b, long_c), vec![segment(1, 1, 7)]),
            ((long_c, faba), vec![segment(1, 1, 8)]),
        ]));
        let set = execute(&query, &catalog);
        let found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        assert!(
            found
                .iter()
                .any(|path| path == &vec![token(1), mid, NATIVE, faba, end]),
            "{found:?}"
        );
    }

    #[test]
    fn native_input_walks_onward_from_the_wrapped_token() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(10))],
                outputs: vec![token(3)],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let catalog = Catalog(HashMap::from([
            ((wrapped, token(2)), vec![segment(1, 1, 1)]),
            ((token(2), token(3)), vec![segment(1, 1, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![NATIVE, wrapped, token(2), token(3)]
        );
    }

    #[test]
    fn a_native_wrap_does_not_repeat_the_native_token() {
        let wrapped = token(1);
        let out = token(3);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(10))],
                outputs: vec![out],
            },
            &[NATIVE, wrapped, out],
            Some(wrapped),
        )
        .unwrap();
        let catalog = Catalog(HashMap::from([
            ((NATIVE, out), vec![segment(1, 1, 1)]),
            ((wrapped, out), vec![segment(1, 1, 2)]),
        ]));
        let set = execute(&query, &catalog);
        let found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        assert!(
            found.iter().any(|path| path == &vec![NATIVE, out]),
            "{found:?}"
        );
        assert!(
            found.iter().any(|path| path == &vec![NATIVE, wrapped, out]),
            "{found:?}"
        );
        assert!(
            found.iter().all(|path| {
                let mut seen = std::collections::HashSet::new();
                path.iter().all(|token| seen.insert(*token))
            }),
            "{found:?}"
        );
    }

    #[test]
    fn a_path_through_another_output_is_left_out() {
        let query = prepare(trade(&[(1, 10)], &[2, 3]), &[token(2)], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), token(3)), vec![segment(1, 1, 2)]),
            ((token(1), token(3)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        let found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        assert!(
            found.iter().any(|path| path == &vec![token(1), token(2)]),
            "{found:?}"
        );
        assert!(
            found.iter().any(|path| path == &vec![token(1), token(3)]),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|path| path != &vec![token(1), token(2), token(3)]),
            "{found:?}"
        );
    }

    #[test]
    fn a_path_through_another_input_is_left_out() {
        let query = prepare(trade(&[(1, 10), (4, 10)], &[3]), &[token(4)], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(4)), vec![segment(1, 1, 1)]),
            ((token(4), token(3)), vec![segment(1, 1, 2)]),
            ((token(1), token(3)), vec![segment(1, 1, 3)]),
            ((token(4), token(1)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        let found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        assert!(
            found.iter().any(|path| path == &vec![token(1), token(3)]),
            "{found:?}"
        );
        assert!(
            found.iter().any(|path| path == &vec![token(4), token(3)]),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|path| path != &vec![token(1), token(4), token(3)]),
            "{found:?}"
        );
    }
}
