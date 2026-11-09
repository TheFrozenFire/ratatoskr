//! Closed form stored on a fitted curve.
//!
//! A search returns one of these instead of a closure. Evaluation rebuilds the
//! same quote the search scored, including off the sample grid. The bytes are
//! the tag-3 payload's direction entry.

use alloy_primitives::Address;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Formula {
    Hyperbola {
        x1: BigInt,
        y1: BigInt,
        x2: BigInt,
        y2: BigInt,
    },
    Stable {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        denom: BigInt,
        offpeg: BigInt,
        rate_i: BigInt,
        rate_j: BigInt,
        prec_i: BigInt,
        prec_j: BigInt,
    },
    Crypto2 {
        balances: Vec<BigInt>,
        precs: Vec<BigInt>,
        scale: BigInt,
        ann: BigInt,
        gamma: BigInt,
        d: BigInt,
        i: i64,
        j: i64,
        mid: BigInt,
        out_fee: BigInt,
        fee_gamma: BigInt,
    },
    FlatPair {
        balances: Vec<BigInt>,
        precs: Vec<BigInt>,
        scale: BigInt,
        amp: BigInt,
        d: BigInt,
        i: i64,
        j: i64,
        mid: BigInt,
        out_fee: BigInt,
        fee_gamma: BigInt,
        flat_fee: bool,
    },
    Crypto3 {
        balances: Vec<BigInt>,
        precs: Vec<BigInt>,
        scales: Vec<BigInt>,
        ann: BigInt,
        gamma: BigInt,
        d: BigInt,
        i: i64,
        j: i64,
        mid: BigInt,
        out_fee: BigInt,
        fee_gamma: BigInt,
    },
    StableScale {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        rates: Vec<BigInt>,
        which: u32,
        ppm: BigInt,
        denom: BigInt,
        fee_on_input: bool,
    },
    StableOutput {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        rates: Vec<BigInt>,
        ppm: BigInt,
    },
    StablePpms {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        rates: Vec<BigInt>,
        ppms: Vec<BigInt>,
    },
    StableRefine {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        offpeg: BigInt,
        rate_i: BigInt,
        prec_i: BigInt,
        ppm_i: BigInt,
        ppm_j: BigInt,
        rate_j: BigInt,
        prec_j: BigInt,
        out_ppm: BigInt,
    },
    Product {
        reserve_in: BigInt,
        reserve_out: BigInt,
        fee: BigInt,
        denom: BigInt,
    },
    Weighted {
        balance_in: BigInt,
        balance_out: BigInt,
        weight_in: BigInt,
        weight_out: BigInt,
        fee: BigInt,
    },
    BalancerStable {
        xp: Vec<BigInt>,
        amp: BigInt,
        i: i64,
        j: i64,
        fee: BigInt,
        prec_i: BigInt,
        prec_j: BigInt,
    },
    Ladder {
        sqrt_price: BigInt,
        liquidity: BigInt,
        tick: BigInt,
        fee: BigInt,
        ticks: Vec<(BigInt, BigInt)>,
        zero_for_one: bool,
    },
    Bins {
        reference: BigInt,
        distance: BigInt,
        length: BigInt,
        position: BigInt,
        selling0: bool,
        scaled_out: BigInt,
        scale_out: BigInt,
    },
    Chord {
        knots: Vec<(BigInt, BigInt)>,
    },
    Cubic {
        knots: Vec<(BigInt, BigInt)>,
    },
    HyperResidual {
        knots: Vec<(BigInt, BigInt)>,
    },
    Added {
        base: Box<Formula>,
        knots: Vec<(BigInt, BigInt)>,
        alpha: BigRational,
    },
    ScaledResidual {
        knots: Vec<(BigInt, BigInt)>,
        ppm: BigInt,
    },
    Fillet {
        slope: BigRational,
        cap: BigRational,
        xa: BigRational,
        xb: BigRational,
    },
    Logistic {
        slope: BigRational,
        rate: BigRational,
        knee: BigRational,
    },
    Scaled {
        base: Box<Formula>,
        ppm: i64,
    },
    Wave {
        base: Box<Formula>,
        cap: BigInt,
        x_last: BigInt,
        a_ppm: i64,
        b_ppm: i64,
    },
    /// Returns the input. Used when a residual is fit on an identity base.
    Identity,
    /// Exact sample lookup. Missing inputs are not a quote.
    Lookup {
        pairs: Vec<(BigInt, BigInt)>,
    },
}

