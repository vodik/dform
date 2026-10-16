//! The Kubernetes provider (`dform-provider-k8s`) behind the plugin protocol:
//! `provider k8s { source = "path/to/dform-provider-k8s" }`.

fn main() -> std::process::ExitCode {
    // The client's chain of errors behind an offline line, under
    // `DFORM_LOG=debug`.
    dform_core::log::install();
    dform_provider_k8s::main()
}
