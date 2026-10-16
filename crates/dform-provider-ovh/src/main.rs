//! The OVHcloud provider (`dform-provider-ovh`) behind the plugin protocol:
//! `[providers] ovh = { path = "path/to/dform-provider-ovh" }`.

fn main() -> std::process::ExitCode {
    dform_provider_ovh::main()
}
