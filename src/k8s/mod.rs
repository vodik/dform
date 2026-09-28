//! The Kubernetes provider, `dform-provider-k8s`: the plugin protocol over
//! the API server of the cluster the kubeconfig names (`service`). Its
//! schema is derived from the cluster's OpenAPI document (`openapi`), its
//! documents map to and from objects in `object`, and `cluster` is the
//! client.

pub mod cluster;
pub mod object;
pub mod openapi;
pub mod service;

pub use service::serve;
