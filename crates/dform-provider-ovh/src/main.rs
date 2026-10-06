//! The OVHcloud provider (`dform-provider-ovh`) behind the plugin protocol:
//! `[providers] ovh = { path = "path/to/dform-provider-ovh" }`.

fn main() -> std::process::ExitCode {
    match dform_provider_ovh::serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dform-provider-ovh: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
