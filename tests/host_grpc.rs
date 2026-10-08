//! The host's interfaces over gRPC (R-13b): dform's `Host` service
//! (`dform_host::grpc`) called as a native SDK provider calls it
//! (`dform_sdk::native::Grpc`). A credential is opened by the name dform.toml
//! grants and applied by the host, its value never crossing; one not
//! granted is refused naming the provider and the credential; a failure's
//! class and `not yet` cross as themselves. A location is read through
//! `io.read` (`Host.Read`, R-153, R-155) as dform.toml grants it, a scheme a
//! provider declares routed to that provider by the host.

mod common;

use dform::plugin::credentials::{Secret, provide};
use dform::plugin::host::{
    Calls, Class, Endpoint, Error, Failure, Grants, HttpRequest, Run, Target,
};
use dform_host::services::Services;
use dform_host::ssh::Ssh;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;

fn grants(credentials: &[&str]) -> Grants {
    Grants {
        provider: "k8s".into(),
        allow: Default::default(),
        credentials: credentials.iter().map(|s| s.to_string()).collect(),
        ..Grants::default()
    }
}

/// A host for `services`, and a native provider's client of it.
fn client(services: Services) -> (dform_host::grpc::Served, dform_sdk::native::Grpc) {
    let served = dform_host::grpc::serve(services).unwrap();
    let c = dform_sdk::native::Grpc::dial(&served.address).unwrap();
    (served, c)
}

#[test]
fn a_credential_not_granted_is_refused_naming_both() {
    let (_h, c) = client(Services::new(grants(&["kubeconfig:prod"])));
    let e = c.open("kubeconfig:staging").unwrap_err();
    assert_eq!(e.class, Class::Final);
    assert!(
        e.message
            .contains("provider k8s is not granted the credential kubeconfig:staging"),
        "{}",
        e.message
    );
    assert!(
        e.message.contains("[providers.k8s] credentials"),
        "{}",
        e.message
    );
}

/// A one-request HTTP server on the loopback answering with the
/// Authorization header it was sent.
fn echo_auth() -> (SocketAddr, std::thread::JoinHandle<()>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let t = std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut auth = String::new();
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if line.to_ascii_lowercase().starts_with("authorization:") {
                auth = line["authorization:".len()..].trim().to_string();
            }
        }
        let mut s = s;
        write!(
            s,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{auth}",
            auth.len()
        )
        .unwrap();
    });
    (addr, t)
}

/// The provider names the credential; the host applies it to the request.
/// What the provider holds is a handle: the value is in no message it is
/// sent.
#[test]
fn a_granted_credential_is_applied_by_the_host() {
    provide(
        "bearer:host-grpc-test",
        Secret::new(b"t0ken-value".to_vec()),
    );
    let (_h, c) = client(Services::new(grants(&["bearer:host-grpc-test"])));
    let opened = c.open("bearer:host-grpc-test").unwrap();
    assert!(!format!("{opened:?}").contains("t0ken"));
    let (addr, t) = echo_auth();
    let r = c
        .send(
            HttpRequest {
                method: "GET".into(),
                url: format!("http://{addr}/"),
                ..HttpRequest::default()
            },
            Some(opened.handle),
            None,
        )
        .unwrap();
    t.join().unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"Bearer t0ken-value");
}

/// An SSH client that says the host is not up yet, then fails.
struct Booting;

impl Ssh for Booting {
    fn exec(&self, on: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(Failure::NotYet(format!("{} is booting", on.host)))
    }
    fn write(&self, _: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(Error::maybe_applied("timed out after sending"))
    }
    fn forward(&self, _: &Target, _: &Endpoint) -> Result<SocketAddr, Error> {
        Err(Error::fatal("no route"))
    }
}

/// `not yet`, and each class, cross the service as themselves (R-81's
/// wait and retries read them).
#[test]
fn failures_cross_with_their_class() {
    let (_h, c) = client(Services::new(grants(&[])).with_ssh(Box::new(Booting)));
    let on = Target {
        host: "node1".into(),
        user: "root".into(),
        port: None,
    };
    assert_eq!(
        c.exec(&on, &["true".into()], None).unwrap_err(),
        Failure::NotYet("node1 is booting".into())
    );
    assert_eq!(
        c.write(&on, "/etc/x", b"", 0o600).unwrap_err(),
        Error::maybe_applied("timed out after sending")
    );
    let e = c
        .forward(
            &on,
            &Endpoint {
                host: "api".into(),
                port: 6443,
            },
        )
        .unwrap_err();
    assert_eq!(e, Error::fatal("no route"));
}

