//! The OVHcloud provider, `dform-provider-ovh` (R-44): Public Cloud
//! instances and SSH keys of one project, and DNS records, over the OVH
//! API (`api`, signed as `sign` says) with the credentials of the
//! provider's own configuration (`config`). `ovh` is the provider, `map`
//! the API's objects as the schema's documents (`schema.df`), `dns` a
//! zone's nameservers for a refusal, and `service` the gRPC service. An instance's user data is write-only: the
//! API never answers it, and dform keeps its digest in state (R-106).

pub mod api;
pub mod config;
pub mod dns;
#[cfg(feature = "fake")]
pub mod fake;
pub mod map;
pub mod ovh;
pub mod service;
pub mod sign;

pub use service::serve;
