//! Generates the provider protocol's messages from
//! `proto/dform/v1/provider.proto` (needs `protoc`, or `PROTOC` naming it).
//! The service (client and server) is `dform-grpc`'s.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/dform/v1/provider.proto");
    prost_build::Config::new()
        .btree_map(["."])
        // An Apply's stream: many small events, then one large result.
        .enum_attribute(
            ".dform.v1.ApplyEvent.kind",
            "#[allow(clippy::large_enum_variant)]",
        )
        .compile_protos(&["../../proto/dform/v1/provider.proto"], &["../../proto"])?;
    Ok(())
}
