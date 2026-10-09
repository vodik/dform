//! The Tailscale provider's executable: `dform_provider_tailscale::main`
//! serves it over gRPC (`dform_sdk::provider!`):
//! `[providers] tailscale = { path = "path/to/dform-provider-tailscale" }`.

fn main() -> std::process::ExitCode {
    dform_provider_tailscale::main()
}
