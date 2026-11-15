use super::*;
use crate::eval::{eval_formula, logistic_quote};
use crate::formula::Formula;
use crate::predict::{
    added_residual, chord_quote, hyperbola_out, product_out, residual_quote, stable_swap_out,
    worst_bps, Predict,
};
use crate::series::*;
use crate::units::*;
use bigdecimal::BigDecimal;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use serde_json::{json, Value};
use std::sync::Arc;

fn parse_bi(text: &str) -> BigInt {
    text.parse().expect("integer")
}

fn direction(samples: &[(&str, &str, &str)], token_in: &str, token_out: &str) -> Value {
    json!({
        "token_in": token_in,
        "token_out": token_out,
        "samples": samples.iter().map(|(x, y, fraction)| json!({
            "amount_in": x,
            "chain_out": y,
            "fraction": fraction,
        })).collect::<Vec<_>>()
    })
}

fn rows_of(samples: &[(&str, &str, &str)]) -> Vec<(BigInt, BigInt)> {
    samples
        .iter()
        .map(|(x, y, _)| (parse_bi(x), parse_bi(y)))
        .collect()
}

#[test]
fn a_short_book_and_a_zero_divisor_do_not_panic() {
    let missing = Formula::StablePpms {
        xp: vec![bi(1), bi(1)],
        amp: bi(100),
        i: -1,
        j: 5,
        fee: BigInt::zero(),
        rates: vec![BigInt::zero()],
        ppms: Vec::new(),
    };
    assert!(eval_formula(&missing, &bi(10)).is_none());
    let zero_ppm = Formula::StableScale {
        xp: vec![bi(1_000), bi(1_000)],
        amp: bi(100),
        i: 0,
        j: 1,
        fee: BigInt::zero(),
        rates: vec![BigInt::zero(), BigInt::zero()],
        which: 0,
        ppm: BigInt::zero(),
        denom: bi(1_000_000),
        fee_on_input: true,
    };
    let _ = eval_formula(&zero_ppm, &bi(100));
    poke_formula(&[0xff, 3, 0, 0], &bi(1));
    // Tag 2 then a list length of 0xf4f4f4f4. The length is not allocated.
    poke_formula(&[2, 244, 244, 244, 244, 244], &bi(1));
    poke_formula(&[22u8; 64], &bi(1));
    assert!(Formula::decode(&[2, 244, 244, 244, 244, 244]).is_err());
    poke_choose(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    poke_choose(br#"{"shape":"constant_product","samples":[{"amount_in":"10","chain_out":"9","fraction":"0.1"}]}"#);
}

#[test]
fn logistic_formula_round_trips_off_the_grid() {
    let formula = Formula::Logistic {
        slope: rat_i(2),
        rate: BigRational::new(bi(1), bi(100)),
        knee: rat_i(100),
    };
    let stored = crate::formula::StoredFit {
        token_in: alloy_primitives::Address::ZERO,
        token_out: alloy_primitives::Address::ZERO,
        score: 0,
        name: "cap".to_string(),
        formula: formula.clone(),
    };
    let bytes = crate::formula::encode_fit(0, &[stored]);
    let decoded = crate::formula::decode_fit(&bytes).unwrap();
    assert!(decoded.directions[0].formula == formula);
    assert_eq!(
        eval_formula(&decoded.directions[0].formula, &bi(50)).unwrap(),
        bi(67)
    );
    assert_eq!(
        eval_formula(&decoded.directions[0].formula, &bi(200)).unwrap(),
        bi(199)
    );
}

#[test]
fn logistic_matches_decimal_context() {
    let predict = logistic_quote(&rat_i(2), &BigRational::new(bi(1), bi(100)), &rat_i(100));
    assert_eq!(predict(bi(50)).unwrap(), bi(67));
    assert_eq!(predict(bi(200)).unwrap(), bi(199));
    let got = he40(ln1exp(&"1.5".parse().unwrap()));
    let expect = he40("1.701413277982752409499482809057091105254".parse().unwrap());
    assert_eq!(got, expect);
    let ln2 = he40(ln_he(&BigDecimal::from(2)));
    let expect_ln2 = he40(
        "0.6931471805599453094172321214581765680755"
            .parse()
            .unwrap(),
    );
    assert_eq!(ln2, expect_ln2);
}

#[test]
fn selector_structural() {
    let dust_score = worst_bps(&|_| Some(bi(2_000)), &[(bi(1), bi(1_000))]);
    assert_eq!(dust_score, Some(0));
    let over = worst_bps(&|_| Some(bi(2_001)), &[(bi(1), bi(1_000))]);
    assert_eq!(over, Some(10_010));

    let y_small = product_out(
        &bi(1000),
        &bi(1000),
        &bi(100),
        &BigInt::zero(),
        &bi(1_000_000),
    )
    .unwrap();
    let y_large = product_out(
        &bi(1000),
        &bi(1000),
        &bi(400),
        &BigInt::zero(),
        &bi(1_000_000),
    )
    .unwrap();
    let product = json!({
        "shape": "constant_product",
        "reserve0": "1000",
        "reserve1": "1000",
        "fee_pips_used": 0,
        "token0": "a",
        "token1": "b",
    });
    let small = y_small.to_string();
    let large = y_large.to_string();
    let tied_samples = [("100", small.as_str(), "0.1"), ("400", large.as_str(), "1")];
    let tied = choose(
        &product,
        &direction(&tied_samples, "a", "b"),
        &rows_of(&tied_samples),
    )
    .unwrap();
    let tied = tied.expect("product tie");
    assert_eq!((tied.score, tied.name), (0, "product"));

    let bow = hyperbola_out(
        &bi(100_000),
        &bi(500_000),
        &bi(400_000),
        &bi(800_000),
        &bi(200_000),
    )
    .unwrap();
    let bow_text = bow.to_string();
    let wrong = json!({
        "shape": "constant_product",
        "reserve0": "100000",
        "reserve1": "100000",
        "fee_pips_used": 0,
        "token0": "a",
        "token1": "b",
    });
    let closer_samples = [
        ("100000", "500000", "0.1"),
        ("200000", bow_text.as_str(), "0.5"),
        ("400000", "800000", "1"),
    ];
    let closer = choose(
        &wrong,
        &direction(&closer_samples, "a", "b"),
        &rows_of(&closer_samples),
    )
    .unwrap()
    .expect("hyperbola");
    assert_eq!(closer.name, "hyperbola");
    assert!(closer.score <= 10);

    let line = [
        ("100000", "1000000", "0.1"),
        ("200000", "1100000", "0.25"),
        ("400000", "1300000", "0.5"),
        ("800000", "1700000", "0.8"),
        ("1000000", "1900000", "1"),
    ];
    let blank = json!({"shape": "curve", "coins": [], "crypto": false});
    let kept = choose(&blank, &direction(&line, "a", "b"), &rows_of(&line))
        .unwrap()
        .expect("line");
    assert!(kept.score <= 10);
    assert!(kept.name == "chord" || kept.name == "cubic");

    let spiked = [
        ("100", "100", "0.1"),
        ("200", "100000", "0.25"),
        ("400", "500", "0.5"),
        ("900", "900", "1"),
    ];
    assert!(
        choose(&blank, &direction(&spiked, "a", "b"), &rows_of(&spiked))
            .unwrap()
            .is_none()
    );

    let train: Vec<(BigInt, BigInt)> = line
        .iter()
        .filter(|(_, _, fraction)| !HOLD_FRACTIONS.contains(fraction))
        .map(|(x, y, _)| (parse_bi(x), parse_bi(y)))
        .collect();
    let hold: Vec<(BigInt, BigInt)> = line
        .iter()
        .filter(|(_, _, fraction)| HOLD_FRACTIONS.contains(fraction))
        .map(|(x, y, _)| (parse_bi(x), parse_bi(y)))
        .collect();
    let refit = chord_quote(&train).expect("chord");
    assert!(worst_bps(refit.as_ref(), &hold).unwrap() <= 10);
}

fn pack(
    samples: &[(&str, &str, &str)],
    token_in: &str,
    token_out: &str,
) -> (Value, Vec<(BigInt, BigInt)>) {
    (direction(samples, token_in, token_out), rows_of(samples))
}

fn spike(
    samples: &[(&str, &str, &str)],
    token_in: &str,
    token_out: &str,
) -> (Value, Vec<(BigInt, BigInt)>) {
    let owned: Vec<(String, String, String)> = samples
        .iter()
        .map(|(x, y, fraction)| {
            let y = parse_bi(y);
            let y = if *fraction == "0.25" || *fraction == "0.8" {
                y * 2
            } else {
                y
            };
            ((*x).to_string(), y.to_string(), (*fraction).to_string())
        })
        .collect();
    let refs: Vec<(&str, &str, &str)> = owned
        .iter()
        .map(|(x, y, fraction)| (x.as_str(), y.as_str(), fraction.as_str()))
        .collect();
    pack(&refs, token_in, token_out)
}

fn curve_coins(balances: &[&str], amp: &str, fee: &str) -> Value {
    let names = ["a", "b", "c", "d"];
    json!({
        "shape": "curve",
        "crypto": false,
        "amp": amp,
        "fee": fee,
        "coins": balances.iter().enumerate().map(|(n, balance)| json!({
            "token": names[n],
            "balance": balance,
            "rate": "0",
        })).collect::<Vec<_>>()
    })
}

fn sized_quotes(
    maker: impl Fn(&BigInt) -> Option<BigInt>,
    stored: &BigInt,
) -> Vec<(String, String, String)> {
    let cuts = [
        ("0.1", fd(stored, &bi(50))),
        ("0.25", fd(stored, &bi(10))),
        ("0.5", fd(stored, &bi(3))),
        ("0.8", fd(stored, &bi(2))),
        ("1", fd(&(stored * bi(4)), &bi(5))),
    ];
    cuts.into_iter()
        .map(|(fraction, dx)| {
            let quoted = maker(&dx).expect("quote");
            assert!(quoted.is_positive());
            (dx.to_string(), quoted.to_string(), fraction.to_string())
        })
        .collect()
}

fn refs_of(owned: &[(String, String, String)]) -> Vec<(&str, &str, &str)> {
    owned
        .iter()
        .map(|(x, y, fraction)| (x.as_str(), y.as_str(), fraction.as_str()))
        .collect()
}

#[test]
fn selector_scale_windows() {
    let stored = pow10(22);
    let amp = bi(100);
    let ppm_true = bi(15_000);
    let true_xp = vec![fd(&(&stored * &ppm_true), &bi(10_000)), stored.clone()];
    let wide_owned = sized_quotes(
        |dx| {
            let paid = fd(&(dx * &ppm_true), &bi(10_000));
            stable_swap_out(
                &true_xp,
                amp.clone(),
                bi(0),
                bi(1),
                paid,
                BigInt::zero(),
                fee_denom(),
                false,
                BigInt::zero(),
            )
        },
        &stored,
    );
    let wide_refs = refs_of(&wide_owned);
    let (wide_dir, wide_rows) = pack(&wide_refs, "a", "b");
    let wide_state = curve_coins(&[&stored.to_string(), &stored.to_string()], "100", "0");
    let wide_got = stable_scale_candidate(&wide_state, 0, 1, &wide_dir, &wide_rows)
        .unwrap()
        .expect("wide scale");
    assert!(wide_got.0 <= 10);
    let chosen = choose(&wide_state, &wide_dir, &wide_rows)
        .unwrap()
        .expect("wide choose");
    assert!(chosen.score <= wide_got.0);
    if chosen.score == wide_got.0 {
        assert_eq!(chosen.name, "stable");
    }

    let bal_owned = sized_quotes(
        |dx| {
            let paid = fd(&(dx * &ppm_true), &bi(10_000));
            stable_swap_out(
                &true_xp,
                amp.clone(),
                bi(0),
                bi(1),
                paid,
                BigInt::zero(),
                wad(),
                true,
                BigInt::zero(),
            )
        },
        &stored,
    );
    let bal_refs = refs_of(&bal_owned);
    let (bal_dir, bal_rows) = pack(&bal_refs, "a", "b");
    let bal_state = json!({
        "shape": "balancer",
        "stable": true,
        "amp": "1000",
        "amp_precision": "10",
        "fee": "0",
        "balances": [stored.to_string(), stored.to_string()],
        "weights": [],
        "tokens": ["a", "b"],
    });
    let bal_got = balancer_scale_candidate(&bal_state, 0, 1, &bal_dir, &bal_rows)
        .unwrap()
        .expect("balancer scale");
    assert!(bal_got.0 <= 10);

    let out_xp = vec![stored.clone(), stored.clone()];
    let out_owned = sized_quotes(
        |dx| {
            let quoted = stable_swap_out(
                &out_xp,
                bi(100),
                bi(0),
                bi(1),
                dx.clone(),
                BigInt::zero(),
                fee_denom(),
                false,
                BigInt::zero(),
            )?;
            Some(fd(&(&quoted * bi(9960)), &bi(10_000)))
        },
        &stored,
    );
    let out_refs = refs_of(&out_owned);
    let (out_dir, out_rows) = pack(&out_refs, "a", "b");
    let raw: Predict = Arc::new({
        let out_xp = out_xp.clone();
        move |dx| {
            stable_swap_out(
                &out_xp,
                bi(100),
                bi(0),
                bi(1),
                dx,
                BigInt::zero(),
                fee_denom(),
                false,
                BigInt::zero(),
            )
        }
    });
    let bare = worst_bps(raw.as_ref(), &out_rows).expect("bare");
    assert!(bare > 10);
    let out_state = curve_coins(&[&stored.to_string(), &stored.to_string()], "100", "0");
    let out_got = stable_output_candidate(&out_state, 0, 1, &out_dir, &out_rows)
        .unwrap()
        .expect("output scale");
    assert!(out_got.0 <= 10);
}

#[test]
fn selector_residuals() {
    let identity = Formula::Identity;
    let level = [
        ("100000", "105000", "0.1"),
        ("250000", "255000", "0.25"),
        ("500000", "505000", "0.5"),
        ("800000", "805000", "0.8"),
        ("1000000", "1005000", "1"),
    ];
    let (level_dir, level_rows) = pack(&level, "a", "b");
    let lifted = base_residual_candidate(&identity, &level_dir, &level_rows)
        .unwrap()
        .expect("lifted");
    assert!(lifted.0 <= 10);

    let spiked_level = [
        ("100000", "100000", "0.1"),
        ("250000", "350000", "0.25"),
        ("500000", "500000", "0.5"),
        ("800000", "800000", "0.8"),
        ("1000000", "1000000", "1"),
    ];
    let (spiked_dir, spiked_rows) = pack(&spiked_level, "a", "b");
    assert!(
        base_residual_candidate(&identity, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );
    let thin = [
        ("100000", "105000", "0.1"),
        ("250000", "255000", "0.25"),
        ("800000", "805000", "0.8"),
    ];
    let (thin_dir, thin_rows) = pack(&thin, "a", "b");
    assert!(base_residual_candidate(&identity, &thin_dir, &thin_rows)
        .unwrap()
        .is_none());

    let scaled_book = [
        ("115031368", "40565", "0.0001"),
        ("1150313686", "405369", "0.001"),
        ("11503136860", "4028173", "0.01"),
        ("57515684301", "19559573", "0.05"),
        ("115031368603", "37341258", "0.1"),
        ("287578421508", "82120432", "0.25"),
        ("575156843017", "136742416", "0.5"),
        ("920250948828", "182192054", "0.8"),
        ("1092798001734", "199658026", "0.95"),
        ("1150313686035", "204896279", "1"),
    ];
    let (scaled_dir, scaled_rows) = pack(&scaled_book, "a", "b");
    let (scaled_train, _) = split_holdout(&scaled_dir).unwrap();
    let bare_residual = residual_quote(&scaled_train).expect("residual");
    assert!(worst_bps(bare_residual.as_ref(), &scaled_rows).unwrap() > 10);
    let scaled_got = scaled_residual_candidate(&scaled_dir, &scaled_rows)
        .unwrap()
        .expect("scaled");
    assert!(scaled_got.0 <= 10);
    assert!(scaled_residual_candidate(&spiked_dir, &spiked_rows)
        .unwrap()
        .is_none());
    assert!(scaled_residual_candidate(&thin_dir, &thin_rows)
        .unwrap()
        .is_none());

    let flat_book = [
        ("14548", "2395240484", "2461420875", "0.0001"),
        ("145488", "10856634268", "11070063847", "0.001"),
        ("1454880", "66069602810", "67749161460", "0.01"),
        ("7274403", "211903794191", "215625141636", "0.05"),
        ("14548807", "297997313258", "301688177096", "0.1"),
        ("36372019", "395260737247", "397871903127", "0.25"),
        ("72744039", "443764316235", "445406024721", "0.5"),
        ("116390462", "465195596129", "466323721194", "0.8"),
        ("138213674", "471188786545", "472160474742", "0.95"),
        ("145488078", "472811276491", "473741935483", "1"),
    ];
    let mut flat_samples = Vec::new();
    let mut pairs = Vec::new();
    for (x, y, pred, fraction) in flat_book {
        pairs.push((parse_bi(x), parse_bi(pred)));
        flat_samples.push((x, y, fraction));
    }
    let ladder = Formula::Lookup { pairs };
    let (flat_dir, flat_rows) = pack(&flat_samples, "a", "b");
    let (flat_train, _) = split_holdout(&flat_dir).unwrap();
    let clamped =
        added_residual(ladder.predict(), &flat_train, &BigRational::one()).expect("clamped");
    assert!(worst_bps(clamped.as_ref(), &flat_rows).unwrap() > 10);
    let flat_got = flat_residual_candidate(&ladder, &flat_dir, &flat_rows)
        .unwrap()
        .expect("flat");
    assert!(flat_got.0 <= 10);
    assert!(
        flat_residual_candidate(&identity, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );
}

#[test]
fn selector_loo_alpha() {
    let alpha_samples = [
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
    let (alpha_dir, alpha_rows) = pack(&alpha_samples, "a", "b");
    let (alpha_train, _) = split_holdout(&alpha_dir).unwrap();
    let alpha_base = hyperbola_candidate(&alpha_rows).expect("hyperbola");
    let clamped = added_residual(alpha_base.1.predict(), &alpha_train, &BigRational::one())
        .expect("clamped alpha");
    assert!(worst_bps(clamped.as_ref(), &alpha_rows).unwrap() > 10);
    let alpha_got = loo_alpha_candidate(&alpha_base.1, &alpha_dir, &alpha_rows)
        .unwrap()
        .expect("loo");
    assert!(alpha_got.0 <= 10);
    let alpha_state = json!({"shape": "curve", "coins": []});
    let choice = choose(&alpha_state, &alpha_dir, &alpha_rows)
        .unwrap()
        .expect("alpha choose");
    assert!(choice.score <= 10);
    assert!(choice.score <= alpha_got.0);
    let (spiked_dir, spiked_rows) = spike(&alpha_samples, "a", "b");
    let spiked_base = hyperbola_candidate(&spiked_rows).expect("spiked hyperbola");
    assert!(
        loo_alpha_candidate(&spiked_base.1, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );
}

fn assert_named(choice: &Chosen, name: &str, limit: i64) {
    assert!(choice.score <= limit, "{}", choice.score);
    assert_eq!(choice.name, name);
}

#[test]
fn selector_joints() {
    let state = curve_coins(&["2546456916", "2316863024", "3983216"], "2000", "4000000");
    let joint_samples = [
        ("617833", "611134", "0.0001"),
        ("6178334", "6111275", "0.001"),
        ("61783341", "61105796", "0.01"),
        ("308916709", "305368078", "0.05"),
        ("617833418", "610278299", "0.1"),
        ("1544583547", "1518777006", "0.25"),
        ("3089167094", "2299616136", "0.5"),
        ("4942667351", "2312289490", "0.8"),
        ("5869417479", "2313538623", "0.95"),
        ("6178334189", "2313808815", "1"),
    ];
    let (joint_dir, joint_rows) = pack(&joint_samples, "a", "b");
    let stored = stable_candidate(&state, 0, 1, &joint_rows)
        .unwrap()
        .expect("stored");
    assert!(stored.0 > 10);
    let one = stable_scale_candidate(&state, 0, 1, &joint_dir, &joint_rows).unwrap();
    assert!(one.as_ref().is_none_or(|item| item.0 > 10));
    let joint_got = stable_joint_candidate(&state, 0, 1, &joint_dir, &joint_rows)
        .unwrap()
        .expect("joint");
    assert!(joint_got.0 <= 10);
    let choice = choose(&state, &joint_dir, &joint_rows)
        .unwrap()
        .expect("joint choose");
    assert!(choice.score <= joint_got.0);
    assert_named(&choice, "stable", 10);
    let (spiked_dir, spiked_rows) = spike(&joint_samples, "a", "b");
    assert!(
        stable_joint_candidate(&state, 0, 1, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );

    let back_samples = [
        ("615342", "621588", "0.0001"),
        ("6153427", "6215815", "0.001"),
        ("61534271", "62151089", "0.01"),
        ("307671355", "310598874", "0.05"),
        ("615342711", "620784689", "0.1"),
        ("1538356779", "1546878243", "0.25"),
        ("3076713558", "2523187763", "0.5"),
        ("4922741693", "2541394325", "0.8"),
        ("5845755761", "2542841442", "0.95"),
        ("6153427116", "2543146698", "1"),
    ];
    let (back_dir, back_rows) = pack(&back_samples, "b", "a");
    let back_stored = stable_candidate(&state, 1, 0, &back_rows)
        .unwrap()
        .expect("back stored");
    assert!(back_stored.0 > 10);
    let back_one = stable_scale_candidate(&state, 1, 0, &back_dir, &back_rows).unwrap();
    assert!(back_one.as_ref().is_none_or(|item| item.0 > 10));
    assert!(stable_joint_candidate(&state, 1, 0, &back_dir, &back_rows)
        .unwrap()
        .is_none());
    let back_got = stable_joint_best_candidate(&state, 1, 0, &back_dir, &back_rows)
        .unwrap()
        .expect("joint best");
    assert!(back_got.0 <= 10);
    let back_choice = choose(&state, &back_dir, &back_rows)
        .unwrap()
        .expect("back choose");
    assert!(back_choice.score <= back_got.0);
    assert_named(&back_choice, "stable", 10);
    let (spiked_back_dir, spiked_back_rows) = spike(&back_samples, "b", "a");
    assert!(
        stable_joint_best_candidate(&state, 1, 0, &spiked_back_dir, &spiked_back_rows)
            .unwrap()
            .is_none()
    );
}

fn blank_curve() -> Value {
    json!({"shape": "curve", "coins": []})
}

fn expect_cap(
    samples: &[(&str, &str, &str)],
    run: fn(&Value, &[(BigInt, BigInt)]) -> Result<Option<Scored>, String>,
    limit: i64,
) {
    let (dir, rows) = pack(samples, "a", "b");
    let got = run(&dir, &rows).unwrap().expect("candidate");
    assert!(got.0 <= limit, "{}", got.0);
    let choice = choose(&blank_curve(), &dir, &rows)
        .unwrap()
        .expect("choose");
    assert!(choice.score <= got.0, "{} > {}", choice.score, got.0);
    assert_named(&choice, "cap", 10);
    let (spiked_dir, spiked_rows) = spike(samples, "a", "b");
    assert!(run(&spiked_dir, &spiked_rows).unwrap().is_none());
}

#[test]
fn selector_fillet_and_logistic_caps() {
    let fillet = [
        ("5375423", "5485986", "0.0001"),
        ("53754234", "54856464", "0.001"),
        ("537542342", "548227631", "0.01"),
        ("2687711711", "2729262949", "0.05"),
        ("5375423422", "4345001472", "0.1"),
        ("13438558556", "4377017501", "0.25"),
        ("26877117112", "4378770599", "0.5"),
        ("43003387380", "4379090767", "0.8"),
        ("51066522513", "4379149123", "0.95"),
        ("53754234225", "4379162930", "1"),
    ];
    expect_cap(&fillet, fillet_cap_candidate, 10);

    let grid = [
        ("3022185", "21069261", "0.0001"),
        ("30221852", "210691862", "0.001"),
        ("302218524", "2106841153", "0.01"),
        ("1511092620", "10532244219", "0.05"),
        ("3022185241", "21058084913", "0.1"),
        ("7555463103", "52525740806", "0.25"),
        ("15110926206", "79237047434", "0.5"),
        ("24177481929", "79474008911", "0.8"),
        ("28710759791", "79498583385", "0.95"),
        ("30221852412", "79504017100", "1"),
    ];
    let (grid_dir, grid_rows) = pack(&grid, "a", "b");
    assert!(logistic_refine_candidate(&grid_dir, &grid_rows)
        .unwrap()
        .is_none());
    expect_cap(&grid, logistic_cap_candidate, 10);

    let refined = [
        ("411826208851860009610", "440581", "0.0001"),
        ("4118262088518600096103", "4405808", "0.001"),
        ("41182620885186000961034", "44057742", "0.01"),
        ("205913104425930004805172", "220280812", "0.05"),
        ("411826208851860009610344", "440539860", "0.1"),
        ("1029565522129650024025860", "1101129923", "0.25"),
        ("2059131044259300048051720", "2200163593", "0.5"),
        ("3294609670814880076882752", "3013664017", "0.8"),
        ("3912348984092670091298268", "3018012608", "0.95"),
        ("4118262088518600096103440", "3018543490", "1"),
    ];
    let (refine_dir, refine_rows) = pack(&refined, "a", "b");
    assert!(logistic_cap_candidate(&refine_dir, &refine_rows)
        .unwrap()
        .is_none());
    expect_cap(&refined, logistic_refine_candidate, 10);
}

#[test]
fn selector_waves() {
    let series = [
        [
            ("4667", "1224633", "0.0001"),
            ("46672", "12246297", "0.001"),
            ("466720", "122458856", "0.01"),
            ("2333602", "612196439", "0.05"),
            ("4667204", "1224112113", "0.1"),
            ("11668011", "3057097607", "0.25"),
            ("23336022", "6059820178", "0.5"),
            ("37337636", "7083955298", "0.8"),
            ("44338443", "7096893380", "0.95"),
            ("46672045", "7099144840", "1"),
        ],
        [
            ("4667", "1088171", "0.0001"),
            ("46672", "10881697", "0.001"),
            ("466720", "108814837", "0.01"),
            ("2333602", "544025227", "0.05"),
            ("4667204", "1087920138", "0.1"),
            ("11668011", "2718601257", "0.25"),
            ("23336022", "5429021430", "0.5"),
            ("37337636", "8392769757", "0.8"),
            ("44338443", "8604957057", "0.95"),
            ("46672045", "8616365875", "1"),
        ],
    ];
    for samples in series {
        let (dir, rows) = pack(&samples, "a", "b");
        assert!(fillet_cap_candidate(&dir, &rows).unwrap().is_none());
        assert!(logistic_cap_candidate(&dir, &rows).unwrap().is_none());
        assert!(logistic_refine_candidate(&dir, &rows).unwrap().is_none());
        expect_cap(&samples, logistic_wave_candidate, 10);
    }
    let wide = [
        [
            ("3022185241214", "18", "0.0001"),
            ("30221852412148", "187", "0.001"),
            ("302218524121487", "1870", "0.01"),
            ("1511092620607439", "9350", "0.05"),
            ("3022185241214879", "18698", "0.1"),
            ("7555463103037197", "46690", "0.25"),
            ("15110926206074395", "82198", "0.5"),
            ("24177481929719033", "82766", "0.8"),
            ("28710759791541351", "82799", "0.95"),
            ("30221852412148791", "82806", "1"),
        ],
        [
            ("4667204582094", "1245", "0.0001"),
            ("46672045820941", "12454", "0.001"),
            ("466720458209411", "124545", "0.01"),
            ("2333602291047059", "622629", "0.05"),
            ("4667204582094118", "1244976", "0.1"),
            ("11668011455235296", "3109251", "0.25"),
            ("23336022910470592", "6165343", "0.5"),
            ("37337636656752947", "7239830", "0.8"),
            ("44338443529894125", "7253306", "0.95"),
            ("46672045820941184", "7255638", "1"),
        ],
    ];
    for samples in wide {
        expect_cap(&samples, logistic_wide_candidate, 10);
    }
    let wave_loo = [
        ("1255412165039985654", "434150601494383576", "0.0001"),
        ("12554121650399856548", "4341393668540135027", "0.001"),
        ("125541216503998565482", "43402511033146923132", "0.01"),
        ("627706082519992827411", "216735387493943511373", "0.05"),
        ("1255412165039985654822", "432645993685433256752", "0.1"),
        ("3138530412599964137055", "1071407515238329484054", "0.25"),
        ("6277060825199928274110", "1969321888656347844695", "0.5"),
        ("10043297320319885238576", "2183065786340123384542", "0.8"),
        ("11926415567879863720809", "2198779625485002206952", "0.95"),
        ("12554121650399856548221", "2201970551579885219215", "1"),
    ];
    expect_cap(&wave_loo, wave_loo_candidate, 4);
}

#[test]
fn selector_fee_refine() {
    let cases: [(usize, usize, &str, &str, &[(&str, &str, &str)]); 2] = [
        (
            1,
            0,
            "147974849398",
            "1008378630008940969171",
            &[
                ("925273707933325931", "10068003", "0.0001"),
                ("9252737079333259310", "99980743", "0.001"),
                ("92527370793332593108", "935384506", "0.01"),
                ("462636853966662965544", "3668188883", "0.05"),
                ("925273707933325931089", "5864773975", "0.1"),
                ("2313184269833314827722", "9649007359", "0.25"),
                ("4626368539666629655445", "13426138976", "0.5"),
                ("7402189663466607448712", "16970020761", "0.8"),
                ("8790100225366596345345", "18602700198", "0.95"),
                ("9252737079333259310890", "19135522827", "1"),
            ],
        ),
        (
            0,
            1,
            "9285165",
            "14825833172516094238",
            &[
                ("928516", "110402526507932229", "0.0001"),
                ("9285165", "664982939163483950", "0.001"),
                ("92851650", "1986343000224994734", "0.01"),
                ("464258250", "5705768274552803321", "0.05"),
                ("928516500", "10244720388441022256", "0.1"),
                ("2321291250", "14812074783128551103", "0.25"),
                ("4642582500", "14818781153909248236", "0.5"),
                ("7428132000", "14819525040808283310", "0.8"),
                ("8820906750", "14819644291707933287", "0.95"),
                ("9285165000", "14819671654549768915", "1"),
            ],
        ),
    ];
    for (coin_in, coin_out, left, right, samples) in cases {
        let names = ["a", "b"];
        let state = curve_coins(&[left, right], "200", "4000000");
        let (dir, rows) = pack(samples, names[coin_in], names[coin_out]);
        let stored = stable_candidate(&state, coin_in, coin_out, &rows)
            .unwrap()
            .expect("stored");
        assert!(stored.0 > 10 && stored.0 <= 400, "{}", stored.0);
        let got = stable_refine_candidate(&state, coin_in, coin_out, &dir, &rows)
            .unwrap()
            .expect("refine");
        assert!(got.0 <= 10, "{}", got.0);
        let choice = choose(&state, &dir, &rows).unwrap().expect("refine choose");
        assert!(choice.score <= got.0);
        assert_named(&choice, "stable", 10);
        let (spiked_dir, spiked_rows) = spike(samples, names[coin_in], names[coin_out]);
        assert!(
            stable_refine_candidate(&state, coin_in, coin_out, &spiked_dir, &spiked_rows)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn selector_consistent_and_mixed() {
    let first = json!({
        "shape": "curve",
        "crypto": true,
        "gamma": "126562100000000",
        "amp": "0",
        "d": "12815659946462930322364730",
        "mid_fee": "207000",
        "out_fee": "69314000",
        "fee_gamma": "4920000000000",
        "coins": [
            {"token": "a", "balance": "4285099268118", "price_scale": "75737331428183937831170", "rate": "0"},
            {"token": "b", "balance": "5613714639", "price_scale": "2235286570743132532000", "rate": "0"},
            {"token": "c", "balance": "1914246883653147602242", "price_scale": "0", "rate": "0"},
        ],
    });
    let first_samples = [
        ("191424688365314760", "425648398", "0.0001"),
        ("1914246883653147602", "4256127463", "0.001"),
        ("19142468836531476022", "42429340539", "0.01"),
        ("95712344182657380112", "205026346100", "0.05"),
        ("191424688365314760224", "390878874549", "0.1"),
        ("478561720913286900560", "857784758655", "0.25"),
        ("957123441826573801121", "1427014660772", "0.5"),
        ("1531397506922518081793", "1900545379336", "0.8"),
        ("1818534539470490222129", "2082496851463", "0.95"),
        ("1914246883653147602242", "2137063828523", "1"),
    ];
    let (dir, rows) = pack(&first_samples, "c", "a");
    let got = crypto3_consistent_candidate(&first, 2, 0, &dir, &rows)
        .unwrap()
        .expect("consistent");
    assert!(got.0 <= 2, "{}", got.0);
    let choice = choose(&first, &dir, &rows)
        .unwrap()
        .expect("consistent choose");
    assert!(choice.score <= got.0);
    assert_named(&choice, "three-coin", 2);
    let (spiked_dir, spiked_rows) = spike(&first_samples, "c", "a");
    assert!(
        crypto3_consistent_candidate(&first, 2, 0, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );

    let second = json!({
        "shape": "curve",
        "crypto": true,
        "gamma": "69999999999999",
        "amp": "3645",
        "d": "289349234090331792140547",
        "mid_fee": "4000000",
        "out_fee": "40000000",
        "fee_gamma": "10000000000000000",
        "coins": [
            {"token": "a", "balance": "91751712394", "price_scale": "78767292027298630679712", "rate": "0"},
            {"token": "b", "balance": "125226144", "price_scale": "2324095769479075828124", "rate": "0"},
            {"token": "c", "balance": "42599448406198332507", "price_scale": "0", "rate": "0"},
        ],
    });
    let second_samples = [
        ("12522", "4247424573662904", "0.0001"),
        ("125226", "42457482330770066", "0.001"),
        ("1252261", "422682754539822034", "0.01"),
        ("6261307", "2059617153289795134", "0.05"),
        ("12522614", "3946195433388600664", "0.1"),
        ("31306536", "8664432804945539603", "0.25"),
        ("62613072", "14386343500120694926", "0.5"),
        ("100180915", "19128341414405320511", "0.8"),
        ("118964836", "20946575943919077096", "0.95"),
        ("125226144", "21491455947841976087", "1"),
    ];
    let (dir, rows) = pack(&second_samples, "b", "c");
    let got = crypto3_consistent_candidate(&second, 1, 2, &dir, &rows)
        .unwrap()
        .expect("consistent 2");
    assert!(got.0 <= 2, "{}", got.0);
    let choice = choose(&second, &dir, &rows)
        .unwrap()
        .expect("consistent choose 2");
    assert!(choice.score <= got.0);
    assert_named(&choice, "three-coin", 2);
    let (spiked_dir, spiked_rows) = spike(&second_samples, "b", "c");
    assert!(
        crypto3_consistent_candidate(&second, 1, 2, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );

    let mixed_a = curve_coins(
        &[
            "6616494140643702778",
            "226374747",
            "721",
            "1550859921615324669071",
        ],
        "100",
        "4000000",
    );
    let mixed_a_samples = [
        ("155085992161532466", "2021942806401156", "0.0001"),
        ("1550859921615324669", "20176503191581999", "0.001"),
        ("15508599216153246690", "197562081339725637", "0.01"),
        ("77542996080766233453", "903343924658862851", "0.05"),
        ("155085992161532466907", "1629583266247721677", "0.1"),
        ("387714980403831167267", "3126129796109520355", "0.25"),
        ("775429960807662334535", "4448793926771016987", "0.5"),
        ("1240687937292259735256", "5233033205196985311", "0.8"),
        ("1473316925534558435617", "5473178570286745061", "0.95"),
        ("1550859921615324669071", "5539336923065416677", "1"),
    ];
    let (dir, rows) = pack(&mixed_a_samples, "d", "a");
    let got = stable_mixed_candidate(&mixed_a, 3, 0, &dir, &rows)
        .unwrap()
        .expect("mixed");
    assert_eq!(got.0, 0);
    let choice = choose(&mixed_a, &dir, &rows)
        .unwrap()
        .expect("mixed choose");
    assert!(choice.score <= got.0);
    assert_named(&choice, "stable", 0);
    let (spiked_dir, spiked_rows) = spike(&mixed_a_samples, "d", "a");
    assert!(
        stable_mixed_candidate(&mixed_a, 3, 0, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );

    let mixed_b = curve_coins(
        &[
            "333055825",
            "332496894",
            "184585815059",
            "334947017487739244032",
        ],
        "500",
        "3000000",
    );
    let mixed_b_samples = [
        ("306540978878428895", "70409589", "0.0001"),
        ("3065409788784288955", "699751212", "0.001"),
        ("30654097887842889553", "6593472787", "0.01"),
        ("153270489439214447766", "26387745588", "0.05"),
        ("306540978878428895533", "42566087216", "0.1"),
        ("766352447196072238832", "68612592162", "0.25"),
        ("1532704894392144477665", "87977291934", "0.5"),
        ("2452327831027431164264", "99626097806", "0.8"),
        ("2912139299345074507563", "103529306006", "0.95"),
        ("3065409788784288955330", "104658453324", "1"),
    ];
    let (dir, rows) = pack(&mixed_b_samples, "d", "c");
    let got = stable_mixed_candidate(&mixed_b, 3, 2, &dir, &rows)
        .unwrap()
        .expect("mixed b");
    assert_eq!(got.0, 0);
    let choice = choose(&mixed_b, &dir, &rows)
        .unwrap()
        .expect("mixed choose b");
    assert!(choice.score <= got.0);
    assert_named(&choice, "stable", 0);
    let (spiked_dir, spiked_rows) = spike(&mixed_b_samples, "d", "c");
    assert!(
        stable_mixed_candidate(&mixed_b, 3, 2, &spiked_dir, &spiked_rows)
            .unwrap()
            .is_none()
    );
}
