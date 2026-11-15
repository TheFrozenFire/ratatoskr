//! Turn a stored formula into the quote the search scored.

use crate::formula::Formula;
use crate::predict::{
    added_residual, bin_out, chord_quote, concentrated_out, crypto2_out, crypto3_out, cubic_quote,
    flat_pair_out, hyperbola_out, product_out, residual_quote, stable_swap_out, weighted_out,
    Predict,
};
use crate::series::*;
use crate::units::*;
use bigdecimal::BigDecimal;
use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::{One, Zero};
use std::sync::Arc;

pub fn eval_formula(formula: &Formula, amount: &BigInt) -> Option<BigInt> {
    formula.predict()(amount.clone())
}

impl Formula {
    pub(crate) fn predict(&self) -> Predict {
        match self {
            Self::Hyperbola { x1, y1, x2, y2 } => {
                let (x1, y1, x2, y2) = (x1.clone(), y1.clone(), x2.clone(), y2.clone());
                Arc::new(move |dx: BigInt| hyperbola_out(&x1, &y1, &x2, &y2, &dx))
            }
            Self::Stable {
                xp,
                amp,
                i,
                j,
                fee,
                denom,
                offpeg,
                rate_i,
                rate_j,
                prec_i,
                prec_j,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let denom = denom.clone();
                let offpeg = offpeg.clone();
                let rate_i = rate_i.clone();
                let rate_j = rate_j.clone();
                let prec_i = prec_i.clone();
                let prec_j = prec_j.clone();
                let i_bi = bi(*i);
                let j_bi = bi(*j);
                Arc::new(move |dx: BigInt| {
                    let dxp = dxp_of(&dx, &rate_i, &prec_i);
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        i_bi.clone(),
                        j_bi.clone(),
                        dxp,
                        fee.clone(),
                        denom.clone(),
                        false,
                        offpeg.clone(),
                    )?;
                    Some(unscale_out(&dy, &rate_j, &prec_j))
                })
            }
            Self::Crypto2 {
                balances,
                precs,
                scale,
                ann,
                gamma,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scale = scale.clone();
                let ann = ann.clone();
                let gamma = gamma.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                Arc::new(move |dx: BigInt| {
                    crypto2_out(
                        &balances, &precs, &scale, &ann, &gamma, &d, i_idx, j_idx, &dx, &mid,
                        &out_fee, &fee_gamma,
                    )
                })
            }
            Self::FlatPair {
                balances,
                precs,
                scale,
                amp,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
                flat_fee,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scale = scale.clone();
                let amp = amp.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                let flat_fee = *flat_fee;
                Arc::new(move |dx: BigInt| {
                    flat_pair_out(
                        &balances, &precs, &scale, &amp, &d, i_idx, j_idx, &dx, &mid, &out_fee,
                        &fee_gamma, flat_fee,
                    )
                })
            }
            Self::Crypto3 {
                balances,
                precs,
                scales,
                ann,
                gamma,
                d,
                i,
                j,
                mid,
                out_fee,
                fee_gamma,
            } => {
                let balances = balances.clone();
                let precs = precs.clone();
                let scales = scales.clone();
                let ann = ann.clone();
                let gamma = gamma.clone();
                let d = d.clone();
                let mid = mid.clone();
                let out_fee = out_fee.clone();
                let fee_gamma = fee_gamma.clone();
                let i_idx = *i;
                let j_idx = *j;
                Arc::new(move |dx: BigInt| {
                    crypto3_out(
                        &balances, &precs, &scales, &ann, &gamma, &d, i_idx, j_idx, &dx, &mid,
                        &out_fee, &fee_gamma,
                    )
                })
            }
            Self::StableScale {
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                which,
                ppm,
                denom,
                fee_on_input,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppm = ppm.clone();
                let denom = denom.clone();
                let Some(i) = rate_index(rates.len(), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len(), *j) else {
                    return Arc::new(|_| None);
                };
                let which = *which as usize;
                let fee_on_input = *fee_on_input;
                Arc::new(move |dx: BigInt| {
                    if fee_on_input {
                        let paid = if which == i {
                            fd(&(&dx * &ppm), &bi(10_000))
                        } else {
                            dx
                        };
                        let mut dy = stable_swap_out(
                            &xp,
                            amp.clone(),
                            bi(i as i64),
                            bi(j as i64),
                            paid,
                            fee.clone(),
                            denom.clone(),
                            true,
                            BigInt::zero(),
                        )?;
                        if which == j {
                            dy = fd(&(&dy * &bi(10_000)), &ppm);
                        }
                        return Some(dy);
                    }
                    let mut paid = if rates[i].is_zero() {
                        dx
                    } else {
                        fd(&(&dx * &rates[i]), &wad())
                    };
                    if which == i {
                        paid = fd(&(&paid * &ppm), &bi(10_000));
                    }
                    let mut dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i as i64),
                        bi(j as i64),
                        paid,
                        fee.clone(),
                        denom.clone(),
                        false,
                        BigInt::zero(),
                    )?;
                    if which == j {
                        dy = fd(&(&dy * &bi(10_000)), &ppm);
                    }
                    if rates[j].is_zero() {
                        Some(dy)
                    } else {
                        Some(fd(&(&dy * &wad()), &rates[j]))
                    }
                })
            }
            Self::StableOutput {
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                ppm,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppm = ppm.clone();
                let Some(i) = rate_index(rates.len(), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len(), *j) else {
                    return Arc::new(|_| None);
                };
                Arc::new(move |dx: BigInt| {
                    let dy = stable_flat_quote(&xp, &amp, i, j, &fee, &rates, &dx)?;
                    Some(fd(&(&dy * &ppm), &bi(10_000)))
                })
            }
            Self::StablePpms {
                xp,
                amp,
                i,
                j,
                fee,
                rates,
                ppms,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let rates = rates.clone();
                let ppms = ppms.clone();
                let Some(i) = rate_index(rates.len().min(ppms.len()), *i) else {
                    return Arc::new(|_| None);
                };
                let Some(j) = rate_index(rates.len().min(ppms.len()), *j) else {
                    return Arc::new(|_| None);
                };
                Arc::new(move |dx: BigInt| {
                    let mut paid = if rates[i].is_zero() {
                        dx
                    } else {
                        fd(&(&dx * &rates[i]), &wad())
                    };
                    paid = fd(&(&paid * &ppms[i]), &bi(10_000));
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i as i64),
                        bi(j as i64),
                        paid,
                        fee.clone(),
                        fee_denom(),
                        false,
                        BigInt::zero(),
                    )?;
                    let dy = fd(&(&dy * bi(10_000)), &ppms[j]);
                    if rates[j].is_zero() {
                        Some(dy)
                    } else {
                        Some(fd(&(&dy * &wad()), &rates[j]))
                    }
                })
            }
            Self::StableRefine {
                xp,
                amp,
                i,
                j,
                fee,
                offpeg,
                rate_i,
                prec_i,
                ppm_i,
                ppm_j,
                rate_j,
                prec_j,
                out_ppm,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let offpeg = offpeg.clone();
                let rate_i = rate_i.clone();
                let prec_i = prec_i.clone();
                let ppm_i = ppm_i.clone();
                let ppm_j = ppm_j.clone();
                let rate_j = rate_j.clone();
                let prec_j = prec_j.clone();
                let out_ppm = out_ppm.clone();
                let i = *i;
                let j = *j;
                Arc::new(move |dx: BigInt| {
                    let paid = if rate_i.is_zero() {
                        fd(&(&dx * &prec_i * &ppm_i), &bi(10_000))
                    } else {
                        fd(
                            &(fd(&(&dx * &rate_i), &wad()) * &prec_i * &ppm_i),
                            &bi(10_000),
                        )
                    };
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i),
                        bi(j),
                        paid,
                        fee.clone(),
                        fee_denom(),
                        false,
                        offpeg.clone(),
                    )?;
                    let scaled = fd(&fd(&(&dy * bi(10_000)), &ppm_j), &prec_j);
                    let scaled = fd(&(&scaled * &out_ppm), &bi(10_000));
                    if rate_j.is_zero() {
                        Some(scaled)
                    } else {
                        Some(fd(&(&scaled * &wad()), &rate_j))
                    }
                })
            }
            Self::Product {
                reserve_in,
                reserve_out,
                fee,
                denom,
            } => {
                let reserve_in = reserve_in.clone();
                let reserve_out = reserve_out.clone();
                let fee = fee.clone();
                let denom = denom.clone();
                Arc::new(move |dx: BigInt| {
                    product_out(&reserve_in, &reserve_out, &dx, &fee, &denom)
                })
            }
            Self::Weighted {
                balance_in,
                balance_out,
                weight_in,
                weight_out,
                fee,
            } => {
                let balance_in = balance_in.clone();
                let balance_out = balance_out.clone();
                let weight_in = weight_in.clone();
                let weight_out = weight_out.clone();
                let fee = fee.clone();
                Arc::new(move |dx: BigInt| {
                    weighted_out(
                        &balance_in,
                        &balance_out,
                        &weight_in,
                        &weight_out,
                        &dx,
                        &fee,
                    )
                })
            }
            Self::BalancerStable {
                xp,
                amp,
                i,
                j,
                fee,
                prec_i,
                prec_j,
            } => {
                let xp = xp.clone();
                let amp = amp.clone();
                let fee = fee.clone();
                let prec_i = prec_i.clone();
                let prec_j = prec_j.clone();
                let i = *i;
                let j = *j;
                Arc::new(move |dx: BigInt| {
                    let dy = stable_swap_out(
                        &xp,
                        amp.clone(),
                        bi(i),
                        bi(j),
                        &dx * &prec_i,
                        fee.clone(),
                        wad(),
                        true,
                        BigInt::zero(),
                    )?;
                    Some(fd(&dy, &prec_j))
                })
            }
            Self::Ladder {
                sqrt_price,
                liquidity,
                tick,
                fee,
                ticks,
                zero_for_one,
            } => {
                let sqrt_price = sqrt_price.clone();
                let liquidity = liquidity.clone();
                let tick = tick.clone();
                let fee = fee.clone();
                let ticks = ticks.clone();
                let zero_for_one = *zero_for_one;
                Arc::new(move |dx: BigInt| {
                    concentrated_out(
                        &sqrt_price,
                        &liquidity,
                        &tick,
                        &fee,
                        &ticks,
                        zero_for_one,
                        &dx,
                    )
                })
            }
            Self::Bins {
                reference,
                distance,
                length,
                position,
                selling0,
                scaled_out,
                scale_out,
            } => {
                let reference = reference.clone();
                let distance = distance.clone();
                let length = length.clone();
                let position = position.clone();
                let selling0 = *selling0;
                let scaled_out = scaled_out.clone();
                let scale_out = scale_out.clone();
                Arc::new(move |dx: BigInt| {
                    bin_out(
                        &reference,
                        &distance,
                        &length,
                        &position,
                        &dx,
                        selling0,
                        &scaled_out,
                        &scale_out,
                    )
                })
            }
            Self::Chord { knots } => knot_predict(chord_quote(knots)),
            Self::Cubic { knots } => knot_predict(cubic_quote(knots)),
            Self::HyperResidual { knots } => knot_predict(residual_quote(knots)),
            Self::Added { base, knots, alpha } => {
                let built = added_residual(base.predict(), knots, alpha);
                knot_predict(built)
            }
            Self::ScaledResidual { knots, ppm } => {
                let Some(base) = residual_quote(knots) else {
                    return Arc::new(|_| None);
                };
                let ppm = ppm.clone();
                Arc::new(move |amount: BigInt| {
                    let value = base(amount)?;
                    Some(fd(&(&value * &ppm), &bi(10_000)))
                })
            }
            Self::Fillet { slope, cap, xa, xb } => {
                fillet_quote(slope.clone(), cap.clone(), xa.clone(), xb.clone())
            }
            Self::Logistic { slope, rate, knee } => logistic_quote(slope, rate, knee),
            Self::Scaled { base, ppm } => scale_out(base.predict(), *ppm),
            Self::Wave {
                base,
                cap,
                x_last,
                a_ppm,
                b_ppm,
            } => wave_quote(base.predict(), cap, x_last, *a_ppm, *b_ppm),
            Self::Identity => Arc::new(Some),
            Self::Lookup { pairs } => {
                let pairs = pairs.clone();
                Arc::new(move |dx: BigInt| {
                    pairs
                        .iter()
                        .find(|(amount, _)| amount == &dx)
                        .map(|(_, out)| out.clone())
                })
            }
        }
    }
}

