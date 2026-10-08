# Writing a provider

A provider is what dform asks about the world: it reads objects, plans a
change to one, applies it. The protocol it speaks is
`wit/dform-provider.wit` (DESIGN.org R-13). This page is how to write one
with the SDK (`crates/dform-sdk`), what the host gives it, how the
operator grants it, and the two ways it runs.

## The host does the transport

A provider does protocol logic; dform does the transport. Everything a
provider would otherwise reimplement, and every place a credential would
reach it, is a call to the host (`wit/host/dform-host.wit`, `dform:host`):

| interface | what it does |
|---|---|
| `http` | a request with TLS by the host: the machine's CA and proxy (`HTTPS_PROXY`, `NO_PROXY`), a credential applied (a header, a client certificate and its CA), optionally through a tunnel |
| `io` | `read` a location, whatever its scheme (read-only: a write is a provider's apply, R-155): `ssh://USER@HOST/PATH`, `https://`, `git+https://HOST/OWNER/REPO/PATH?ref=TAG` through a mirror in `$XDG_CACHE_HOME/dform/git/<host>-<owner>/<repo>.git/`, `s3://BUCKET/KEY`, a scheme another provider declares (R-153; docs/reference.md, "Locations and transports"); as dform.toml grants |
| `ssh` | `exec` (argv, never a shell string), `write` a file, `forward` a port to a tunnel; keys from the operator's agent (a file is read through `io`) |
| `git` | a commit onto a branch of a local repository (a file is read through `io`) |
| `secrets` | open a credential by name: a handle, never the value |
| `log` | a line shown with dform's own output |

A read of something the world has not reached yet (a file cloud-init has
not written, a host still booting) answers `not-yet`, distinct from an
error, so the engine waits on it, within the provider's `wait` (10m
unless dform.toml says; R-81, R-122), not a call's `timeout`. An error carries a class: `final`, `retryable` (nothing changed,
worth sending again) or `maybe-applied` (no answer came; it may have
taken effect).

## With the SDK

```toml
# Cargo.toml
[lib]
crate-type = ["rlib", "cdylib"]

[features]
component = ["dform-sdk/component"]

[dependencies]
dform-sdk = { path = "../dform-sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
// src/lib.rs
use dform_sdk::typed::Result;
use dform_sdk::{host, Lifecycle, Progress, Provider, Request, Resource, Typed};
use serde::{Deserialize, Serialize};

struct Acme {
    endpoint: String,
    token: dform_sdk::Credential,
}

impl Provider for Acme {
    const NAME: &'static str = "acme";
    fn configure(settings: &serde_json::Value) -> Result<(Acme, Option<String>)> {
        let endpoint = settings["endpoint"].as_str().unwrap_or_default().to_string();
        // The operator granted `bearer:acme`; the host applies it.
        let token = host().secrets.open("bearer:acme")?;
        Ok((Acme { endpoint, token }, None))
    }
}

/// An object storage bucket.
#[derive(Resource, Serialize, Deserialize)]
#[dform(type = "acme.bucket", replace = "destroy_first")]
struct Bucket {
    #[dform(required, force_new)]
    name: String,
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
    #[dform(computed, id)]
    #[serde(default)]
    id: Option<String>,
}

impl Lifecycle<Acme> for Bucket {
    fn read(p: &Acme, remote: &str) -> Result<Option<Bucket>> {
        let r = host().http.request(
            Request::get(format!("{}/buckets/{remote}", p.endpoint)).auth(&p.token),
        )?;
        if r.status == 404 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&r.body)?))
    }
    fn create(p: &Acme, desired: Bucket, key: &str, progress: &Progress) -> Result<(String, Bucket)> {
        // .. POST it, then poll until it is ready, saying each status seen:
        progress.status("PROVISIONING");
        /* .. */
    }
    fn update(p: &Acme, remote: &str, prior: Bucket, desired: Bucket, progress: &Progress) -> Result<Bucket> { /* .. */ }
    fn delete(p: &Acme, remote: &str, progress: &Progress) -> Result<()> { /* .. */ }
}

dform_sdk::provider!(
    Typed::<Acme>::new().resource::<Bucket>(),
    uses = ["dform:host/http", "dform:host/secrets"],
);
```

```rust
// src/main.rs
fn main() -> std::process::ExitCode {
    acme_provider::main()
}
```

