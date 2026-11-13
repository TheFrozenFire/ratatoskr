mod hub;
mod kind;
mod path;

use num_bigint::{BigInt, BigUint};
use searcher::Token;
use trade::Trade;

use crate::graph::{PathEnds, Selection};
use crate::pick::Picks;
use crate::scan::Survey;
use crate::token::Known;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Ordinary,
    Long,
    Via,
    Deep,
    Isolated,
}

pub(crate) struct Chosen<'a> {
    pub(crate) survey: &'a Survey,
    pub(crate) known: &'a [Known],
    pub(crate) hubs: &'a [Token],
    pub(crate) wrapped: Token,
    pub(crate) picks: &'a Picks,
    pub(crate) selection: &'a Selection,
    pub(crate) long: &'a [PathEnds],
    pub(crate) via: &'a [PathEnds],
    pub(crate) isolated: &'a [PathEnds],
}

pub(crate) struct Scenario {
    pub(crate) name: String,
    pub(crate) detail: String,
    pub(crate) trade: Trade,
    pub(crate) hubs: Vec<Token>,
    pub(crate) paths: Vec<Vec<Token>>,
    pub(crate) show_path: bool,
    pub(crate) direct_kinds: Option<u32>,
    pub(crate) expect_pure: Vec<(Token, Token, u32)>,
    pub(crate) must_empty: bool,
    pub(crate) expect_routes: Option<usize>,
    pub(crate) sweep: Option<String>,
    pub(crate) require_end: Option<Token>,
    pub(crate) forbid_ends: Vec<Token>,
    pub(crate) reach: Vec<Token>,
    pub(crate) must_wrap: bool,
    pub(crate) cover: bool,
    pub(crate) exact_path: bool,
    pub(crate) class: Class,
}

pub(crate) fn scenarios(chosen: &Chosen<'_>) -> Vec<Scenario> {
    let Chosen {
        survey,
        known,
        hubs,
        wrapped,
        picks,
        selection,
        long,
        via,
        isolated,
    } = chosen;
    let mut out = Vec::new();
    out.extend(hub::pair_scenarios(known, hubs, *wrapped));
    out.extend(hub::native_scenarios(known, hubs, *wrapped));
    out.extend(hub::basket_scenarios(known, hubs, *wrapped));
    out.extend(kind::width_scenarios(survey, known, hubs, *wrapped));
    out.extend(kind::kind_scenarios(picks, known, hubs, *wrapped));
    out.extend(kind::native_kind_scenarios(survey, known, hubs, *wrapped));
    out.extend(kind::bundle_scenarios(picks, known, hubs, *wrapped));
    out.extend(path::via_scenarios(via, known, hubs, *wrapped));
    out.extend(kind::split_scenarios(
        via,
        &selection.deep_from,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(path::deep_scenarios(
        &survey.graph,
        &selection.deep_to,
        true,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(path::deep_scenarios(
        &survey.graph,
        &selection.deep_from,
        false,
        known,
        hubs,
        *wrapped,
    ));
    out.extend(path::long_scenarios(long, hubs, *wrapped));
    out.extend(path::multi_scenarios(long, known, hubs, *wrapped));
    out.extend(path::isolated_scenarios(isolated, hubs, *wrapped));
    out
}

pub(crate) fn defined(inputs: Vec<(Token, BigUint)>, outputs: Vec<Token>, wrapped: Token) -> Trade {
    Trade {
        inputs: inputs
            .into_iter()
            .map(|(token, amount)| (token, BigInt::from(amount)))
            .collect(),
        outputs: outputs
            .into_iter()
            .map(|token| (token, BigInt::from(1)))
            .collect(),
        wrapped: Some(wrapped),
        gas_ceiling: None,
    }
}

pub(crate) fn single(
    name: impl Into<String>,
    detail: impl Into<String>,
    from: Token,
    to: Token,
    value: BigUint,
    hubs: &[Token],
    wrapped: Token,
) -> Scenario {
    Scenario {
        name: name.into(),
        detail: detail.into(),
        trade: defined(vec![(from, value)], vec![to], wrapped),
        hubs: hubs.to_vec(),
        paths: Vec::new(),
        show_path: false,
        direct_kinds: None,
        expect_pure: Vec::new(),
        must_empty: false,
        expect_routes: None,
        sweep: None,
        require_end: None,
        forbid_ends: Vec::new(),
        reach: Vec::new(),
        must_wrap: false,
        cover: false,
        exact_path: false,
        class: Class::Ordinary,
    }
}

pub(crate) fn multi(
    name: &str,
    detail: &str,
    inputs: Vec<(Token, BigUint)>,
    outputs: Vec<Token>,
    hubs: &[Token],
    wrapped: Token,
) -> Scenario {
    Scenario {
        name: name.into(),
        detail: detail.into(),
        trade: defined(inputs, outputs, wrapped),
        hubs: hubs.to_vec(),
        paths: Vec::new(),
        show_path: false,
        direct_kinds: None,
        expect_pure: Vec::new(),
        must_empty: false,
        expect_routes: None,
        sweep: None,
        require_end: None,
        forbid_ends: Vec::new(),
        reach: Vec::new(),
        must_wrap: false,
        cover: false,
        exact_path: false,
        class: Class::Ordinary,
    }
}

pub(crate) fn path_ends(path: &[Token]) -> Option<(Token, Token)> {
    Some((*path.first()?, *path.last()?))
}
