use alloy_primitives::{Uint, U256};

pub(super) type U512 = Uint<512, 8>;

pub(super) fn mul_div(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    if let (Ok(left), Ok(right)) = (u128::try_from(a), u128::try_from(b)) {
        if let Some(product) = left.checked_mul(right) {
            // The product fits in 128 bits. A wider denominator yields zero.
            return match u128::try_from(denom) {
                Ok(den) => Ok(U256::from(product / den)),
                Err(_) => Ok(U256::ZERO),
            };
        }
        // The product of two 128-bit factors still fits in 256 bits.
        let product = U256::from(left) * U256::from(right);
        return Ok(product / denom);
    }
    let (q, _) = widening(a, b).div_rem(u512(denom));
    narrow(q).ok_or_else(|| "overflow".to_string())
}

pub(super) fn mul_div_up(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    mul_div_up_512(a, b, u512(denom))
}

pub(super) fn mul_div_up_512(a: U256, b: U256, denom: U512) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (mut q, r) = widening(a, b).div_rem(denom);
    if !r.is_zero() {
        q += U512::from(1u8);
    }
    narrow(q).ok_or_else(|| "overflow".to_string())
}

pub(super) fn div_up(value: U256, denom: U256) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (q, r) = value.div_rem(denom);
    if r.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::from(1u8))
            .ok_or_else(|| "overflow".to_string())
    }
}

pub(super) fn widening(a: U256, b: U256) -> U512 {
    a.widening_mul(b)
}

pub(super) fn mul_shift_128(a: U256, b: U256) -> U256 {
    narrow(widening(a, b) >> 128).expect("tick factor")
}

pub(super) fn u512(value: U256) -> U512 {
    let limbs = value.as_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

pub(super) fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

pub(super) fn q96() -> U256 {
    U256::from(1u8) << 96
}

pub(crate) fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}
