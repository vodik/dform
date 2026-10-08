//! The host's types (`dform_core::plugin::host`) as the `Host` service's
//! messages and back (R-13b): a failure is a `Failure` field carrying its
//! class, or `NOT_YET`, never a gRPC status.

use crate::host_pb as h;
use dform_core::plugin::host::{
    Class, Endpoint, Error, Failure, GitFile, HttpRequest, HttpResponse, Level, Run, Target,
};
use std::time::Duration;

pub fn from_error(e: Error) -> h::Failure {
    h::Failure {
        kind: match e.class {
            Class::Final => h::Kind::Final,
            Class::Retryable => h::Kind::Retryable,
            Class::MaybeApplied => h::Kind::MaybeApplied,
        } as i32,
        message: e.message,
    }
}

pub fn from_failure(f: Failure) -> h::Failure {
    match f {
        Failure::NotYet(m) => h::Failure {
            kind: h::Kind::NotYet as i32,
            message: m,
        },
        Failure::Error(e) => from_error(e),
    }
}

/// A response's failure, if it has one.
pub fn to_failure(f: Option<h::Failure>) -> Result<(), Failure> {
    let Some(f) = f else {
        return Ok(());
    };
    let class = match h::Kind::try_from(f.kind) {
        Ok(h::Kind::NotYet) => return Err(Failure::NotYet(f.message)),
        Ok(h::Kind::Retryable) => Class::Retryable,
        Ok(h::Kind::MaybeApplied) => Class::MaybeApplied,
        _ => Class::Final,
    };
    Err(Failure::Error(Error {
        class,
        message: f.message,
    }))
}

/// A response's failure, where the call has no `not-yet`: one is an error.
pub fn to_error(f: Option<h::Failure>) -> Result<(), Error> {
    to_failure(f).map_err(|f| match f {
        Failure::NotYet(m) => Error::fatal(format!("not yet: {m}")),
        Failure::Error(e) => e,
    })
}

pub fn target(t: Option<h::Target>) -> Target {
    let t = t.unwrap_or_default();
    Target {
        host: t.host,
        user: t.user,
        port: t.port.and_then(|p| u16::try_from(p).ok()),
    }
}

pub fn from_target(t: &Target) -> h::Target {
    h::Target {
        host: t.host.clone(),
        user: t.user.clone(),
        port: t.port.map(u32::from),
    }
}

fn headers(hs: Vec<h::Header>) -> Vec<(String, String)> {
    hs.into_iter().map(|x| (x.name, x.value)).collect()
}

fn from_headers(hs: Vec<(String, String)>) -> Vec<h::Header> {
    hs.into_iter()
        .map(|(name, value)| h::Header { name, value })
        .collect()
}

/// A Send request: the request, its credential and its tunnel.
pub fn request(r: h::SendRequest) -> (HttpRequest, Option<u64>, Option<u64>) {
    (
        HttpRequest {
            method: r.method,
            url: r.url,
            headers: headers(r.headers),
            body: r.body,
            timeout: r.timeout_ms.map(|ms| Duration::from_millis(ms.into())),
        },
        r.auth,
        r.via,
    )
}

pub fn from_request(r: HttpRequest, auth: Option<u64>, via: Option<u64>) -> h::SendRequest {
    h::SendRequest {
        method: r.method,
        url: r.url,
        headers: from_headers(r.headers),
        body: r.body,
        timeout_ms: r
            .timeout
            .map(|t| u32::try_from(t.as_millis()).unwrap_or(u32::MAX)),
        auth,
        via,
    }
}

pub fn from_response(r: Result<HttpResponse, Error>) -> h::SendResponse {
    match r {
        Ok(r) => h::SendResponse {
            failure: None,
            status: r.status.into(),
            headers: from_headers(r.headers),
            body: r.body,
        },
        Err(e) => h::SendResponse {
            failure: Some(from_error(e)),
            ..Default::default()
        },
    }
}

pub fn response(r: h::SendResponse) -> Result<HttpResponse, Error> {
    to_error(r.failure)?;
    Ok(HttpResponse {
        status: u16::try_from(r.status).unwrap_or(0),
        headers: headers(r.headers),
        body: r.body,
    })
}

