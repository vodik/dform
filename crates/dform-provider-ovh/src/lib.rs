//! The OVHcloud provider, `dform-provider-ovh` (R-44): Public Cloud
//! instances and SSH keys of one project, and DNS records, over the OVH
//! API (`api`, signed as `sign` says) with the credentials of the
//! provider's own configuration (`config`). `ovh` is the provider, `map`
//! the API's objects as the schema's documents (`schema.df`), `record` the
//! user data digests the API cannot answer, and `service` the gRPC
//! service.

pub mod api;
pub mod config;
#[cfg(feature = "fake")]
pub mod fake;
pub mod map;
pub mod ovh;
pub mod record;
pub mod service;
pub mod sign;

pub use service::serve;
