//! `dform`: the command line (`dform::cli`) over providers reached by
//! gRPC, each a process (`dform-grpc`). The only production path.

fn main() -> std::process::ExitCode {
    dform::cli::main(&dform_grpc::client::Process::Cli, std::env::args_os())
}
