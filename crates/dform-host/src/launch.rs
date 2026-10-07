//! How the CLI starts providers ([`Launcher`]): an executable as a process
//! over gRPC (`dform-grpc`) with the `Host` service beside it, named in its
//! environment; a component (`.wasm`) in the wasm host, with the same
//! interfaces as imports (the `wasm` feature). Each is started with the
//! grants dform.toml gives it (`plugin::host::grants_for`), and how it is
//! hosted is recorded for `provider check` (`plugin::host::observe`).

use crate::grpc;
use crate::services::Services;
use anyhow::Result;
use dform_core::plugin::Launch;
use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Stop, Ticket};
use dform_core::plugin::host::{self, Grants, Hosting, Manifest};
use dform_core::plugin::link::Link;
use dform_core::plugin::pb;
use dform_core::plugin::source;
use dform_grpc::client::{Conn, Process};
use dform_grpc::spawn::{Env, FAKE_ENV, Program};
use std::path::{Path, PathBuf};

/// Every provider started as `dform-grpc`'s [`Process`] does, an
/// executable with the host service beside it, a component in the wasm
/// host.
pub enum Launcher {
    /// The `dform` binary's: the mock is `dform __provider fake`.
    Cli,
    /// A test's: the mock is the executable at this path
    /// (`dform-provider-fake`), or a component.
    Mock(PathBuf),
}

impl Launcher {
    fn process(&self) -> Process {
        match self {
            Launcher::Cli => Process::Cli,
            Launcher::Mock(p) => Process::Mock(p.clone()),
        }
    }

    /// The mock as a component, when `DFORM_PROVIDER_FAKE` (or the test's
    /// path) names a `.wasm`.
    fn mock_component(&self) -> Option<PathBuf> {
        let named = std::env::var_os(FAKE_ENV)
            .map(PathBuf::from)
            .or_else(|| match self {
                Launcher::Mock(p) => Some(p.clone()),
                Launcher::Cli => None,
            })?;
        named
            .extension()
            .is_some_and(|e| e == "wasm")
            .then_some(named)
    }
}

/// The mock's grants: it is dform's own, and its world, inventory and
/// schemas are files.
pub fn mock_grants() -> Grants {
    Grants {
        provider: "fake".into(),
        allow: ["wasi:filesystem".to_string()].into(),
        credentials: Default::default(),
    }
}

impl Launch for Launcher {
    fn mock(&self) -> Result<Link> {
        match self.mock_component() {
            Some(c) => component(&c, mock_grants()),
            None => self.process().mock(),
        }
    }

    fn plugin(&self, exe: &Path) -> Result<Link> {
        let grants = host::grants_for(exe);
        if source::is_component(exe) {
            return component(exe, grants);
        }
        native(exe, grants)
    }
}

/// A component in the wasm host.
#[cfg(feature = "wasm")]
fn component(path: &Path, grants: Grants) -> Result<Link> {
    crate::wasm::link(path, grants)
}

#[cfg(not(feature = "wasm"))]
fn component(path: &Path, _: Grants) -> Result<Link> {
    anyhow::bail!(
        "provider {}: a component needs the wasm host, which this dform is built without \
         (experimental: cargo build --features wasm)",
        path.display()
    )
}

/// A native provider and the host service it may call back into.
struct Native {
    conn: Conn,
    /// Dropped after the connection: the provider exits first.
    _host: grpc::Served,
}

impl Provider for Native {
    fn submit(&mut self, call: Call) -> Ticket {
        self.conn.submit(call)
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        self.conn.next_completed(events)
    }

    fn is_dead(&mut self) -> bool {
        self.conn.is_dead()
    }

    fn stopper(&self) -> Option<Stop> {
        self.conn.stopper()
    }
}

/// The executable `exe`, with the host service at `DFORM_HOST`.
fn native(exe: &Path, grants: Grants) -> Result<Link> {
    let served = grpc::serve(Services::new(grants.clone()))?;
    let program = Program::exe(exe);
    let mut conn = Conn::start(&program, &Env::default().set(grpc::ENV, &served.address))?;
    let manifest = conn
        .manifest()?
        .map(|imports| Manifest::of(imports.iter().map(String::as_str), false));
    let display = program.display();
    host::observe(
        &display,
        Hosting {
            host: "native",
            manifest,
            grants,
        },
    );
    Link::start(
        display,
        Box::new(Native {
            conn,
            _host: served,
        }),
    )
}
