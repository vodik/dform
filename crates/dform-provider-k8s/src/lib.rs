//! The Kubernetes provider (`dform-k8s`) as dform runs it: natively the
//! executable `dform-provider-k8s`, built for wasm32-wasip3 with the
//! `component` feature a wasm provider. Either reaches the cluster only
//! through the host's HTTP, with the kubeconfig the host holds for it,
//! which its signer (`signer/`, `dform_provider_k8s_signer.wasm` beside
//! the wasm provider) makes of the operator's grant.

dform_sdk::provider!(
    dform_k8s::service::Provider::default(),
    uses = ["dform:host/http", "dform:host/secrets"]
);
