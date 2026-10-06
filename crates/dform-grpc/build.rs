//! Generates the provider protocol's gRPC service (client and server) from
//! `proto/dform/v1/provider.proto` over `dform-wire`'s messages (needs
//! `protoc`, or `PROTOC` naming it). Beside it, additively, the host's
//! services (`proto/dform/host/v1/host.proto`, R-13b): `Host`, which dform
//! serves to a native provider, and `Manifest`, which a provider serves.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/dform/v1/provider.proto");
    tonic_build::configure()
        .extern_path(".dform.v1", "::dform_wire")
        .compile_protos(&["../../proto/dform/v1/provider.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/dform/host/v1/host.proto");
    tonic_build::configure()
        .compile_protos(&["../../proto/dform/host/v1/host.proto"], &["../../proto"])?;
    Ok(())
}
