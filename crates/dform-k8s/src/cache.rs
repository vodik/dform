//! What the provider keeps of a cluster, in the host's cache
//! (`dform:host/cache`): the provider says what each thing is and how
//! widely it holds, the host where and for how long.
//!
//! - A cluster's OpenAPI documents (`openapi SERVER`) are the account's
//!   (the session's credentials'), kept from run to run, so the next run
//!   asks the cluster only what changed; and the run's, so the
//!   deployments of one run on one cluster fetch them once.
//! - The schema derived from a document (`schema KEY`, keyed by the
//!   document's hash and this derivation) is the account's; the
//!   snapshot's is the provider build's, every account's.
//! - A deployment's extension of the static schema (`extension
//!   DEPLOYMENT`, its CRDs) is the account's, so a Configure that is the
//!   program's first of a run (`deferred`) has the CRDs its cluster served
//!   at the last one.
//!
//! A miss is never an error: the provider asks the cluster, or derives
//! again.

pub use dform_sdk::Scope;

/// The text kept at `key` in `scope`, if any.
pub fn read(scope: Scope, key: &str) -> Option<String> {
    dform_sdk::host().cache.get_text(scope, key)
}

/// Keep `text` at `key` in `scope`, in place of what was.
pub fn write(scope: Scope, key: &str, text: &str) {
    dform_sdk::host().cache.put(scope, key, text)
}
