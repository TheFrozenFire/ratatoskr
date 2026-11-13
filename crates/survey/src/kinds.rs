use num_bigint::{BigInt, Sign};
use searcher::Curve;
use venues::kind_name;

pub(crate) fn curve_label(curve: &Curve) -> String {
    match curve {
        Curve::Wrap => "wrap".into(),
        Curve::Conduit(segments) => format!(
            "conduit segments {} kinds {}",
            segments.len(),
            kind_list(curve_kinds(curve))
        ),
    }
}

pub(crate) fn curve_kinds(curve: &Curve) -> u32 {
    match curve {
        Curve::Wrap => 0,
        Curve::Conduit(segments) => segments
            .iter()
            .fold(0_u32, |bits, segment| bits | kind_bit(segment.action.kind)),
    }
}

pub(crate) fn kind_bit(kind: u8) -> u32 {
    if kind == 0 || kind >= 32 {
        return 0;
    }
    1_u32 << (kind - 1)
}

pub(crate) fn only_kind(bits: u32) -> Option<u8> {
    if bits.count_ones() == 1 {
        Some(bits.trailing_zeros() as u8 + 1)
    } else {
        None
    }
}

pub(crate) fn kind_list(bits: u32) -> String {
    if bits == 0 {
        return "-".into();
    }
    (1..32_u8)
        .filter(|kind| bits & kind_bit(*kind) != 0)
        .map(kind_name)
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn as_u128(value: &BigInt) -> u128 {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 16 {
        return if sign == Sign::Minus { 0 } else { u128::MAX };
    }
    let mut buf = [0_u8; 16];
    buf[16 - bytes.len()..].copy_from_slice(&bytes);
    u128::from_be_bytes(buf)
}
