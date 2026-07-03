fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/baston.proto");
    tonic_build::configure().compile_protos(&["proto/baston.proto"], &["proto"])?;
    Ok(())
}
