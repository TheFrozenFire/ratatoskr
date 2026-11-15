//! Spot row bytes.

use alloy_primitives::Address;
use num_bigint::BigInt;
use store_model::Spot;

use super::{Error, VERSION};

pub(super) fn key_of(spot: &Spot) -> [u8; 40] {
    let mut key = [0_u8; 40];
    key[..20].copy_from_slice(spot.token.as_slice());
    key[20..].copy_from_slice(spot.numeraire.as_slice());
    key
}

pub(super) fn encode(spot: &Spot) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(VERSION);
    out.push(spot.hops);
    out.extend_from_slice(&spot.block.to_le_bytes());
    push_int(&mut out, &spot.size);
    push_int(&mut out, &spot.bought);
    push_int(&mut out, &spot.sold);
    out
}

pub(super) fn decode(key: &[u8], body: &[u8]) -> Result<Spot, Error> {
    if body.len() < 10 {
        return Err(Error::Store("spot row is short".into()));
    }
    if body[0] != VERSION {
        return Err(Error::Store(format!("spot version {} is not 1", body[0])));
    }
    let hops = body[1];
    let mut block = [0_u8; 8];
    block.copy_from_slice(&body[2..10]);
    let mut at = 10;
    let size = take_int(body, &mut at)?;
    let bought = take_int(body, &mut at)?;
    let sold = take_int(body, &mut at)?;
    if at != body.len() {
        return Err(Error::Store("spot row has trailing bytes".into()));
    }
    Ok(Spot {
        token: Address::from_slice(&key[..20]),
        numeraire: Address::from_slice(&key[20..]),
        size,
        bought,
        sold,
        hops,
        block: u64::from_le_bytes(block),
    })
}

fn push_int(out: &mut Vec<u8>, value: &BigInt) {
    let (_, bytes) = value.to_bytes_be();
    let len = u32::try_from(bytes.len()).expect("a spot integer fits in a u32 length");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes);
}

fn take_int(body: &[u8], at: &mut usize) -> Result<BigInt, Error> {
    if *at + 4 > body.len() {
        return Err(Error::Store("spot integer is short".into()));
    }
    let mut len_bytes = [0_u8; 4];
    len_bytes.copy_from_slice(&body[*at..*at + 4]);
    *at += 4;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if *at + len > body.len() {
        return Err(Error::Store("spot integer is short".into()));
    }
    let bytes = &body[*at..*at + len];
    *at += len;
    Ok(BigInt::from_bytes_be(num_bigint::Sign::Plus, bytes))
}
