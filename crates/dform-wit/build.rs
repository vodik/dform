//! Writes the provider protocol's descriptor set (`provider.fds` in
//! `OUT_DIR`) for tests/drift.rs, which checks the WIT against it. Needs
//! `protoc`, or `PROTOC` naming it, as dform-wire does.

use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/dform/v1/provider.proto");
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("provider.fds");
    let status = Command::new(prost_build::protoc_from_env())
        .arg("--include_source_info")
        .arg("--proto_path=../../proto")
        .arg(format!("--descriptor_set_out={}", out.display()))
        .arg("../../proto/dform/v1/provider.proto")
        .status()?;
    if !status.success() {
        return Err(format!("protoc: {status}").into());
    }
    Ok(())
}