pub(crate) struct StoredFit {
    pub token_in: Address,
    pub token_out: Address,
    pub score: i64,
    pub name: String,
    pub formula: Formula,
}

pub(crate) struct FitPayload {
    pub fallback: Vec<u8>,
    pub directions: Vec<StoredFit>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(len).ok_or_else(|| "short".to_string())?;
        if end > self.bytes.len() {
            return Err("short".to_string());
        }
        let start = self.at;
        self.at = end;
        Ok(&self.bytes[start..end])
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn done(&self) -> Result<(), String> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing".to_string())
        }
    }
}

fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_bool(out: &mut Vec<u8>, value: bool) {
    push_u8(out, u8::from(value));
}

fn push_bi(out: &mut Vec<u8>, value: &BigInt) {
    let bytes = value.to_signed_bytes_be();
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(&bytes[..bytes.len().min(u32::MAX as usize)]);
}

fn push_bis(out: &mut Vec<u8>, values: &[BigInt]) {
    push_u32(out, u32::try_from(values.len()).unwrap_or(u32::MAX));
    for value in values.iter().take(u32::MAX as usize) {
        push_bi(out, value);
    }
}

fn push_pairs(out: &mut Vec<u8>, values: &[(BigInt, BigInt)]) {
    push_u32(out, u32::try_from(values.len()).unwrap_or(u32::MAX));
    for (left, right) in values.iter().take(u32::MAX as usize) {
        push_bi(out, left);
        push_bi(out, right);
    }
}

fn push_rat(out: &mut Vec<u8>, value: &BigRational) {
    push_bi(out, value.numer());
    push_bi(out, value.denom());
}

fn read_bi(cursor: &mut Cursor<'_>) -> Result<BigInt, String> {
    let len = cursor.u32()? as usize;
    let bytes = cursor.take(len)?;
    if bytes.is_empty() {
        Ok(BigInt::zero())
    } else {
        Ok(BigInt::from_signed_bytes_be(bytes))
    }
}

fn read_bis(cursor: &mut Cursor<'_>) -> Result<Vec<BigInt>, String> {
    let n = cursor.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_bi(cursor)?);
    }
    Ok(out)
}

fn read_pairs(cursor: &mut Cursor<'_>) -> Result<Vec<(BigInt, BigInt)>, String> {
    let n = cursor.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push((read_bi(cursor)?, read_bi(cursor)?));
    }
    Ok(out)
}

fn read_rat(cursor: &mut Cursor<'_>) -> Result<BigRational, String> {
    let numer = read_bi(cursor)?;
    let denom = read_bi(cursor)?;
    if denom.is_zero() {
        return Err("rational".to_string());
    }
    Ok(BigRational::new(numer, denom))
}

fn read_bool(cursor: &mut Cursor<'_>) -> Result<bool, String> {
    Ok(cursor.u8()? != 0)
}

