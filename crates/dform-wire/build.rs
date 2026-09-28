//! Generates the provider protocol's messages from
//! `proto/dform/v1/provider.proto` (needs `protoc`, or `PROTOC` naming it).
//! The service (client and server) is `dform-grpc`'s.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/dform/v1/provider.proto");
    prost_build::Config::new()
        .btree_map(["."])
        .compile_protos(&["../../proto/dform/v1/provider.proto"], &["../../proto"])?;
    Ok(())
}