- `#[derive(Resource)]` writes the type's schema facts from its fields:
  `String` is a string, the integers int, `f64` number, `bool` bool, a
  `Vec` list, a map map, anything else obj (`Option<T>` is T's);
  `ty = "ref(net.vpc)"` says otherwise. Field flags: `required`,
  `computed`, `id`, `sensitive`, `force_new`, `optional_computed`,
  `nullable`, `write_only` (the API takes it and never answers it: a
  password; dform keeps its digest in state and a changed one is a
  change, R-106), `name_like`; `list_key = "name"`. On the struct: `type`
  (required), `replace`, `retry`. A computed field is an `Option` with
  `#[serde(default)]`: the engine's desired document does not have it.
  An `optional_computed` one is answered as computed (dform compares it
  only where the program writes it), so leave it out of what Read
  answers when unset: `skip_serializing_if = "Option::is_none"`.
- `Lifecycle` is the four calls, and `check`, a refusal of a document
  the provider cannot apply as configured (Postgres refuses to manage the
  role it connects as), called by Plan once the provider is configured
  and by Apply before it changes anything. Plan is not written: the SDK
  diffs leaf by leaf with the schema, a change to a `force_new` path
  replaces, a missing `required` attribute refuses. Import is a read.
- `Provider::configure` gets the program's `use NAME { .. }` settings,
  each secret revealed into them, `{}` when the program writes none.
- `const KEEP: bool = true` on the `Provider` says each `update` sends
  only what `desired` holds, leaving an absent attribute as the object
  has it. The provider then has the `keep` capability: an Apply update's
  `keep` paths (a write-only secret a run without the deployment's master
  proved unchanged, R-164) come filled from what Read answers, or absent
  where it answers nothing. Without it a `keep` is refused, and dform
  sends none: such an update needs the master.
- `Typed::example::<R>(create, update, required)` gives `dform provider
  check` documents of the provider's own types to run its resource
  checks with.
- Create, update and delete get a `Progress`: `progress.status("BUILD")`
  each time the object's status as the API gives it changes (a poll that
  saw a new one), `progress.message(..)` for the log. dform prints the
  status beside the change in apply's progress, as it is, with the time
  the call has run; say it when your view changes, never on a timer.
  Over gRPC it is the Apply's stream of events, in a component the
  `stream<event>` its `apply` answers (R-130). A `Handler` gets the same
  sink as `handle`'s second argument.
- An error refuses the call. One the host classed `maybe-applied` is
  `MaybeApplied` (dform looks before it sends it again); a `retryable`
  one says so, and dform's retry policy sends it again.
- `provider!(EXPR)` is the entry point. Any `Handler` will do: the fake
  provider (`crates/dform-provider-fake`) is `provider!(dform_mock::Mock::process())`,
  the mock answering every call itself.

Not yet in the typed layer: externs (Query), refinements on secret paths
(an Apply with assertions is refused saying so), the `managed`
capability, and schema docs from `///`.
A provider that needs them implements `Handler` itself.

## Two transports, one source

**Native** (`cargo build`): an executable. dform starts it, reads its
handshake line, and speaks gRPC to it (`proto/dform/v1/provider.proto`).
Beside each, dform serves the `Host` service on a socket of its own and
names it in the provider's environment as `DFORM_HOST`
(`proto/dform/host/v1/host.proto`, additive: the provider contract is
unchanged); the SDK dials it. The provider also serves `Manifest`, what
`uses = [..]` declares. A native provider runs as the user: its grants
and its manifest are a declaration and a convention, not a sandbox.
dform sends calls at once (a refresh's Reads, a plan's Plans,
`--parallel`'s Applies); natively the SDK runs each on a thread of its
own, and the host calls they make run at once too, so a `Handler` (or a
`Lifecycle`) is called from several threads at once and keeps any state
of its own behind its own lock (it is `Send + Sync`).

**Component** (experimental): `cargo build --target wasm32-wasip2
--features component` makes `target/wasm32-wasip2/debug/<crate>.wasm`, a
component of `dform:host/hosted-provider`. dform runs it in wasmtime when
built with `--features wasm` (`dform version` says `wasm host in`); a
`.wasm` source, or a directory holding `provider.wasm`, is one. Its
imports are its manifest, and nothing reaches it that it does not
import: no environment variable, no file, no socket without a grant.
The provider's calls are async functions (the component model's async
ABI, WASI 0.3's; wit-bindgen's `async` and wasmtime's
`component-model-async`): one instance has every call dform submits in
flight at once, and an Apply answers a `stream<event>` and a `future` of
its result. The SDK runs a `Handler`, which is synchronous, one call at a
time, so `--parallel` still serializes for an SDK component, and its
events reach dform as the call returns; the WASI imports are 0.2's, which
the wasm32-wasip2 target links (wasm32-wasip3 is not a target stable Rust
ships yet). A component's
exit carries no code (`exit status: 1`). Its compiled code is cached in
`$XDG_CACHE_HOME/dform/wasm/` by the file's digest.

The fake provider is built both ways and passes `dform provider check`
on both; tests/host_wasm.rs keeps them from drifting.

| provider | written with | hosted | page |
|---|---|---|---|
| `postgres` (`crates/dform-provider-postgres`) | the SDK's typed layer | native only: it dials the server itself | docs/providers/postgres.md |
| `vault` (`crates/dform-provider-vault`) | the SDK's typed layer, a scheme and no resource | native: every request through the host's HTTP client | docs/providers/vault.md |

