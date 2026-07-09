fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    // Plain protobuf messages (no gRPC service) — compile the Mumble control
    // wire format. proto2 optionals map to Option<T> in prost.
    tonic_build::configure()
        .build_client(false)
        .build_server(false)
        .compile_protos(&["proto/Mumble.proto"], &["proto"])?;
    Ok(())
}
