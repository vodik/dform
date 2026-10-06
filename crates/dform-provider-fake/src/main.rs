//! The mock provider's executable: `dform_provider_fake::main` serves it
//! over gRPC (`dform_sdk::provider!`).

fn main() -> std::process::ExitCode {
    dform_provider_fake::main()
}