pub(crate) fn knot_predict(predict: Option<Predict>) -> Predict {
    predict.unwrap_or_else(|| Arc::new(|_| None))
}

pub(crate) fn dxp_of(dx: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        dx * prec
    } else {
        fd(&(dx * rate), &wad()) * prec
    }
}

pub(crate) fn unscale_out(dy: &BigInt, rate: &BigInt, prec: &BigInt) -> BigInt {
    if rate.is_zero() {
        fd(dy, prec)
    } else {
        fd(&fd(&(dy * &wad()), rate), prec)
    }
}

pub(crate) fn rate_index(len: usize, index: i64) -> Option<usize> {
    usize::try_from(index).ok().filter(|index| *index < len)
}

pub(crate) fn stable_flat_quote(
    xp: &[BigInt],
    amp: &BigInt,
    i: usize,
    j: usize,
    fee: &BigInt,
    rates: &[BigInt],
    dx: &BigInt,
) -> Option<BigInt> {
    let rate_i = rates.get(i)?;
    let rate_j = rates.get(j)?;
    let paid = if rate_i.is_zero() {
        dx.clone()
    } else {
        fd(&(dx * rate_i), &wad())
    };
    let dy = stable_swap_out(
        xp,
        amp.clone(),
        bi(i as i64),
        bi(j as i64),
        paid,
        fee.clone(),
        fee_denom(),
        false,
        BigInt::zero(),
    )?;
    if rate_j.is_zero() {
        Some(dy)
    } else {
        Some(fd(&(&dy * &wad()), rate_j))
    }
}

