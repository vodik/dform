//! The Postgres provider's executable: `dform_provider_postgres::main`
//! serves it over gRPC (`dform_sdk::provider!`):
//! `[providers] postgres = { path = "path/to/dform-provider-postgres" }`.

fn main() -> std::process::ExitCode {
    dform_provider_postgres::main()
}
