# Writing a provider

A provider is what dform asks about the world: it reads objects, plans a
change to one, applies it. The protocol it speaks is
`wit/dform-provider.wit`. This page is how to write one
with the SDK (`crates/dform-sdk`), what the host gives it, how the
operator grants it, and the two ways it runs.

## The host does the transport

A provider does protocol logic; dform does the transport. Everything a
provider would otherwise reimplement, and every place a credential would
reach it, is a call to the host (`wit/host/dform-host.wit`, `dform:host`):

| interface | what it does |
|---|---|
| `http` | a request with TLS by the host: the machine's CA and proxy (`HTTPS_PROXY`, `NO_PROXY`), a credential applied (a header, a client certificate and its CA), optionally through a tunnel |
| `io` | `read` a location, whatever its scheme (read-only: a write is a provider's apply): `ssh://USER@HOST/PATH`, `https://`, `git+https://HOST/OWNER/REPO/PATH?ref=TAG` through a mirror in `$XDG_CACHE_HOME/dform/git/<host>-<owner>/<repo>.git/`, `s3://BUCKET/KEY`, a scheme another provider declares (docs/reference.md, "Locations and transports"); as dform.toml grants |
| `ssh` | `exec` (argv, never a shell string), `write` a file, `forward` a port to a tunnel; keys from the operator's agent (a file is read through `io`) |
| `git` | a commit onto a branch of a local repository (a file is read through `io`) |
| `secrets` | open a credential by name: a handle, never the value |
| `log` | a line shown with dform's own output |

A read of something the world has not reached yet (a file cloud-init has
not written, a host still booting) answers `not-yet`, distinct from an
error, so the engine waits on it, within the provider's `wait` (10m
unless dform.toml says), not a call's `timeout`. An error carries a class: `final`, `retryable` (nothing changed,
worth sending again) or `maybe-applied` (no answer came; it may have
taken effect).

A provider is a worker and dform is its runtime, as an edge runtime is
a worker's: the provider gets these `fetch`-shaped capabilities and
nothing else, and the host owns what is under them: the connections,
the pace, the credentials, the tunnels, the trace, the timeouts. A
component has nothing else to reach for (its imports are these, and
`wasi:*` only as granted). A native provider keeps the same contract
over gRPC by sending through the SDK's `Http` rather than a client of
its own; one that holds its own client opts out of all of the below.

What the host guarantees a request:

- one connection per endpoint for the whole process, its requests the
  streams of one HTTP/2 connection where the server offers it, else
  kept-alive HTTP/1.1; TLS with the machine's CA and the credential's;
- a pace: a window of requests in flight per account and bucket that
  grows while the server answers, halves on a 429, a 503, a timeout or a
  stall of the connection, waits out a `Retry-After`, and never passes
  the streams the server takes at once;
- a request the connection lost before the server took it, sent again
  on a new one; a call that failed `retryable`, sent again by dform's
  retry policy;
- a timeout (60s unless the request says), and a span in the run's
  trace under the call it is made for.

What a provider author may assume, and leave to it: no sockets, no
client to configure, no credential values (a handle names one), no
retries of its own beyond classifying its errors (a 429 comes back as
the server answered it; sent again, it waits its turn), no rates: it
names its limits (`Request::bucket("ec2.describe")`) and the window
learns them. What a call awaits is the call's (paced in its session's
windows, traced under its span), however many it awaits together; a
thread it starts by itself is in no call.