pub(crate) fn fillet_quote(
    slope: BigRational,
    cap: BigRational,
    xa: BigRational,
    xb: BigRational,
) -> Predict {
    Arc::new(move |x: BigInt| {
        let fx = BigRational::from_integer(x);
        let y = if xa <= BigRational::zero() || fx <= xa || xb <= xa {
            let mut y = &slope * &fx;
            if xb <= xa && y > cap {
                y = cap.clone();
            }
            y
        } else if fx >= xb {
            cap.clone()
        } else {
            let u = (&fx - &xa) / (&xb - &xa);
            let one = BigRational::one();
            let two = BigRational::from_integer(bi(2));
            let three = BigRational::from_integer(bi(3));
            let one_u = &one - &u;
            let h00 = (&one + &two * &u) * &one_u * &one_u;
            let h10 = &u * &one_u * &one_u;
            let h01 = &u * &u * (&three - &two * &u);
            h00 * &slope * &xa + h10 * (&xb - &xa) * &slope + h01 * &cap
        };
        if y <= BigRational::zero() {
            Some(BigInt::zero())
        } else {
            Some(trunc_rat(&y))
        }
    })
}

pub(crate) fn logistic_quote(
    slope: &BigRational,
    rate: &BigRational,
    knee: &BigRational,
) -> Predict {
    let s = rat_dec(slope);
    let k = rat_dec(rate);
    let x0 = rat_dec(knee);
    if k.sign() != Sign::Plus || s.sign() != Sign::Plus || x0.sign() != Sign::Plus {
        return Arc::new(|_| Some(BigInt::zero()));
    }
    // The opening height and the slope/rate ratio do not depend on the input.
    let numer = ln1exp(&he40(&k * &x0));
    let slope_over_rate = he_div_at(&s, &k, 40);
    Arc::new(move |x: BigInt| {
        let x = BigDecimal::from(x);
        let denom = ln1exp(&he40(&(-&k) * he40(&x - &x0)));
        let y = he40(&slope_over_rate * he40(&numer - &denom));
        if y.sign() != Sign::Plus {
            Some(BigInt::zero())
        } else {
            Some(trunc_dec(&y))
        }
    })
}

