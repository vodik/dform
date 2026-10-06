//! `dform`: the command line (`dform::cli`) over providers started by
//! `dform-host`'s launcher: each executable a process over gRPC
//! (`dform-grpc`) with the host's service beside it, and, built with the
//! `wasm` feature, each component in the wasm host. The only production
//! path.

fn main() -> std::process::ExitCode {
    dform::cli::main(&dform_host::Launcher::Cli, std::env::args_os())
}
