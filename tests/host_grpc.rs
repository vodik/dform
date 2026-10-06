//! The host's interfaces over gRPC (R-13b): dform's `Host` service
//! (`dform_host::grpc`) called as a native SDK provider calls it
//! (`dform_sdk::native::Grpc`). A credential is opened by the name dform.toml
//! grants and applied by the host, its value never crossing; one not
//! granted is refused naming the provider and the credential; a failure's
//! class and `not yet` cross as themselves.

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
    let (_h, mut c) = client(Services::new(grants(&["kubeconfig:prod"])));
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
    let (_h, mut c) = client(Services::new(grants(&["bearer:host-grpc-test"])));
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
    fn exec(&mut self, on: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(Failure::NotYet(format!("{} is booting", on.host)))
    }
    fn read(&mut self, _: &Target, _: &str) -> Result<Vec<u8>, Failure> {
        Err(Error::retryable("connection reset").into())
    }
    fn write(&mut self, _: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(Error::maybe_applied("timed out after sending"))
    }
    fn forward(&mut self, _: &Target, _: &Endpoint) -> Result<SocketAddr, Error> {
        Err(Error::fatal("no route"))
    }
}

/// `not yet`, and each class, cross the service as themselves (R-81's
/// wait and retries read them).
#[test]
fn failures_cross_with_their_class() {
    let (_h, mut c) = client(Services::new(grants(&[])).with_ssh(Box::new(Booting)));
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
        c.read(&on, "/etc/x").unwrap_err(),
        Failure::Error(Error::retryable("connection reset"))
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