To place it: a Cloudflare Worker or a Fastly Compute service gets
`fetch` and bindings and never a socket, and the platform pools its
connections and holds its secrets; Chrome's renderers likewise reach the
network only through its network process. dform is that runtime for
infrastructure APIs. What it adds is the pace: a provider's requests
come from one plan, not from many users, so the host sees them together
and paces each account's API by what it answers.

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
    async fn configure(settings: &serde_json::Value) -> Result<(Acme, Option<String>)> {
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
    async fn read(p: &Acme, remote: &str) -> Result<Option<Bucket>> {
        let get = |path: String| host().http.request(Request::get(path).auth(&p.token));
        // One request awaited: what it answers decides the next.
        let r = get(format!("{}/buckets/{remote}", p.endpoint)).await?;
        if r.status == 404 {
            return Ok(None);
        }
        let mut bucket: Bucket = serde_json::from_slice(&r.body)?;
        // Two awaited together: both in flight, the host pacing them.
        let (tags, policy) = dform_sdk::join(
            get(format!("{}/buckets/{remote}/tags", p.endpoint)),
            get(format!("{}/buckets/{remote}/policy", p.endpoint)),
        )
        .await;
        bucket.tags = serde_json::from_slice(&tags?.body)?;
        /* .. policy? .. */
        Ok(Some(bucket))
    }
    async fn create(p: &Acme, desired: Bucket, key: &str, progress: &Progress<'_>) -> Result<(String, Bucket)> {
        // .. POST it, then poll until it is ready, saying each status seen,
        // waiting between polls with `dform_sdk::sleep(..).await`:
        progress.status("PROVISIONING");
        /* .. */
    }
    async fn update(p: &Acme, remote: &str, prior: Bucket, desired: Bucket, progress: &Progress<'_>) -> Result<Bucket> { /* .. */ }
    async fn delete(p: &Acme, remote: &str, progress: &Progress<'_>) -> Result<()> { /* .. */ }
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
  change), `name_like`; `list_key = "name"`. On the struct: `type`
  (required), `replace`, `retry`, `lookup = "name,region"` (its
  `type_lookup`, below), `remote_name = "name"` (its `type_remote_name`,
  below), `lifecycle = "retain"` (its `type_lifecycle`, below), `health`
  (it answers Health, below). A computed field is an `Option` with
  `#[serde(default)]`: the engine's desired document does not have it.
  An `optional_computed` one is answered as computed (dform compares it
  only where the program writes it), so leave it out of what Read
  answers when unset: `skip_serializing_if = "Option::is_none"`.
- A reference is the resource: a program writes `vpc = main` and the
  provider is sent `main`'s id. So dform names an attribute typed
  `ref(T)`, or a list or set of them, for the resource: a name ending in
  `_id` is the program's without it (`vpc_id` is `vpc`, `peer_vpc_id`
  `peer_vpc`), and a list or set named `_ids` is the plural
  (`security_group_ids` is `security_groups`). The provider keeps its
  own name: every document, path and inventory row it is sent or
  answers carries it, and dform renames at the boundary. A program that
  writes the provider's name is told the program's. Name a reference
  for the thing in a new schema (`network`, `instance`), as the OVH and
  Kubernetes providers do.
- Identity, replacement and the lookup are three things. An
  object's identity is its `id`: state holds it, and an update goes to
  it. An attribute is `force_new` only if the API cannot change it on
  the object's id; that it names the object to a person, or is what a
  lookup matches, is no reason. What a Create that timed out is found
  by (dform asks `provider.created` by the Create's idempotency key, and
  how the provider finds what that key made is its own) is the content
  key it lists and matches, declared in the schema as
  `type_lookup(Type, [Attr, ..])`: the OVH provider finds a DNS record
  by its zone, subdomain, type and target, and changes the subdomain,
  target and ttl in place on the record's id. dform carries the fact
  and does not read it; a provider reads its own.
- Where an object's name is its identity (an `id` path the program
  writes, a Kubernetes `metadata.name`), a replacement created before
  the old object is deleted needs another name. A provider that can make
  an object under another name than the program's says so per type,
  `type_remote_name(Type, Path)`: dform then sends a create-first
  Replace the next generation of the program's name at Path
  (`forgejo-3`), records it, and answers it to every read of the name; a
  type with a named identity and no such fact is refused `create_first`
  at plan. Declare it only where nothing names the object literally
  (a Kubernetes Service's name is a DNS name: not it). dform reads this
  fact; the provider just makes what it is sent.
- What removal from the program means for a type is `destroy` unless
  its schema says `type_lifecycle(Type, "retain")`: dform then forgets
  an object of it the program no longer makes and sends no Delete. The
  fact seeds the row `lifecycle(r, "retain")` for each resource of the
  type, which a program's own word for it replaces (docs/reference.md,
  "Lifecycle"). Declare it where the object is not the program's to
  remove: a Tailscale device, which its node joined to the tailnet. The
  Kubernetes and OVH providers declare none.
