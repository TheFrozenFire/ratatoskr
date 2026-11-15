//! Closed-form curve fits.
//!
//! A [`Formula`] is the stored quote for one direction. Prediction is the
//! integer quote for each invariant. Selection searches those formulas against
//! a labeled sample and keeps the best score. The search order matches
//! `cas/select_fit.py`. Catalog export and on-chain quote collection stay in
//! `venues`.

#![allow(dead_code)]
#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::explicit_auto_deref
)]

mod eval;
mod formula;
mod predict;
mod select;
mod series;
mod units;

pub use eval::eval_formula;
pub use formula::{decode_fit, encode_fit, FitPayload, Formula, StoredFit};
pub use predict::{crypto2_out, crypto3_out};
pub use select::Chosen;
pub use select::{
    amount, choose, labels_of, logistic_outs, poke_choose, poke_formula, select_fit, whole,
};
