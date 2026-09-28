//! Generates the provider protocol's messages and service from
//! `proto/dform/v1/provider.proto` (needs `protoc`, or `PROTOC` naming it).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/dform/v1/provider.proto");
    tonic_build::configure()
        .btree_map(["."])
        .compile_protos(&["proto/dform/v1/provider.proto"], &["proto"])?;
    Ok(())
}