- `Lifecycle` is the four calls, and `check`, a refusal of a document
  the provider cannot apply as configured (Postgres refuses to manage the
  role it connects as), called by Plan once the provider is configured
  and by Apply before it changes anything. Plan is not written: the SDK
  diffs leaf by leaf with the schema, a change to a `force_new` path
  replaces, a missing `required` attribute refuses. Import is a read.
- `Provider::configure` gets the program's `use NAME { .. }` settings,
  each secret revealed into them, `{}` when the program writes none.
  It makes the provider of one session: a configured instance, one per
  deployment of the run (docs/reference.md, "Providers are processes").
  The SDK keeps one per session and answers each call with its
  session's, so two deployments' calls in flight never meet; a session's
  calls come one at a time, two sessions' at once. Keep nothing of a
  session's outside the value `configure` returns. A location read
  (`Provider::read`) names no session: the session opened last answers
  it.
  Until they are known (a kubeconfig another deployment outputs), the
  provider is configured `deferred` and still asked to Plan, against
  the schema it serves offline. A setting that only reaches what the
  provider serves, and says nothing of what it makes, is its connection:
  its schema says so with a `provider_setting(NAME, SETTING,
  ["connection"])` fact (`["sensitive", "connection"]` for a secret
  one). The k8s provider's are `kubeconfig`, `host`, `ca`, `token`,
  `client_certificate` and `client_key` (its kubeconfig credential's
  parts, "Grants and credentials"), not `namespace` (an object's
  default). When every setting a provider waits on is its connection,
  the plan shows its objects as planned, documents and diffs, under
  `later` marked `provisional: planned against the offline schema;
  planned again once kubeconfig is known`, counts them, and records
  the mark in the plan file; the run that has the settings plans them
  again against what they reach. A provider that flags none
  has every setting as its connection: its offline plan is the only
  one there is until then. One that waits on another setting (the
  namespace) holds its objects without the mark.
- `apply` gets a sensitive attribute's bytes when the secret is another
  provider's (a Tailscale key in an instance's `user_data`), whole or
  inside a string template: dform reveals it into the document for that
  call only. Plan sees it as dform has it: a secret as its label,
  `{"$secret": "T/N#P"}`, a template with each secret held in place. One
  the provider holds itself comes as its label in Apply too, for it to
  read where it keeps it.
- `const KEEP: bool = true` on the `Provider` says each `update` sends
  only what `desired` holds, leaving an absent attribute as the object
  has it. The provider then has the `keep` capability: an Apply update's
  `keep` paths (a write-only secret a run without the deployment's master
  proved unchanged) come filled from what Read answers, or absent
  where it answers nothing. Without it a `keep` is refused, and dform
  sends none: such an update needs the master.
- A typed provider has the `offline` capability: its Plan with no
  credentials checks the desired document against its schema, as with
  them but for `Lifecycle::check`, so `dform test` asks it. A
  provider of its own declares `offline` in its handshake when its Plan
  answers with no credentials as it would with them (from a schema it
  holds, or a fake of its API); one that does not is not asked under
  test, which says so.
