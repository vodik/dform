//! A provider's signer (wit/host/dform-host.wit, `signer`): what turns the
//! operator's grant of a credential its provider makes (`signed`,
//! `kubeconfig`) into its material, and signs a request with it, for a
//! provider whose API wants its requests signed with a secret or whose
//! credential is a file of its own. Sans I/O: a signer names what it needs ([`Need`]), the
//! host performs each under the operator's grant and hands it what each
//! got ([`Got`]), until it answers the material ([`Outcome::Material`]).
//! The host holds the material between calls and applies it as its
//! [`Recipe`] says, signing each request it sends with it through
//! [`Signer::sign`] when the recipe is [`Recipe::Signed`].
//!
//! The host runs a signer apart from its provider: as a component that
//! imports nothing ([`component`], `world signer-component`), or in a
//! native provider's process, served to dform as `Signer`; the provider
//! that talks to the network never holds the material, and a signer
//! component cannot reach the network.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "component")]
pub mod component;

/// A request to sign, or a fetch to perform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A fetch's answer, and the host's time when it came, in seconds since
/// the epoch: a server's clock is read against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub at: u64,
}

/// A command to run: its argv, never a shell string, and variables set
/// for it beside the host's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What the host performs for a signer, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// These environment variables: the ones set, by name. Only for
    /// `KIND:environment`.
    Env(Vec<String>),
    /// A file's bytes, `~/` its path under the operator's home; a relative
    /// path is the directory's of the operator's file (`KIND:NAME`), else
    /// the working directory's.
    File(String),
    /// A command's standard output, run under the operator's grant.
    Exec(Command),
    /// This request, sent as it is to exactly its URL, unauthenticated.
    Fetch(Request),
    /// The value the grant gives, when it gives one ([`Got::Given`]).
    Given,
}

/// What a need got.
#[derive(Clone, PartialEq, Eq)]
pub enum Got {
    Env(Vec<(String, String)>),
    /// None: there is no such file.
    File(Option<Vec<u8>>),
    Exec(Vec<u8>),
    Fetch(Response),
    /// The settings a program gave the provider, by setting, or the
    /// operator's file (`KIND:NAME`), named by its kind; none for
    /// `KIND:environment`.
    Given(Vec<(String, Vec<u8>)>),
}

impl core::fmt::Debug for Got {
    /// What it is, never what it holds: a variable's or a file's value may
    /// be a secret.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Got::Env(vars) => write!(f, "Env({} set)", vars.len()),
            Got::File(Some(b)) => write!(f, "File({} bytes)", b.len()),
            Got::File(None) => f.write_str("File(none)"),
            Got::Exec(b) => write!(f, "Exec({} bytes)", b.len()),
            Got::Fetch(r) => write!(f, "Fetch({})", r.status),
            Got::Given(values) => write!(f, "Given({} values)", values.len()),
        }
    }
}

/// A client certificate and its key, PEM.
#[derive(Clone, PartialEq, Eq)]
pub struct TlsClient {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

/// One way the host applies a credential's material.
#[derive(Clone, PartialEq, Eq)]
pub enum Recipe {
    /// `Authorization: Bearer TOKEN`.
    Bearer(String),
    /// These headers, on each request.
    Headers(Vec<(String, String)>),
    /// Each request signed ([`Signer::sign`]) as the host sends it.
    Signed,
    /// The TLS session's client certificate.
    TlsClient(TlsClient),
    /// Each request through this proxy, an `http://` URL.
    Proxy(String),
    /// A CA to trust beside the machine's, PEM.
    Ca(Vec<u8>),
    /// The server not verified.
    Insecure,
    /// The name TLS offers and verifies in place of the URL's host.
    ServerName(String),
}

/// A credential as its signer made it: its bytes, the signer's own
/// encoding, opaque to the host; each way the host applies it; when it
/// stops working, in seconds since the epoch, when it does (the host asks
/// [`Signer::refresh`] before then); and what it is for, when it says,
/// which is not secret: the provider reads it off the credential it
/// opened.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Material {
    pub bytes: Vec<u8>,
    pub recipe: Vec<Recipe>,
    pub expires: Option<u64>,
    /// The server it is for.
    pub endpoint: Option<String>,
    /// The namespace it defaults to.
    pub namespace: Option<String>,
}

impl Material {
    /// Whether each request is signed with it ([`Recipe::Signed`]).
    pub fn signs(&self) -> bool {
        self.recipe.contains(&Recipe::Signed)
    }
}

impl core::fmt::Debug for Material {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Material")
            .field("expires", &self.expires)
            .field("endpoint", &self.endpoint)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

/// What a step came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// More for the host to perform.
    More(Vec<Need>),
    Material(Material),
}

/// A provider's signer: pure functions of what the host hands it.
pub trait Signer {
    /// What it needs first.
    fn needs(&self) -> Result<Vec<Need>, String>;
    /// The next step, given what every need so far got, in the order
    /// they were asked (the first [`Signer::needs`], then each
    /// [`Outcome::More`]'s).
    fn step(&self, got: &[Got]) -> Result<Outcome, String>;
    /// `req` signed with `material` at `now`, the host's time in seconds
    /// since the epoch: the request as it is to be sent.
    fn sign(&self, req: Request, material: &Material, now: u64) -> Result<Request, String>;
    /// The material renewed before it expires, at `now`.
    fn refresh(&self, material: &Material, now: u64) -> Result<Outcome, String>;
}