pub(crate) fn scale_out(base: Predict, ppm: i64) -> Predict {
    let ppm = bi(ppm);
    Arc::new(move |amount: BigInt| {
        let value = base(amount).unwrap_or_else(BigInt::zero);
        Some(fd(&(&value * &ppm), &bi(10_000)))
    })
}

pub(crate) fn wave_quote(
    base: Predict,
    cap: &BigInt,
    x_last: &BigInt,
    a_ppm: i64,
    b_ppm: i64,
) -> Predict {
    let cap = cap.clone();
    let x_last = x_last.clone();
    let a_ppm = bi(a_ppm);
    let b_ppm = bi(b_ppm);
    Arc::new(move |amount: BigInt| {
        let value = base(amount.clone()).unwrap_or_else(BigInt::zero);
        if amount <= BigInt::zero() || amount >= x_last || (a_ppm.is_zero() && b_ppm.is_zero()) {
            return Some(value);
        }
        let span = &amount * (&x_last - &amount);
        let mut corr = fd(&(&cap * &a_ppm * &span), &(&x_last * &x_last * bi(10_000)));
        corr += fd(
            &(&cap * &b_ppm * &span * (bi(2) * &amount - &x_last)),
            &(&x_last * &x_last * &x_last * bi(10_000)),
        );
        Some(value + corr)
    })
}