- Read is told the object's prior computed values, as the last apply
  or read returned them (docs/reference.md, "Providers are
  processes"): `Lifecycle::read_with_prior(p, remote, prior)`, which
  is `read` unless a type overrides it. A type that makes something
  below its object that the API does not mark as its own (a port on an
  instance) answers its id under computed when it makes it, and finds
  it again by that id here; one the prior names that is gone is
  absent from what it answers.
- Health is a call of its own, beside Read, and `dform status`
  is the only one that asks it: an object's state now, one of
  `healthy`, `progressing`, `degraded`, `suspended` and `unknown`, and a
  reason in one line, never a sensitive value. `#[dform(health)]` on
  the struct lists the type in the handshake's `health`, and
  `Lifecycle::health(p, remote)` answers it (`dform_sdk::health(state,
  reason)`); the default answers none, which is `unknown`. Judge from
  what the API says now and never measure time: `progressing` is on its
  way and may get there by itself (a rollout, `BUILD`), `degraded` is
  failing and will not get better without a change (the API gave up:
  `ProgressDeadlineExceeded`, `BackoffLimitExceeded`, `ERROR`; or it
  names a failure: `CrashLoopBackOff`; or the object is gone),
  `suspended` is stopped on purpose (`spec.paused`, `SHUTOFF`). A
  provider that must poll its API to answer polls inside the call;
  dform never polls it. A type with nothing to judge (a key, a record, a
  ConfigMap) declares none, and `status` prints `-` for it. Health is
  not a value a program reads, not in the plan, the apply or state:
  dform asks only which objects have it and prints the answer. A
  provider of its own answers `Health` (gRPC `rpc Health`, the WIT's
  `health`) with one answer per object asked, in order, and lists the
  types in its handshake's `health`; the Kubernetes and OVH providers
  do (docs/reference.md "Targets and commands"). Postgres and Vault
  declare none.
- `Typed::example::<R>(create, update, required)` gives `dform provider
  check` documents of the provider's own types to run its resource
  checks with.
- Create, update and delete get a `Progress`: `progress.status("BUILD")`
  each time the object's status as the API gives it changes (a poll that
  saw a new one), `progress.message(..)` for the log. dform prints the
  status beside the change in apply's progress, as it is, with the time
  the call has run; say it when your view changes, never on a timer.
  Over gRPC it is the Apply's stream of events, in a component the
  `stream<event>` its `apply` answers. A `Handler` gets the same
  sink as `handle`'s second argument.
- An error refuses the call. One the host classed `maybe-applied` is
  `MaybeApplied` (dform looks before it sends it again); a `retryable`
  one says so, and dform's retry policy sends it again.
- `provider!(EXPR)` is the entry point. Any `Handler` will do: the fake
  provider (`crates/dform-provider-fake`) is `provider!(dform_mock::Mock::process())`,
  the mock answering every call itself. `Handler::handle` is told each
  call's session: what serves it numbers them (a Configure that names
  none opens the next) and puts it in the Configure's answer. A
  `Handler` keeps one configuration per session (`backend::Configured`
  holds them: the mock keeps a cloud per session, the OVH provider an
  `Ovh`) and refuses a call of a session no Configure opened. A
  provider of its own over gRPC answers each Configure with the session
  (`ConfigureResponse.session`, never 0: dform refuses a provider that
  answers none) and reads each call's `session`, as the Kubernetes
  provider does.

- A type whose objects the API makes (a device joins a tailnet) says why
  in `Lifecycle::NOT_CREATED`: Plan refuses a create of one, naming the
  address, and a program adopts it. `Lifecycle::adopt(p, given)` answers
  the remote id of what a program adopts by a name that is not its id
  (a hostname), and refuses a name two objects have; state keeps the id.
- A change the API cannot make in place where no `force_new` path says
  so (a size it only grows) is `Lifecycle::replaced(p, prior, desired)`:
  why, in one line, which Plan answers as a replace whose reason the
  plan prints under the change. A `force_new` change needs none: the
  plan names the path.
- `progress.note(..)` is something the user sees under the change once
  the call is done (a destroy that wrote a default rather than removing
  anything).
- A `sensitive` computed value leaves the provider as its label: the
  provider keeps the bytes (an API that answers a key once, to its
  create) and `Provider::reveal(held)` answers them to the engine, under
  the deployment's lease only, for the one call that takes them.
- A data source is an `extern_decl` fact given to
  `Typed::facts_text` (`provider_setting` facts too), answered by
  `Provider::query(pred, inputs)`; asked before the program's settings
  came, the SDK answers it not yet. It may take one of the provider's
  types' names (`tailscale.device`): the relation `T(..)` is then
  what the provider lists, managed or not, and `x in T` stays the
  program's own resources of `T`, so a policy over them never ranges
  over the account.

