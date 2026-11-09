#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1_536 {
        return;
    }
    venues::poke_choose(data);
});
