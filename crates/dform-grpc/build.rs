//! Generates the provider protocol's gRPC service (client and server) from
//! `proto/dform/v1/provider.proto` over `dform-wire`'s messages (needs
//! `protoc`, or `PROTOC` naming it).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/dform/v1/provider.proto");
    tonic_build::configure()
        .extern_path(".dform.v1", "::dform_wire")
        .compile_protos(&["../../proto/dform/v1/provider.proto"], &["../../proto"])?;
    Ok(())
}