Not yet in the typed layer: refinements on secret paths (an Apply with
assertions is refused saying so), the `managed` capability, and schema
docs from `///`.
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
`--parallel`'s Applies); natively the SDK answers each on a thread of
its own, and each host call a handler awaits waits on a thread of its
own, so what it awaits together is in flight together. A `Handler` (or
a `Lifecycle`) is called several times at once and keeps any state of
its own behind its own lock (it is `Send + Sync`); a lock held across
an `.await` is `dform_sdk::sync::Mutex`, never std's (below).

**Component** (experimental): `cargo build --target wasm32-wasip3
--features component` makes `target/wasm32-wasip3/debug/<crate>.wasm`, a
component of `dform:host/hosted-provider` (the target is nightly's: the
workspace pins one in rust-toolchain.toml). dform runs it in wasmtime
when built with `--features wasm` (`dform version` says `wasm host in`);
a `.wasm` source, or a directory holding `provider.wasm`, is one. Its
imports are its manifest, and nothing reaches it that it does not
import: no environment variable, no file, no socket without a grant.
The provider's calls are async functions (the component model's async
ABI, WASI 0.3): one instance has every call dform submits in flight at
once, and an Apply answers a `stream<event>` and a `future` of its
result, each event written as the provider says it. The host's calls
that wait on the world (`http.send`, `io.read`, `ssh.exec`, and
`wasi:clocks`' wait under `dform_sdk::sleep`) are async too, each a
subtask: a `Handler` awaits them, and the instance answers its other
calls meanwhile, so `--parallel` holds for a component as natively.
What a call awaits together (`dform_sdk::join`, `join_all`) is in
flight together with no thread, dform's window pacing it. A synchronous
handler (a `backend::Handler`, or one waiting with
`dform_sdk::block_on`) holds the instance while it waits: its other
calls wait for it. On wasip3 every call is a task of its own, with its
own thread-locals, so what an instance keeps is in statics, and the
calls take turns on one thread: a std lock held across an `.await`
would never be released to the call waiting on it. A component imports
`wasi:filesystem` only with the SDK's `filesystem` feature (it then
starts in dform's working directory, as a native provider does), so
one that reads no file needs no grant. A component's exit carries its
code. Its compiled code is cached in `$XDG_CACHE_HOME/dform/wasm/` by
the file's digest.

The fake provider is built both ways and passes `dform provider check`
on both; tests/host_wasm.rs keeps them from drifting.

| provider | written with | hosted | page |
|---|---|---|---|
| `postgres` (`crates/dform-provider-postgres`) | the SDK's typed layer | native only: it dials the server itself | docs/providers/postgres.md |
| `vault` (`crates/dform-provider-vault`) | the SDK's typed layer, a scheme and no resource | native: every request through the host's HTTP client | docs/providers/vault.md |
| `tailscale` (`crates/dform-provider-tailscale`) | the SDK's typed layer: a tailnet's policy file, auth keys (held), DNS and devices (adopted), its users and devices data sources | native: every request through the host's HTTP client | docs/providers/tailscale.md |
| `ovh` (`crates/dform-provider-ovh`) | the protocol's `Handler` directly, its own signing | native: every request through the host's HTTP client | docs/providers/ovh.md |
| `k8s` (`crates/dform-k8s`) | the protocol's gRPC service directly, a thin client of the Kubernetes API | native: every request through the host's HTTP client, its kubeconfig a credential the host holds | docs/reference.md, "The Kubernetes provider" |

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
SDK, or a typed provider's `Provider::SCHEMES` and `Provider::read`), and serves `io` for it (the gRPC `Io` service, and `Files`, its earlier
name; the WIT world
`scheme-provider` exports it): dform routes a read of the scheme to it, the
program's (`yaml.decode(io.read("gs://.."))`) and another provider's alike, through the
host, never one provider to another; dform's own schemes are never a
provider's. A source that keeps versions (a secret manager) answers the
version with the bytes (`io.read-versioned`, gRPC `Io.ReadVersioned`, the
SDK's `Document { bytes, version }`), and a provider's own read of such
a location gets it too (`Calls::io_read_versioned`, `Host.ReadVersioned`); dform records it in the plan file
and refuses `apply PLAN` once it moved. The Vault provider is the
reference (`vault://`); a 1Password provider would declare `op` and read
`op://VAULT/ITEM/FIELD` over 1Password Connect's HTTP API, a fnox one
`fnox`, running fnox itself if its author chooses: dform never does.

The host's interfaces need no grant. A component that imports
`wasi:sockets`, `wasi:http` or `wasi:filesystem` is refused unless
`allow` lists it, naming the provider and the interface. With the grant
it gets the machine's network, or the filesystem from `/` (relative
paths are dform's working directory's), and gives up for what it does
there the host's TLS, retries, waits and audit: a trade made in the open.
`wasi:http` keeps the host's transport: its requests go through the
host's `send` as `dform:host/http`'s do (dform's pool, the window, a
span), paced in the component's default bucket since such a request
names neither its session nor a bucket; only the credential is not the
host's.

