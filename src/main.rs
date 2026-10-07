//! `dform`: the command line (`dform::cli`) over providers started by
//! `dform-host`'s launcher: each executable a process over gRPC
//! (`dform-grpc`) with the host's service beside it, and, built with the
//! `wasm` feature, each component in the wasm host. The only production
//! path.

fn main() -> std::process::ExitCode {
    // `s3://BUCKET/KEY` is read with the S3 client (R-153).
    dform_core::files::register("s3", std::sync::Arc::new(dform_s3::Reader));
    dform::cli::main(&dform_host::Launcher::Cli, std::env::args_os())
}