pub fn from_run(r: Result<Run, Failure>) -> h::ExecResponse {
    match r {
        Ok(r) => h::ExecResponse {
            failure: None,
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        },
        Err(f) => h::ExecResponse {
            failure: Some(from_failure(f)),
            ..Default::default()
        },
    }
}

pub fn run(r: h::ExecResponse) -> Result<Run, Failure> {
    to_failure(r.failure)?;
    Ok(Run {
        status: r.status,
        stdout: r.stdout,
        stderr: r.stderr,
    })
}

pub fn endpoint(r: &h::ForwardRequest) -> Endpoint {
    Endpoint {
        host: r.host.clone(),
        port: u16::try_from(r.port).unwrap_or(0),
    }
}

pub fn files(fs: Vec<h::GitFile>) -> Vec<GitFile> {
    fs.into_iter()
        .map(|f| GitFile {
            path: f.path,
            data: f.data,
        })
        .collect()
}

pub fn from_files(fs: Vec<GitFile>) -> Vec<h::GitFile> {
    fs.into_iter()
        .map(|f| h::GitFile {
            path: f.path,
            data: f.data,
        })
        .collect()
}

pub fn level(l: i32) -> Level {
    match h::Level::try_from(l) {
        Ok(h::Level::Debug) => Level::Debug,
        Ok(h::Level::Warn) => Level::Warn,
        Ok(h::Level::Error) => Level::Error,
        _ => Level::Info,
    }
}

pub fn from_level(l: Level) -> i32 {
    (match l {
        Level::Debug => h::Level::Debug,
        Level::Info => h::Level::Info,
        Level::Warn => h::Level::Warn,
        Level::Error => h::Level::Error,
    }) as i32
}

/// How large a `ReadChunk` is.
const CHUNK: usize = 1 << 20;

/// A read's answer as `ReadChunk`s: its bytes in order (one chunk when
/// empty), or the one failure.
pub fn chunks(r: Result<Vec<u8>, Failure>) -> Vec<h::ReadChunk> {
    versioned_chunks(r.map(dform_core::files::Document::new))
}

/// A versioned read's answer as `ReadChunk`s (`ReadVersioned`, R-172):
/// [`chunks`], the first carrying the version.
pub fn versioned_chunks(r: Result<dform_core::files::Document, Failure>) -> Vec<h::ReadChunk> {
    match r {
        Ok(d) if d.bytes.is_empty() => vec![h::ReadChunk {
            version: d.version,
            ..h::ReadChunk::default()
        }],
        Ok(d) => {
            let mut version = d.version;
            d.bytes
                .chunks(CHUNK)
                .map(|c| h::ReadChunk {
                    failure: None,
                    data: c.to_vec(),
                    version: version.take(),
                })
                .collect()
        }
        Err(f) => vec![h::ReadChunk {
            failure: Some(from_failure(f)),
            data: Vec::new(),
            version: None,
        }],
    }
}

/// `ReadChunk`s as the read's answer.
pub fn read(chunks: Vec<h::ReadChunk>) -> Result<Vec<u8>, Failure> {
    read_versioned(chunks).map(|d| d.bytes)
}

/// `ReadChunk`s as the read's answer, with the version the first gives.
pub fn read_versioned(chunks: Vec<h::ReadChunk>) -> Result<dform_core::files::Document, Failure> {
    let mut out = dform_core::files::Document::default();
    for c in chunks {
        to_failure(c.failure)?;
        out.bytes.extend(c.data);
        if out.version.is_none() {
            out.version = c.version;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each class, and not-yet, crosses as itself.
    #[test]
    fn failures_keep_their_class() {
        for f in [
            Failure::NotYet("booting".into()),
            Failure::Error(Error::fatal("no")),
            Failure::Error(Error::retryable("503")),
            Failure::Error(Error::maybe_applied("timed out")),
        ] {
            assert_eq!(to_failure(Some(from_failure(f.clone()))), Err(f));
        }
        assert_eq!(to_failure(None), Ok(()));
    }
}
