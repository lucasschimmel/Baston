# baston-fuzz

cargo-fuzz targets for the untrusted parse entry points of `baston-protocol`
(the same six functions covered by the deterministic sweep test in
`crates/baston-protocol/src/rage/packet.rs`):

- `decode_incoming`, `decode_downlink` (packet framing)
- `parse_nack`, `parse_ack` (reliability)
- `parse_object_ids`
- `lz4dict_decompress`

Contract under fuzzing: return `None`/empty — never panic, never allocate
unbounded.

## Running

Requires nightly + `cargo-fuzz` (libFuzzer). libFuzzer support on Windows is
spotty — the canonical runners are the scheduled GitHub workflow
(`.github/workflows/fuzz.yml`) or a Linux/WSL shell:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run decode_incoming -- -max_total_time=60
```

The crate is excluded from the root workspace so the stable CI never builds
it.
