//! The mock provider (`dform-mock`) behind the plugin protocol, over gRPC
//! (`dform-grpc`'s server adapter). dform starts it for every mock schema;
//! `dform provider check` checks it.

fn main() -> std::process::ExitCode {
    match dform_grpc::server::serve(dform_mock::Mock::process()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dform-provider-fake: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
