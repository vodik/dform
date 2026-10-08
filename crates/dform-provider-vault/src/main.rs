//! The Vault provider's executable: `dform_provider_vault::main` serves it
//! over gRPC (`dform_sdk::provider!`):
//! `[providers] vault = { path = "path/to/dform-provider-vault" }`.

fn main() -> std::process::ExitCode {
    dform_provider_vault::main()
}
