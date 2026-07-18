#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = baston_protocol::rage::lz4dict::decompress_using_dict(data, 16 * 1024);
});