A credential is a name; its kind says how the host applies it:

| kind | the value | applied as |
|---|---|---|
| `bearer` | a token | `Authorization: Bearer TOKEN` |
| `basic` | `USER:PASSWORD` | `Authorization: Basic ..` |
| `header` | `NAME: VALUE` | that header |
| `tls` | PEM: a certificate chain and its key | the TLS session's client certificate |
| `kubeconfig` | a kubeconfig, or its parts (`host`, `ca`, `token`, `client_certificate`, `client_key`) | its current context's token, client certificate or exec plugin's answer (run by dform, again once it expires), and its cluster's CA, `tls-server-name` and `proxy-url`; its server is the credential's `endpoint`, its namespace the credential's `namespace` |
| `ssh` | an unencrypted OpenSSH private key, in the operator's file only (or an agent's key by its comment or fingerprint) | by dform's SSH client for an `ssh://` or `git+ssh://` location, never to an HTTP call: `[io] credentials = { "ssh://HOST/*" = "ssh:NAME" }` (docs/reference.md, "Locations and transports") |

The value comes from the program or the operator. A provider declares
which of its settings are credentials (the handshake's
`setting-decl.credential`; in the SDK, `Typed::credential("token",
"bearer")`): the settings of one kind that a Configure gives (`use k8s {
kubeconfig = cluster.kubeconfig }`, a secret revealed into that
Configure) are held by dform as one credential for that provider, under
a name of its own (`kubeconfig:k8s.1`), and each setting reaches the
provider as that name, which it opens; the next Configure of the session
lets it go. Only the provider it was held for opens it. Else the value is
the operator's, as dform.toml grants it: the file
`$XDG_CONFIG_HOME/dform/credentials/KIND/NAME` (`DFORM_CREDENTIALS` names
another directory), or `KIND:environment`, the environment's of a kind
that has one (`kubeconfig:environment`: `KUBECONFIG`'s files, else
`~/.kube/config`, else a pod's service account, read by dform). A
provider the program does not configure is told that name in the
Configure's `credentials` (`{"kubeconfig": "kubeconfig:environment"}`),
and opens it only with the grant: nothing in the environment reaches a
provider unless the program or dform.toml gives it.
Either way it stays in dform's memory, is never serialized to a
provider, and is zeroed when dropped. A provider opening a credential it
was not granted is refused: `provider k8s is not granted the credential
kubeconfig:staging: add it to [providers.k8s] credentials in
dform.toml`.

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
  over gRPC; a component's `stream<u8>` is to come, additively. `io` has no
  write, and will not: a write is an effect with no state to
  converge, a provider's apply.
- A wasm component cannot declare a scheme yet: the wasm host does not
  call a component's `io` export (a native provider's `Io` is called).
- A component's calls have an epoch deadline of an hour, a backstop
  under the provider's `timeout`, which answers the engine first.