/// Two calls of one provider to its host run at once: neither its client
/// nor the host holds a lock across a call's I/O (R-142). The server answers only once both
/// requests are open; before, the second waited for the first, which
/// waited for the second, and was answered `503 alone`.
#[test]
fn a_provider_s_host_calls_run_at_once() {
    let (_h, c) = client(Services::new(grants(&[])));
    let c = std::sync::Arc::new(c);
    let addr = common::answers_in_pairs(std::time::Duration::from_secs(3));
    let calls: Vec<_> = (0..2)
        .map(|_| {
            let c = c.clone();
            std::thread::spawn(move || {
                c.send(
                    HttpRequest {
                        method: "GET".into(),
                        url: format!("http://{addr}/"),
                        ..HttpRequest::default()
                    },
                    None,
                    None,
                )
                .unwrap()
            })
        })
        .collect();
    for c in calls {
        let r = c.join().unwrap();
        assert_eq!(
            (r.status, String::from_utf8_lossy(&r.body).to_string()),
            (200, "200 pair".to_string())
        );
    }
}

/// A provider declaring `gs` (as its manifest would): the run's reader
/// routes `gs://` to it. This one answers one object and says the rest
/// are not there yet.
struct Gs;

impl dform::files::Transport for Gs {
    fn read(&self, at: &dform::uri::Uri, _: &dform::files::Files) -> Result<Vec<u8>, Failure> {
        match at.path.as_str() {
            "/cfg.yml" => Ok(b"from gs".to_vec()),
            _ => Err(Failure::NotYet(format!("{at} is not written yet"))),
        }
    }
}

/// `io.read` over gRPC: a location the provider's grants name is read
/// by the host and streams back (`Host.Read`); one they do not name is
/// refused naming the provider, the location and the dform.toml key; a
/// project file is never a provider's; a scheme another provider declares
/// is read by that provider, through the host, its `not yet` crossing as
/// itself.
#[test]
fn a_location_is_read_as_the_grants_allow() {
    let files = std::sync::Arc::new(dform::files::Files::default());
    files.declare("gs", "google", std::sync::Arc::new(Gs));
    let mut g = grants(&[]);
    g.reads = ["data:*", "gs://bucket/*", "file:*"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    g.files = dform::files::Shared(Some(files));
    let (_h, c) = client(Services::new(g));
    assert_eq!(c.io_read("data:,hello%20host").unwrap(), b"hello host");
    let big = format!("data:;base64,{}", "QUFB".repeat(600_000));
    assert_eq!(c.io_read(&big).unwrap().len(), 1_800_000);
    assert_eq!(c.io_read("gs://bucket/cfg.yml").unwrap(), b"from gs");
    assert_eq!(
        c.io_read("gs://bucket/later.yml").unwrap_err(),
        Failure::NotYet("gs://bucket/later.yml is not written yet".into())
    );
    let Failure::Error(e) = c.io_read("https://other.example/x").unwrap_err() else {
        panic!("not an error")
    };
    assert_eq!(e.class, Class::Final);
    assert!(
        e.message
            .contains("provider k8s is not granted a read of https://other.example/x")
            && e.message.contains("[providers.k8s] reads"),
        "{}",
        e.message
    );
    let Failure::Error(e) = c.io_read("gs://other/cfg.yml").unwrap_err() else {
        panic!("not an error")
    };
    assert!(
        e.message
            .contains("not granted a read of gs://other/cfg.yml"),
        "{}",
        e.message
    );
    let Failure::Error(e) = c.io_read("file:///etc/passwd").unwrap_err() else {
        panic!("not an error")
    };
    assert!(e.message.contains("the project's"), "{}", e.message);
}

/// A source that keeps versions answers its version.
struct Kept;

impl dform::files::Transport for Kept {
    fn read(&self, at: &dform::uri::Uri, f: &dform::files::Files) -> Result<Vec<u8>, Failure> {
        self.read_document(at, f).map(|d| d.bytes)
    }

    fn read_document(
        &self,
        _: &dform::uri::Uri,
        _: &dform::files::Files,
    ) -> Result<dform::files::Document, Failure> {
        Ok(dform::files::Document {
            bytes: b"s3cr3t".to_vec(),
            version: Some("7".into()),
        })
    }
}

/// A native provider's own read of a location whose source keeps
/// versions answers the version too (`Host.ReadVersioned`, R-172).
#[test]
fn a_location_s_version_crosses() {
    let files = std::sync::Arc::new(dform::files::Files::default());
    files.declare("kv", "vault", std::sync::Arc::new(Kept));
    let mut g = grants(&[]);
    g.reads = ["kv://app/*".to_string()].into();
    g.files = dform::files::Shared(Some(files));
    let (_h, c) = client(Services::new(g));
    let d = c.io_read_versioned("kv://app/key").unwrap();
    assert_eq!(d.bytes, b"s3cr3t");
    assert_eq!(d.version.as_deref(), Some("7"));
    assert_eq!(c.io_read("kv://app/key").unwrap(), b"s3cr3t");
}
