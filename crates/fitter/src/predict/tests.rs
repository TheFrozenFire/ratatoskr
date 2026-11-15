use super::book::*;
use super::crypto::*;
use super::product::*;
use super::share::*;
use super::spline::*;
use super::stable::*;
use super::weighted::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use std::sync::Arc;

fn ib(n: i128) -> BigInt {
    BigInt::from(n)
}

fn s(text: &str) -> BigInt {
    parse_dec(text)
}

fn pair(a: i128, b: i128) -> (BigInt, BigInt) {
    (ib(a), ib(b))
}

#[test]
fn self_check_and_inverses() {
    assert_eq!(
        product_out(
            &ib(1_000),
            &ib(1_000),
            &ib(1_000),
            &ib(3_000),
            &ib(1_000_000)
        ),
        Some(ib(499))
    );
    assert_eq!(sqrt_at_tick(&BigInt::zero()), Some(Q96.clone()));
    let tick_one = sqrt_at_tick(&BigInt::one()).expect("tick 1");
    assert_eq!(tick_one, s("79232123823359799118286999568"));
    let balanced = vec![pow10(18), pow10(18)];
    let d = stable_d(&balanced, &ib(100)).expect("d");
    assert!((d - BigInt::from(2) * pow10(18)).abs() <= BigInt::from(2));
    assert_eq!(
        hyperbola_out(&ib(2), &ib(3), &ib(6), &ib(6), &ib(3)),
        Some(ib(4))
    );
    assert_eq!(
        hyperbola_in(&ib(2), &ib(3), &ib(6), &ib(6), &ib(4)),
        Some(ib(3))
    );
    let knots = [pair(100, 1100), pair(400, 1400)];
    let line = chord_quote(&knots).expect("line");
    assert_eq!(line(ib(200)), Some(ib(1200)));
    assert_eq!(line(ib(100)), Some(ib(1100)));
    assert_eq!(line(ib(400)), Some(ib(1400)));
    let backward = chord_in(&knots).expect("chord in");
    assert_eq!(backward(ib(1200)), Some(ib(200)));
    let curve = cubic_quote(&[pair(100, 1100), pair(400, 1400), pair(800, 1500)]).expect("curve");
    assert_eq!(curve(ib(100)), Some(ib(1100)));
    assert_eq!(curve(ib(400)), Some(ib(1400)));
    assert_eq!(curve(ib(800)), Some(ib(1500)));
    let repaired = residual_quote(&[pair(2, 3), pair(3, 5), pair(6, 6)]).expect("repaired");
    assert_eq!(repaired(ib(2)), Some(ib(3)));
    assert_eq!(repaired(ib(3)), Some(ib(5)));
    assert_eq!(repaired(ib(6)), Some(ib(6)));
    let base: Predict = Arc::new(Some);
    let lifted = added_residual(base, &[pair(100, 600), pair(400, 900)], &BigRational::one())
        .expect("lifted");
    assert_eq!(lifted(ib(100)), Some(ib(600)));
    assert_eq!(lifted(ib(200)), Some(ib(700)));
    assert_eq!(lifted(ib(400)), Some(ib(900)));
    let ann = ib(400_000);
    let gamma = s("145000000000000");
    let d_crypto = s("863257940499748221795");
    let balances = [s("535404253762953867722"), s("17320665696279630959")];
    let scale = crypto_scale(
        &ann,
        &gamma,
        &d_crypto,
        &balances[0],
        &balances[1],
        &ib(1),
        &ib(1),
    )
    .expect("scale");
    let pred = crypto2_out(
        &balances,
        &[ib(1), ib(1)],
        &scale,
        &ann,
        &gamma,
        &d_crypto,
        0,
        1,
        &s("53540425376295386"),
        &ib(26_000_000),
        &ib(45_000_000),
        &s("230000000000000"),
    )
    .expect("crypto");
    let label = s("1738837333048969");
    assert!((&pred - &label).abs() <= ib(100));
    assert_eq!(
        flat_y(
            &ib(600_000),
            &[s("412253191892625532828380"), s("471799111780361957599989")],
            &s("884018281329555456735930"),
            1,
        ),
        Some(s("471798109549896825012548"))
    );
    assert_eq!(
        dynamic_fee(&ib(1), &ib(1), &ib(10), &ib(500), &ib(100)),
        ib(10)
    );
    assert_eq!(
        dynamic_fee(&ib(1), &ib(100), &ib(10), &ib(500), &ib(100)),
        ib(43)
    );
    // weighted_out(1000, 1000, 2, 1, 100, 0)
    assert_eq!(
        weighted_out(&ib(1000), &ib(1000), &ib(2), &ib(1), &ib(100), &ib(0)),
        Some(ib(173))
    );
    // weighted_out(10**18, 2*10**18, 3, 2, 10**17, 0)
    assert_eq!(
        weighted_out(
            &pow10(18),
            &(ib(2) * pow10(18)),
            &ib(3),
            &ib(2),
            &pow10(17),
            &ib(0)
        ),
        Some(s("266431655917104881"))
    );
}

#[test]
fn tricrypto_get_dy_matches_chain() {
    let balances = vec![
        s("7949965799"),
        s("4113599969321168658"),
        s("848042467675018410911"),
    ];
    let precs = vec![pow10(12), ib(1), ib(1)];
    let scales = vec![s("1928615248180488065580"), s("9373305418011774631")];
    let gamma = s("1300000000000");
    let d = s("23832478032965456732327");
    let dx = s("1000000000000000");
    let mid = s("2999999");
    let out_fee = s("80000000");
    let fee_gamma = s("350000000000000");
    let got = crypto3_out(
        &balances,
        &precs,
        &scales,
        &s("2700000"),
        &gamma,
        &d,
        1,
        0,
        &dx,
        &mid,
        &out_fee,
        &fee_gamma,
    );
    assert_eq!(got, Some(s("1928233")));
}
