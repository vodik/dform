//! The Kubernetes provider, `dform-provider-k8s`: the plugin protocol over
//! the API server of the cluster the kubeconfig names (`service`). Its
//! schema is derived from the cluster's OpenAPI document (`openapi`), its
//! documents map to and from objects in `object`, and `cluster` is the
//! client. `health` judges an object from its status, for `dform status`.

pub mod cluster;
pub mod health;
pub mod object;
pub mod openapi;
pub mod service;

pub use service::serve;