## Grants and credentials

In `dform.toml`:

```toml
[providers.k8s]
source = "providers/k8s"
# Beyond the host's own interfaces: wasi:filesystem, wasi:http, wasi:sockets.
allow = ["wasi:sockets"]
# Credentials it may open by name, KIND:NAME.
credentials = ["kubeconfig:prod"]
# Locations its io.read may take, SCHEME://HOST/PATH with `*`.
reads = ["https://github.com/*"]
```

A read of a location no `reads` pattern matches is refused: `provider
k8s is not granted a read of https://other.example/x: add a pattern that
matches it to [providers.k8s] reads in dform.toml`. A project's own file
is never a provider's to read. A provider that reads a scheme of its own
(`gs://`) declares it, `schemes = ["gs"]` in its manifest (the gRPC
`Manifest`'s `schemes`; a `Handler`'s `schemes` and `read_location` in the
SDK, or a typed provider's `Provider::SCHEMES` and `Provider::read`), and serves `io` for it (the gRPC `Io` service, and `Files`, its name
before R-155; the WIT world
`scheme-provider` exports it): dform routes a read of the scheme to it, the
program's (`yaml.decode(io.read("gs://.."))`) and another provider's alike, through the
host, never one provider to another; dform's own schemes are never a
provider's. A source that keeps versions (a secret manager) answers the
version with the bytes (`io.read-versioned`, gRPC `Io.ReadVersioned`, the
SDK's `Document { bytes, version }`), and a provider's own read of such
a location gets it too (`Calls::io_read_versioned`, `Host.ReadVersioned`); dform records it in the plan file
and refuses `apply PLAN` once it moved (R-172). The Vault provider is the
reference (`vault://`); a 1Password provider would declare `op` and read
`op://VAULT/ITEM/FIELD` over 1Password Connect's HTTP API, a fnox one
`fnox`, running fnox itself if its author chooses: dform never does.

The host's interfaces need no grant. A component that imports
`wasi:sockets`, `wasi:http` or `wasi:filesystem` is refused unless
`allow` lists it, naming the provider and the interface. With the grant
it gets the machine's network, or the filesystem from `/` (relative
paths are dform's working directory's), and gives up for what it does
there the host's TLS, retries, waits and audit: a trade made in the open.

A credential is a name; its kind says how the host applies it:

| kind | the value | applied as |
|---|---|---|
| `bearer` | a token | `Authorization: Bearer TOKEN` |
| `basic` | `USER:PASSWORD` | `Authorization: Basic ..` |
| `header` | `NAME: VALUE` | that header |
| `tls` | PEM: a certificate chain and its key | the TLS session's client certificate |
| `kubeconfig` | a kubeconfig | its current context's token or client certificate, and its cluster's CA; its server is the credential's `endpoint` |
| `ssh` | an unencrypted OpenSSH private key, in the operator's file only (or an agent's key by its comment or fingerprint) | by dform's SSH client for an `ssh://` or `git+ssh://` location, never to an HTTP call: `[io] credentials = { "ssh://HOST/*" = "ssh:NAME" }` (docs/reference.md, "Locations and transports") |

The value comes from the program (`use k8s { kubeconfig =
cluster.kubeconfig }` registers the secret under the name the grant
lists, R-45's reveal), else the operator's file
`$XDG_CONFIG_HOME/dform/credentials/KIND/NAME` (`DFORM_CREDENTIALS`
names another directory). It stays in dform's memory, is never
serialized to a provider, and is zeroed when dropped. A provider opening
a credential it was not granted is refused: `provider k8s is not granted
the credential kubeconfig:staging: add it to [providers.k8s] credentials
in dform.toml`.

`dform provider check` prints how a provider is hosted, beside the
conformance cases:

```
host  wasm: imports: host http, secrets
host  wasm: imports: beyond the host wasi:sockets
host  wasm: granted: wasi:sockets
host  wasm: credentials: kubeconfig:prod
```

## Not yet

- `ssh.exec`, `ssh.write` and `ssh.forward` are not wired to dform's SSH
  client yet (no provider's apply calls them); every such call is refused
  saying so. A host's file is read through `io`.
- `git.commit` writes to a local repository; pushing to a remote is
  refused (gitoxide has no push yet). A remote is read through its
  mirror, fetched over dform's HTTP client or its SSH client.
- `io.read` answers a `list<u8>` in a component and a stream of chunks
  over gRPC; a component's `stream<u8>` waits on the host interfaces'
  move to WASI 0.3's async, with the rest of `dform:host`. `io` has no
  write, and will not (R-155): a write is an effect with no state to
  converge, a provider's apply.
- A wasm component cannot declare a scheme yet: the wasm host does not
  call a component's `io` export (a native provider's `Io` is called).
- A component's calls have an epoch deadline of an hour, a backstop
  under the provider's `timeout`, which answers the engine first.
