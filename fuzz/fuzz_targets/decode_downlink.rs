#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = baston_protocol::rage::packet::decode_downlink(data);
});
