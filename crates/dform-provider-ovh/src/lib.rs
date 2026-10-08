//! The OVHcloud provider, `dform-provider-ovh` (R-44): the Public Cloud of
//! one project (instances, SSH keys, volumes, private networks and
//! subnets, users with S3 credentials, S3 containers) and DNS records,
//! over the OVH API (`api`, signed as `sign` says) with the credentials of
//! the provider's own configuration (`config`), what they may do said
//! by `credential`. `ovh` is the provider (a
//! module per family of types under it), `map` the API's objects as the
//! schema's documents (`schema.df`), `dns` a zone's nameservers for a
//! refusal, and `service` the gRPC service. An instance's user data and a
//! volume's image are write-only: the API never answers them, and dform
//! keeps their digests in state (R-106). A user's S3 secret is held: the
//! API keeps it, and Reveal reads it there (R-45).

pub mod api;
pub mod config;
pub mod credential;
pub mod dns;
#[cfg(feature = "fake")]
pub mod fake;
pub mod map;
pub mod ovh;
pub mod service;
pub mod sign;

pub use service::serve;
