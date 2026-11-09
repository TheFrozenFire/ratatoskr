#![no_main]

use libfuzzer_sys::fuzz_target;
use num_bigint::BigInt;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4_096 {
        return;
    }
    let split = data.len() / 2;
    let amount = BigInt::from_signed_bytes_be(&data[..split.min(32)]);
    venues::poke_formula(data, &amount);
});