impl Formula {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hyperbola { x1, y1, x2, y2 } => {
                push_u8(out, 1);
                push_bi(out, x1);
                push_bi(out, y1);
                push_bi(out, x2);
                push_bi(out, y2);
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
                push_u8(out, 2);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bi(out, denom);
                push_bi(out, offpeg);
                push_bi(out, rate_i);
                push_bi(out, rate_j);
                push_bi(out, prec_i);
                push_bi(out, prec_j);
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
                push_u8(out, 3);
                push_bis(out, balances);
                push_bis(out, precs);
                push_bi(out, scale);
                push_bi(out, ann);
                push_bi(out, gamma);
                push_bi(out, d);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, mid);
                push_bi(out, out_fee);
                push_bi(out, fee_gamma);
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
                push_u8(out, 4);
                push_bis(out, balances);
                push_bis(out, precs);
                push_bi(out, scale);
                push_bi(out, amp);
                push_bi(out, d);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, mid);
                push_bi(out, out_fee);
                push_bi(out, fee_gamma);
                push_bool(out, *flat_fee);
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
                push_u8(out, 5);
                push_bis(out, balances);
                push_bis(out, precs);
                push_bis(out, scales);
                push_bi(out, ann);
                push_bi(out, gamma);
                push_bi(out, d);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, mid);
                push_bi(out, out_fee);
                push_bi(out, fee_gamma);
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
                push_u8(out, 6);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bis(out, rates);
                push_u32(out, *which);
                push_bi(out, ppm);
                push_bi(out, denom);
                push_bool(out, *fee_on_input);
            }
            Self::StableOutput { xp, amp, i, j, fee, rates, ppm } => {
                push_u8(out, 7);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bis(out, rates);
                push_bi(out, ppm);
            }
            Self::StablePpms { xp, amp, i, j, fee, rates, ppms } => {
                push_u8(out, 8);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bis(out, rates);
                push_bis(out, ppms);
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
                push_u8(out, 9);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bi(out, offpeg);
                push_bi(out, rate_i);
                push_bi(out, prec_i);
                push_bi(out, ppm_i);
                push_bi(out, ppm_j);
                push_bi(out, rate_j);
                push_bi(out, prec_j);
                push_bi(out, out_ppm);
            }
            Self::Product { reserve_in, reserve_out, fee, denom } => {
                push_u8(out, 10);
                push_bi(out, reserve_in);
                push_bi(out, reserve_out);
                push_bi(out, fee);
                push_bi(out, denom);
            }
            Self::Weighted {
                balance_in,
                balance_out,
                weight_in,
                weight_out,
                fee,
            } => {
                push_u8(out, 11);
                push_bi(out, balance_in);
                push_bi(out, balance_out);
                push_bi(out, weight_in);
                push_bi(out, weight_out);
                push_bi(out, fee);
            }
            Self::BalancerStable { xp, amp, i, j, fee, prec_i, prec_j } => {
                push_u8(out, 12);
                push_bis(out, xp);
                push_bi(out, amp);
                push_i64(out, *i);
                push_i64(out, *j);
                push_bi(out, fee);
                push_bi(out, prec_i);
                push_bi(out, prec_j);
            }
            Self::Ladder { sqrt_price, liquidity, tick, fee, ticks, zero_for_one } => {
                push_u8(out, 13);
                push_bi(out, sqrt_price);
                push_bi(out, liquidity);
                push_bi(out, tick);
                push_bi(out, fee);
                push_pairs(out, ticks);
                push_bool(out, *zero_for_one);
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
                push_u8(out, 14);
                push_bi(out, reference);
                push_bi(out, distance);
                push_bi(out, length);
                push_bi(out, position);
                push_bool(out, *selling0);
                push_bi(out, scaled_out);
                push_bi(out, scale_out);
            }
            Self::Chord { knots } => {
                push_u8(out, 15);
                push_pairs(out, knots);
            }
            Self::Cubic { knots } => {
                push_u8(out, 16);
                push_pairs(out, knots);
            }
            Self::HyperResidual { knots } => {
                push_u8(out, 17);
                push_pairs(out, knots);
            }
            Self::Added { base, knots, alpha } => {
                push_u8(out, 18);
                base.encode(out);
                push_pairs(out, knots);
                push_rat(out, alpha);
            }
            Self::ScaledResidual { knots, ppm } => {
                push_u8(out, 19);
                push_pairs(out, knots);
                push_bi(out, ppm);
            }
            Self::Fillet { slope, cap, xa, xb } => {
                push_u8(out, 20);
                push_rat(out, slope);
                push_rat(out, cap);
                push_rat(out, xa);
                push_rat(out, xb);
            }
            Self::Logistic { slope, rate, knee } => {
                push_u8(out, 21);
                push_rat(out, slope);
                push_rat(out, rate);
                push_rat(out, knee);
            }
            Self::Scaled { base, ppm } => {
                push_u8(out, 22);
                base.encode(out);
                push_i64(out, *ppm);
            }
            Self::Wave { base, cap, x_last, a_ppm, b_ppm } => {
                push_u8(out, 23);
                base.encode(out);
                push_bi(out, cap);
                push_bi(out, x_last);
                push_i64(out, *a_ppm);
                push_i64(out, *b_ppm);
            }
            Self::Identity => push_u8(out, 24),
            Self::Lookup { pairs } => {
                push_u8(out, 25);
                push_pairs(out, pairs);
            }
        }
    }

    fn decode_from(cursor: &mut Cursor<'_>) -> Result<Self, String> {
        Ok(match cursor.u8()? {
            1 => Self::Hyperbola {
                x1: read_bi(cursor)?,
                y1: read_bi(cursor)?,
                x2: read_bi(cursor)?,
                y2: read_bi(cursor)?,
            },
            2 => Self::Stable {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                denom: read_bi(cursor)?,
                offpeg: read_bi(cursor)?,
                rate_i: read_bi(cursor)?,
                rate_j: read_bi(cursor)?,
                prec_i: read_bi(cursor)?,
                prec_j: read_bi(cursor)?,
            },
            3 => Self::Crypto2 {
                balances: read_bis(cursor)?,
                precs: read_bis(cursor)?,
                scale: read_bi(cursor)?,
                ann: read_bi(cursor)?,
                gamma: read_bi(cursor)?,
                d: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                mid: read_bi(cursor)?,
                out_fee: read_bi(cursor)?,
                fee_gamma: read_bi(cursor)?,
            },
            4 => Self::FlatPair {
                balances: read_bis(cursor)?,
                precs: read_bis(cursor)?,
                scale: read_bi(cursor)?,
                amp: read_bi(cursor)?,
                d: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                mid: read_bi(cursor)?,
                out_fee: read_bi(cursor)?,
                fee_gamma: read_bi(cursor)?,
                flat_fee: read_bool(cursor)?,
            },
            5 => Self::Crypto3 {
                balances: read_bis(cursor)?,
                precs: read_bis(cursor)?,
                scales: read_bis(cursor)?,
                ann: read_bi(cursor)?,
                gamma: read_bi(cursor)?,
                d: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                mid: read_bi(cursor)?,
                out_fee: read_bi(cursor)?,
                fee_gamma: read_bi(cursor)?,
            },
            6 => Self::StableScale {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                rates: read_bis(cursor)?,
                which: cursor.u32()?,
                ppm: read_bi(cursor)?,
                denom: read_bi(cursor)?,
                fee_on_input: read_bool(cursor)?,
            },
            7 => Self::StableOutput {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                rates: read_bis(cursor)?,
                ppm: read_bi(cursor)?,
            },
            8 => Self::StablePpms {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                rates: read_bis(cursor)?,
                ppms: read_bis(cursor)?,
            },
            9 => Self::StableRefine {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                offpeg: read_bi(cursor)?,
                rate_i: read_bi(cursor)?,
                prec_i: read_bi(cursor)?,
                ppm_i: read_bi(cursor)?,
                ppm_j: read_bi(cursor)?,
                rate_j: read_bi(cursor)?,
                prec_j: read_bi(cursor)?,
                out_ppm: read_bi(cursor)?,
            },
            10 => Self::Product {
                reserve_in: read_bi(cursor)?,
                reserve_out: read_bi(cursor)?,
                fee: read_bi(cursor)?,
                denom: read_bi(cursor)?,
            },
            11 => Self::Weighted {
                balance_in: read_bi(cursor)?,
                balance_out: read_bi(cursor)?,
                weight_in: read_bi(cursor)?,
                weight_out: read_bi(cursor)?,
                fee: read_bi(cursor)?,
            },
            12 => Self::BalancerStable {
                xp: read_bis(cursor)?,
                amp: read_bi(cursor)?,
                i: cursor.i64()?,
                j: cursor.i64()?,
                fee: read_bi(cursor)?,
                prec_i: read_bi(cursor)?,
                prec_j: read_bi(cursor)?,
            },
            13 => Self::Ladder {
                sqrt_price: read_bi(cursor)?,
                liquidity: read_bi(cursor)?,
                tick: read_bi(cursor)?,
                fee: read_bi(cursor)?,
                ticks: read_pairs(cursor)?,
                zero_for_one: read_bool(cursor)?,
            },
            14 => Self::Bins {
                reference: read_bi(cursor)?,
                distance: read_bi(cursor)?,
                length: read_bi(cursor)?,
                position: read_bi(cursor)?,
                selling0: read_bool(cursor)?,
                scaled_out: read_bi(cursor)?,
                scale_out: read_bi(cursor)?,
            },
            15 => Self::Chord { knots: read_pairs(cursor)? },
            16 => Self::Cubic { knots: read_pairs(cursor)? },
            17 => Self::HyperResidual { knots: read_pairs(cursor)? },
            18 => Self::Added {
                base: Box::new(Self::decode_from(cursor)?),
                knots: read_pairs(cursor)?,
                alpha: read_rat(cursor)?,
            },
            19 => Self::ScaledResidual {
                knots: read_pairs(cursor)?,
                ppm: read_bi(cursor)?,
            },
            20 => Self::Fillet {
                slope: read_rat(cursor)?,
                cap: read_rat(cursor)?,
                xa: read_rat(cursor)?,
                xb: read_rat(cursor)?,
            },
            21 => Self::Logistic {
                slope: read_rat(cursor)?,
                rate: read_rat(cursor)?,
                knee: read_rat(cursor)?,
            },
            22 => Self::Scaled {
                base: Box::new(Self::decode_from(cursor)?),
                ppm: cursor.i64()?,
            },
            23 => Self::Wave {
                base: Box::new(Self::decode_from(cursor)?),
                cap: read_bi(cursor)?,
                x_last: read_bi(cursor)?,
                a_ppm: cursor.i64()?,
                b_ppm: cursor.i64()?,
            },
            24 => Self::Identity,
            25 => Self::Lookup { pairs: read_pairs(cursor)? },
            _ => return Err("formula".to_string()),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut cursor = Cursor::new(bytes);
        let formula = Self::decode_from(&mut cursor)?;
        cursor.done()?;
        Ok(formula)
    }
}

pub(crate) fn encode_fit(fallback: &[u8], directions: &[StoredFit]) -> Vec<u8> {
    let mut out = vec![3];
    push_u32(&mut out, u32::try_from(fallback.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(fallback);
    let count = u32::try_from(directions.len()).unwrap_or(u32::MAX);
    push_u32(&mut out, count);
    for direction in directions.iter().take(count as usize) {
        out.extend_from_slice(direction.token_in.as_slice());
        out.extend_from_slice(direction.token_out.as_slice());
        push_i64(&mut out, direction.score);
        let name = direction.name.as_bytes();
        let name_len = u8::try_from(name.len()).unwrap_or(u8::MAX);
        push_u8(&mut out, name_len);
        out.extend_from_slice(&name[..name_len as usize]);
        direction.formula.encode(&mut out);
    }
    out
}

pub(crate) fn decode_fit(payload: &[u8]) -> Result<FitPayload, String> {
    if payload.first() != Some(&3) {
        return Err("fit".to_string());
    }
    let mut cursor = Cursor::new(&payload[1..]);
    let fallback_len = cursor.u32()? as usize;
    let fallback = cursor.take(fallback_len)?.to_vec();
    let count = cursor.u32()? as usize;
    let mut directions = Vec::with_capacity(count);
    for _ in 0..count {
        let token_in = Address::from_slice(cursor.take(20)?);
        let token_out = Address::from_slice(cursor.take(20)?);
        let score = cursor.i64()?;
        let name_len = cursor.u8()? as usize;
        let name = std::str::from_utf8(cursor.take(name_len)?)
            .map_err(|_| "name".to_string())?
            .to_string();
        directions.push(StoredFit {
            token_in,
            token_out,
            score,
            name,
            formula: Formula::decode_from(&mut cursor)?,
        });
    }
    cursor.done()?;
    Ok(FitPayload { fallback, directions })
}
