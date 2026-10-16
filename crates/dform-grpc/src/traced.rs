//! A provider call's trace number on the wire: the `dform-call` metadata
//! of a protocol call (dform to the provider: the call it is) and of the
//! host's `Send` (the provider back to dform: the call the request is
//! sent for), so that the host's span of the request is the call's child
//! (`dform_core::plugin::timed::call_span`). Metadata, where gRPC carries
//! trace context: the messages are unchanged, and a peer that does not
//! know it ignores it.

/// The metadata key.
pub const KEY: &str = "dform-call";

/// `message` as a request naming the call `call` (none: not traced).
pub fn request<T>(message: T, call: Option<u64>) -> tonic::Request<T> {
    let mut r = tonic::Request::new(message);
    if let Some(n) = call {
        r.metadata_mut().insert(KEY, n.into());
    }
    r
}

/// The call `r` names.
pub fn call<T>(r: &tonic::Request<T>) -> Option<u64> {
    r.metadata().get(KEY)?.to_str().ok()?.parse().ok()
}
