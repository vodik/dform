//! The Kubernetes provider (`src/k8s/`) behind the plugin protocol:
//! `provider k8s { source = "path/to/dform-provider-k8s" }`.

fn main() -> std::process::ExitCode {
    match dform::k8s::serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dform-provider-k8s: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
