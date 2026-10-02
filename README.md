# dform (prototype)

`dform` is a tiny Datalog-like language for describing infrastructure intent as:

- facts: inputs and discovered data
- rules: derive desired resources
- constraints: enforce production invariants

This repo currently uses a **fake backend** (no AWS/GCP/Azure) so we can iterate on language + semantics.

New here? Start with the tour: `examples/tour/stacks/tour.df` is a tutorial
read top to bottom, from what Terraform does to what it cannot express,
each section a command to run and what it prints.

```bash
cd examples/tour && cargo run -- plan
```

Authoring guidance: `docs/best_practices.md`.

## Quick start

Every example is a project under `examples/<name>/` (see "Project layout"
below). The demo, `examples/demo/`, is the stack `stacks/dform.df`, which
imports reusable chunks from its `modules/*.df` and `policies/`.

```bash
cargo run -- -C examples/demo plan                  # its one stack, dform, in env's default
cargo run -- -C examples/demo apply dform env=staging
cargo run -- -C examples/demo plan dform env=prod   # or 'dform[env=prod]'
cargo run -- -C examples/demo test                  # the program's denies, over every env
cargo run -- -C examples/demo dev strata            # evaluation order: the partition graph's strata
cargo run -- -C examples/demo dev effects           # per scope: what it reads, writes, offers
cargo run -- -C examples/demo fmt                   # format the project's .df files in place
cargo run -- fmt --check $(git ls-files '*.df' ':!tests/syntax/err' ':!editors')   # CI: list unformatted files, fail
```

`-C DIR` runs as if dform started in DIR; from inside a project the same
commands need no `-C`: `cd examples/demo/stacks && cargo run -- plan`.
`apply` names a keyed stack's deployment (`dform env=staging`), and the
stack when the project has several.

### Project layout

A project is a directory with a `dform.toml` at its root: `stacks/` (one
stack per file), `modules/`, `policies/`, `config/<stack>/<key>.yaml`,
`data/`, `providers/<name>/` and a gitignored `dform.state/`.
The root is the nearest directory up from the working directory holding a
`dform.toml`; `dform init [NAME]` makes one (and puts `dform.state/` in the
nearest `.gitignore`). Every path a program states resolves from the project
root: imports, table and config sources, `file.*` externs, input relations,
provider sources and trust roots. Outside a project, `plan` and the `dev`
views run on a program file with no state; `apply`, `controller`, `stack`,
`state` and `log` refuse (a `dev --world` run keeps its state beside the
world file, and runs anywhere). A stack is a file, named after itself:
`stacks/shop.df` is the stack `shop`. With no `stacks/` directory the
root's `.df` files are the stacks, so `dform.toml` beside `shop.df` is a
project with one stack; any file runs by path, named after itself. A
module or policy file with a `key` is an error, importing a stack file is
an error, and a `.df` outside the layout's directories is a warning. `docs/layout.md` has the convention; every
example under `examples/` follows it, and test-only programs are under
`tests/fixtures/`.
Each example's `README.md` says what it shows and lists the commands to
run from its directory (`dform` there is `cargo run --` in a checkout);
`tests/examples.rs` runs them: plan, apply, and plan again, undeformed.

`dform.toml` is small; programs stay in `.df` files. It holds
the project's name and the dform versions it takes, each provider's source
and version (Cargo's semver syntax: `"2.1"` is `^2.1`), each stack's
operational settings, `[stacks.NAME]` for `stacks/NAME.df` over
`[defaults]`, and globs discovery skips. A stack's settings are a closed
list: `backend`, `role`, `approvals`, `audit_sink`,
`isolated` and `config`; a term is written as a string, `{stack}` the
stack's name and `{k}` the value of its key `k` (in `backend` and
`config`), and any other key is an error naming the list. A
`[stacks.NAME]` no file is is an error. Policy reads it as facts,
`project_provider(Name, Constraint)`, `project_default(Key, Value)` and
`project_stack(Name, Key, Value)`. No inputs and no key values: a
deployment is named by its target.

```toml
[project]
name = "shop"
dform = ">=0.1"

[providers]
aws = { source = "aws-mock", version = "2.1" }   # `provider aws` in a program
gcp = { source = "providers/gcp" }               # a path under the root

[defaults]
backend = 'local("state/{stack}")'   # or 's3("bucket", "dform/{stack}", {...})'
lease_duration = "60s"               # an s3 backend's lease (the default)
lease_renewal = "20s"                # how often its holder renews it (the default)

[stacks.shop]                        # stacks/shop.df
backend = 's3("acme-state", "shop/{env}")'
approvals = 'jwks("https://sso.acme.example/keys")'
config = 'yaml("config/shop/{env}.yaml")'
isolated = true

[discovery]
exclude = ["scratch/**"]
```

### Targets and commands

A command runs on a target: a stack's name (`dform plan infra`), its file
(`dform plan stacks/infra.df`), or one deployment of a keyed stack with its
key (`dform plan 'shop[env=prod]'`, or `dform plan shop env=prod`). Key
values belong to the target; other inputs stay `--set`, and `--set` of a
key is an error. `--set k=@FILE` reads FILE (YAML, JSON or TOML by its
extension, or a `.df` file of the one fact `k(value)`) as the input's
value, parsed as its type the way a YAML cell is: how the outside gives an
object or a list; a plan file records the file's digest. With no target it
is the one stack under the working directory, else the stacks are listed
and dform exits non-zero. A key the
target does not name is its input's default, for `plan` and `apply` alike;
both print the deployment first, `deployment: shop[env=dev] (env from its
default)` (`plan --json`: `deployment` and `key_defaults`). `controller run`,
which runs unattended, names every key value (`dform controller run shop
env=prod`). `apply` also takes a plan file (`dform apply plan.json`).

`apply` prints the plan and asks `Apply these N deformations to
shop[env=prod]? [y/N]`; only `y` or `yes` proceeds. `--yes` (`-y`)
applies without asking, as a script or CI does: with no terminal to ask on
and no `--yes`, apply refuses at once, naming the flag. An undeformed plan
asks nothing, nor does `apply plan.json` (the file was reviewed; approvals
guard it) or the controller. Nothing is written before the answer: an
apply reads (refresh, the lookups that resolve uncertain calls), plans and
asks, and only a `y` writes state (`moved` renames included) or calls
Apply. A later tick whose plan holds an address no earlier tick listed (a
pending group's member: `iam.policy[?]` at tick 1, named once the endpoint
it is built from exists) asks again: its plan is printed, `tick 2:` and
the report, then `Apply N new deformations to D? [y/N]`, counting the
addresses no earlier tick listed. A `n` there stops the apply with what
the earlier ticks did in state; the audit log's `apply_end` says
`declined` and the `tick`, and the next apply resumes. An unattended
apply (`--yes`, `apply plan.json`, `--approval`) has nobody to ask: it
applies the ticks whose addresses the plan named and stops before the
first tick that would add one, after writing state: `apply stopped after
tick 1: tick 2 adds 1 deformation the plan could not name
(iam.policy[?] on ?db.postgres["orders"].endpoint); run apply again to
plan them against the world as it now is`. It exits non-zero, nothing
applied that was not printed, and `apply_end` says `stopped`; the next
apply plans them as its tick 1, by name. There is no strict mode and no
resource-level target: a plan that needs a second tick applies tick by
tick, and a program that wants to apply part of itself is two stacks.

The stack is the unit of partial work. `apply X` in a project applies the
deployments X reads (`stack_output("platform[env=prod]", ..)`, a name
written out or built from X's own key, `"platform[env=${env}]"`) first,
and theirs before them, each a run of its own with its own plan,
confirmation and state, then X; nothing that reads X. The first line
says so: `apply shop[env=prod]: platform[env=prod] first, each with its
own plan and state: shop[env=prod] reads its outputs`, and each run is
headed `== NAME`. A dependency whose apply fails or is declined stops the
run before its reader; stacks that read each other are an error naming
the cycle. A `--set` goes to each stack of the run that declares the
input (one none declares is the target's error); `--input-file` is the
target's. A plan file,
a `--world` fixture and a program outside a project apply only
themselves.

| Commands | |
|---|---|
| `plan`, `apply`, `why`, `query`, `diff`, `test`, `fmt`, `log` | on a target |
| `stack list`, `stack rekey`, `stack handover`, `stack unlock` | the project's stacks |
| `state show`, `state taint`, `state mv` | a deployment's state |
| `provider check`, `provider schema` | providers |
| `controller run` | controller mode |
| `dev strata`, `dev graph`, `dev effects`, `dev --world W --inventory I --provider P --chaos C COMMAND` | the mock and the evaluator |
| `doc [TARGET]` | the doc comments as Markdown, on stdout |
| `init [NAME]` | make the working directory a project |
| `completions zsh\|bash\|fish` | a completion script |
| `lsp` | the language server, on stdin and stdout (see "Language server") |

`dform stack list` shows every stack, its key and file, and per deployment
with state its last apply (time, actor and the project's commit, from the
audit log) and a saved plan not yet applied. `dform state show TARGET`
prints the deployment's objects, `--address ADDR` only the one at `ADDR` (it, `state mv`, `log` and `stack unlock`
need the deployment's key, not the program's other inputs); `dform state mv FROM TO TARGET` gives the
object at the address `FROM` the address `TO`; `dform stack unlock TARGET` removes
an apply lock whose holder is gone (breaks an s3 backend's lease). `dform provider schema NAME` prints a
provider's schema facts. `dform doc` prints the project's doc comments
(`#|` lines above an item, docs/grammar.md "Doc comments") as Markdown: per
file, each documented item's kind and name, its first line, its
description and its other keys (`owner`, `since`, `deprecated`, ...);
`dform doc TARGET` only its program's files (the stack's and every file
it imports). `dform completions zsh > _dform` completes stack
names, key values (from the key inputs' enum types) and deployments with
state.

State is scoped to a stack. One program is one stack, named after its
file (`stacks/demo.df` is `demo`), and its operational settings are
`dform.toml`'s `[stacks.demo]`, over `[defaults]`:

```toml
[stacks.demo]
backend = 'local("state/demo")'   # where state, world and lock live, relative to the
                                  # project root; default dform.state/<name>; or
                                  # s3(...), see "State backends"
role = "bootstrap"                # optional: it stays batch; see "Bootstrap and handover"
approvals = 'jwks("https://...")' # optional: who may approve a plan; see "Approvals"
audit_sink = "logger -t dform"    # optional: each audit entry to a command; see "The audit log"
isolated = true                   # a keyed stack's deployments do not share names
config = 'yaml("config/demo/{env}.yaml")'   # a keyed stack's settings per deployment
```

Two programs never see each other's resources. `apply` holds the
stack's lock, `<state dir>/state.lock` (the holder's pid): a second apply
of the same stack while one runs fails naming the holder; a lock whose
holder is gone (a killed apply) is taken over with a note.

### State backends

`backend = 'local("DIR")'` (the default, `dform.state/<stack>`) keeps a
deployment's files in a directory. `backend = 's3("BUCKET", "PREFIX",
{endpoint: "URL", region: "R"})'` keeps them in an S3 bucket under PREFIX
(a keyed stack's deployment under `PREFIX/<k>=<v>`, unless the backend
names the key, `s3("acme", "shop/{env}")`, and each deployment is where it
says): the state (identity,
in-flight and uncertain records, outputs), the plan key `state.key`, the
audit log (in segments, `state.audit/000001.jsonl`, ...: see "The audit
log"), the lease `state.lock`, the published outputs `outputs.json` and the
controller's memo `controller.json` (and its `approvals/` drop directory
and `approval-pending.json`). The record is
optional: without an endpoint it is AWS S3's regional endpoint
(virtual-host style), with one the URL path-style (MinIO, OVH Object
Storage, anything S3-compatible); the region defaults to `us-east-1`. In
`[defaults]` and `[stacks.NAME]`, `{stack}` is the stack's name and `{k}`
the value of its key `k`. The mock's world, the inventory and the cache
stay under `dform.state/`: they are the provider's and the machine's, not
state.

```toml
[stacks.net]
backend = 's3("acme-dform", "prod/net", {endpoint: "https://s3.gra.io.cloud.ovh.net", region: "gra"})'
```

Credentials come from `DFORM_S3_ACCESS_KEY_ID` and
`DFORM_S3_SECRET_ACCESS_KEY` (and `DFORM_S3_SESSION_TOKEN`), else AWS's
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`; only
the environment (no profile file, no instance metadata).

Every write is conditional (`If-Match` on the ETag this run read, or
`If-None-Match: *` for a new object), so two writers never silently
overwrite each other; the server must support conditional PUTs (AWS S3
since November 2024, MinIO). The lock is a lease: an object holding the
holder, an expiry and a fencing counter. An apply takes it (refused, naming
the holder and when its lease expires, while another's is live), renews
it every `lease_renewal` from a thread (one Apply call, a cluster's
create, can outlast a lease), and releases it at the end. A lease that
expired (its holder was killed) is taken over, with a note, and the
counter goes up; the new holder writes its counter into the state at
once, and every state write first checks that the lease is still its own.
A holder that stalled past its lease and wakes after a takeover is
refused ("state write refused by fencing") and writes nothing; the new
holder resumes the interrupted apply as after any crash. The lease is also
checked before each Apply call is submitted, so a stale holder that can see
its lease is gone makes no provider call ("no provider call was made"),
and before the published outputs, the controller's memo and its approval
digest are written ("the controller's memo was not written", "the
approval digest was not written"); a controller whose apply failed, its
lease released, keeps that memo in memory, starts its next run from it,
and writes it once that run holds the lease. What a stale holder can still do: the calls it submitted before the lease
was lost carry on at the provider (at most `--parallel` of them; none is
recalled), and a call whose check passed is sent however long the holder
stalls between the check and the send. Neither answer can be written down;
the new holder finds them as uncertain calls and resolves them by their
idempotency keys, as after a crash. `dform stack unlock` breaks a lease
whoever holds it. The machines sharing a backend
must agree on the time to well within a lease: expiry is wall-clock.
`[defaults] lease_duration` and `lease_renewal` (`500ms`, `30s`, `2m`; 60s
and 20s by default) set the lease; the renewal must be shorter.

Before it writes to a bucket (a `plan` too, when it makes the plan key
for a plan that needs an approval), dform checks once that the server keeps the
conditions (a probe object under the prefix: written with `If-None-Match:
*`, written over with each condition, deleted) and refuses a server that
ignores `If-Match` or `If-None-Match`, naming which; a pass is remembered
per endpoint and bucket in `dform.state/cache/s3-conditions/`. OVH Object
Storage and any other S3-compatible service is taken on that check.

Every command takes an s3 stack as it takes a local one: `plan`, `apply`,
`controller run` (its memo in the bucket), `stack rekey` and `stack
handover` (they move the deployment's objects between prefixes, or between
a bucket and a directory), `state taint`, `state show`, `state mv`, `log`,
`stack unlock` and `stack list` (which lists the deployments under the
stack's prefix), and other stacks read its outputs (the registry records
`s3://BUCKET/PREFIX/state.json` with the endpoint and region).

### Keyed stacks: one deployment per key value

`key env: T` (and `key region: T`, a composite key in source order)
declares an input that is deployment identity: an input in every other
respect (typed, a cell, read as `env`), given by the target, never
`--set`, and never a secret. Each value of the key is its own deployment,
`app[env=prod]`, with its own state directory (`dform.state/app/env=prod/`;
several keys are joined, `env=prod,region=us-east1`, and a value is escaped
for the file system: every byte but letters, digits, `-`, `_` and a `.`
that does not lead is `%XX`), lock, registry entry and controller. Inputs
outside the key are parameters of a deployment: they deform it in place.
The key's value comes from the target, an `--input-file`, else its
default; a key with none is an error naming it. `plan`
and `apply` name the deployment on their first line, and say which key
values are defaults. A key whose default is `"prod"` or `"production"` is
a lint warning: a run that forgets the key would be of production.
Nothing about environments is built in: `dform.df` says `key env:
environment`, so `plan dform env=prod` plans prod against prod's state,
not staging's.

```bash
cargo run -- -C examples/demo apply dform env=staging   # dform[env=staging]: dform.state/dform/env=staging/
cargo run -- -C examples/demo plan dform env=prod       # dform[env=prod]: creates, beside staging
cargo run -- -C examples/demo stack rekey dform env=staging env=stg   # move a deployment's state
```

`dform stack rekey STACK K=V... K=V...` moves one deployment's state (and
its registry entry) to another key value: the old key's pairs, then the
new key's. Nothing in the cloud changes. First it lists, from provenance,
the resources whose name-like attributes depend on the key: the next plan
renames them, usually a replace. With the new key only (`rekey dform
env=staging`) it moves the state a stack had before it was keyed, the
files directly in `dform.state/<stack>/`; a plan of a keyed stack whose
deployment has no state yet but whose unkeyed state exists says so.

The collision lint: in a keyed stack, a resource whose name-like attribute
(`name`, `metadata.name`, `bucket`, or a path a provider's schema flags
`name_like`) has a value no key input flows into gets the same name in
every deployment, and they collide in a shared account: a warning at the
field. The value flows through bindings, interpolation,
calls, lookups (`settings[env].bucket`), module inputs and refs; a read
that only gates the resource's block, or feeds another field, does not
count, and neither does it for `rekey`'s list. `isolated = true` in the
stack's `[stacks.NAME]` says each key value deploys into its own account
(or world), and turns the lint off; `dform.df`'s says so, since its iam
module's names are fixed.

`stack_output("app[env=prod]", k, V)` reads one deployment's outputs,
`dform stack handover 'app[env=prod]' --to ...` hands one over, and
`dform controller run 'app[env=prod]'` runs one.

Cross-stack values: `output k = t` at the top of a program is a stack
output. `apply` records the stack's outputs whose values are known in its
state, publishes them beside it as their own object, `outputs.json`, and
records where the deployment's objects are (an absolute directory, or
`s3://...`) in `dform.state/stacks.json`; every other program reads them
as facts, `stack_output("net", vpc_id, V)`, from `outputs.json`
only, never the state. An output of a configured attribute (`output c =
net.vpc.main.cidr`, which the program keeps as a ref) is published as the
program's value of it, else the world's; one whose value is not known yet
is published as pending, and its reader has a null (`?stack_output/net#c`)
that its resources wait on until a later apply publishes the value. An
output declared `secret(T)` is recorded and
published as its label (`output/#k`) and the keyed digest of its value
(`hmac-sha256:..`, the deployment's plan key), never its value (E DR-19):
a reader gets a secret null, and using it in a public place is the static
secret error (E0304). A secret output that is a resource's attribute
(`output pw = db.user.main.password`, or a sensitive computed value) is
also published with where it is held: the provider, the object's type
and remote id, and the path. Read into a sensitive field of another
stack, it travels to that stack's provider in the Apply document as its
label and that reference (the protocol's `Null.held`), and the provider
reads the value there inside the call; the bytes never pass through
dform. A changed digest updates the reader's field. A secret output
that is only bytes the program had (an input's value) is held nowhere,
and a reader's plan that puts it in a field is refused ("... is a secret
output of prod that no provider holds"). The mock reads a held secret
from the producing deployment's world (a directory's; the project's own
bucket deployments' too) and keeps what it read in the reader's world as
`materialized`. `dform-provider-k8s` reads one its own objects hold from
the cluster (a Secret's `stringData` key from its `data`), refuses one
another provider holds, and marks the object with the references it read
(the annotation `dform.io/held`, never the value), so the object reads
back as the reference and compares equal to the document that made it. A saved plan records the
digest of each outputs object it read, and `apply PLAN` refuses once one
has changed ("stack_output of NAME: its published outputs changed since
the plan").

#### Remote outputs

A project reads the outputs of another project's stacks through that
project's backend, named in its `dform.toml`:

```toml
[remotes]
platform = { backend = 's3("acme-dform", "prod/{stack}", {endpoint: "https://s3.gra.io.cloud.ovh.net", region: "gra"})' }
network = { backend = 'local("../network/dform.state")' }
```

`stack_output("platform.cluster[env=prod]", "endpoint", E)` names the
remote, then the stack and its key: when this project has no stack of that
name, it is read from `platform`'s backend, `{stack}` the stack's name
(without `{stack}`, the stacks are under the backend by name, as under a
state root), a keyed deployment under its key's segment, and the object is
that deployment's `outputs.json`. A reader needs read access to that object
only. A `local` directory is relative to the project root. A deployment the
remote has not applied has no outputs. The program names the deployment as
a string: which remote deployments are read comes from its
`stack_output` literals.

`dform.state/` (every path below, and the registry) is at the project
root, so the project's stacks share it wherever in the project dform runs
from. It is gitignored: it holds each deployment's plan key.

- Core state (Terraform-style address -> remote mapping, outputs): `dform.state/<stack>/state.json`
  (a keyed stack's deployment: `dform.state/<stack>/<k>=<v>/state.json`), and
  beside it the published outputs `outputs.json`.
- The fake backend's world (what "exists"): `dform.state/<stack>/remote.json`, beside the state.
- Discovery inventory, shared by every stack: `dform.state/inventory.json`.
- What providers and trust roots fetch (the k8s OpenAPI document, JWKS): `dform.state/cache/`.

The mock's flags go after `dev`, before the command. `dform dev --inventory
PATH` points at the discovery inventory file directly (see "Discovery facts"
below). Default: `<world dir>/inventory.json` when `--world` is given and that
file exists there, else `dform.state/inventory.json`. This is how
`examples/adopt` runs from a clean clone with no `dform.state/` setup:

```bash
cargo run -- -C examples/adopt dev --inventory ../../tests/fixtures/world/inventory.json plan --set env=prod
```

`dform dev --world PATH` (plan and apply) points the fake backend at a world file
instead: the JSON of what "exists", each resource's configured `attrs` and its
`computed` values. Plan refreshes from it, apply writes it back, and the stack's
state sits beside it as `<stem>.state.json`. Edit the world file and re-plan to
see drift:

```bash
cargo run -- -C examples/demo dev --world ../../tests/fixtures/world/dform.json plan   # steady state: no changes
```

Only what state maps is the stack's: an object in the world file that state
does not name is someone else's, never refreshed, updated or deleted, and a
resource of the same name is a create until an `adopt` names it (see "Adopt
existing resources"). A world file with no state beside it is nobody's.
`tests/fixtures/world/<stack>.json` and its `<stack>.state.json` are the fixture
format for tests.

## Providers are processes; the mock plays schema files

A provider is any executable that speaks the plugin protocol
(`proto/dform/v1/provider.proto`, gRPC; DESIGN.org "Providers and fact
plugins"). dform starts it, reads one handshake line from its stdout,
`dform-provider|1|tcp://127.0.0.1:PORT` (or `dform-provider|1|unix:///PATH`
for a unix socket), and calls Handshake, Configure,
Schema, Query, Read, Plan, Apply and Import on it; it exits when dform closes
its stdin. Schema returns the schema as facts, Plan and Apply are per resource,
and a sensitive computed value only ever crosses as its label. dform owns
ordering, parallelism, state and crash safety: a provider that dies during an
Apply is a failed action naming the resource, and the next `apply` resumes.
A provider built on `dform-grpc`'s `transport::serve`, the mock included, listens
on TCP unless `DFORM_PROVIDER_TRANSPORT=unix` is in its environment (which it
inherits from dform): then on a socket in the temporary directory, removed
when it exits.

The mock provider (`crates/dform-mock`) is dform itself: dform starts it as
`dform __provider fake` (a hidden command), a process of its own over gRPC
but always of the same build. `DFORM_PROVIDER_FAKE=PATH` names another
executable to run instead; `dform-provider-fake` is the same mock as an
executable of its own, for the conformance suite and use outside dform. A
handshake carries the provider's build (`0.1.0+COMMIT`, the git commit it
was built from), and dform refuses one of its built-in providers (the mock,
`fakecloud`, and `dform-provider-k8s`, `kubernetes`) built otherwise:
`rebuild: cargo build --workspace`.

The mock can pretend to be any provider: a provider it plays is a schema file of
plain facts, `providers/<name>/schema.df`, selected by the program's
`provider` statements (`provider gke`, or `provider aws { source =
"providers/aws-mock" }` for a directory or `.df` file relative to the
program's file), by the manifest's `[providers]` entry of the name, or,
overriding them, with `dform dev --provider NAME` (repeatable). There is
no default: a program with no `provider` statement starts none, and
`plan`, `apply`, `query`, `why` and `test` refuse it, "the program names no
provider: add `provider NAME` (dform.toml names its source) or run under
`dev --provider`". A `provider` or `instance` with no entries is written
without braces (`provider fake`, `instance network blue`); `fmt` drops a
`{}`. In any block an entry that is only a path takes the value of its
last segment's name, `region` for `region = region` and
`spec.selector.color` for `spec.selector.color = color`, as `{ a }` is
`{ a: a }`; `fmt` prints that form. `--provider path/to/schema.df` loads a file directly. A
`providers/<name>/schema.df` in the working directory wins over the schemas
built into the binary (`crates/dform-mock/schemas/`: `fake`, `gke`, `k8s`,
`aws-mock`). A `source` (or `--provider` path) that
is an executable, or a directory holding one named `dform-provider*`, is a
plugin instead, started on its own; each type goes to the provider whose
schema declares it. The world file, the inventory and `--chaos` reach the mock
at Configure.

Every resource's type is declared by the schema of the provider that applies
it. One that none of the stack's providers declares is a plan (and apply)
error before anything is planned, naming the resource, the `provider` blocks
and the known schemas (the built-in ones and `providers/*/schema.df`) that do
declare it: `provider fake does not declare google_compute_subnetwork;
declared by: gke`. A type the program declares itself with a `type` block is
the mock's to play.

A `provider` block's settings other than `source` configure the provider,
and read like any rule reads: inputs, settings rows, value names, tables and
`env.var`. A keyed deployment configures its providers by its key:

```dform
provider env

provider google {
  project = cfg.project_id                                # the env's settings row
  credentials = env.var("GOOGLE_CREDENTIALS_${env}")      # a secret, per key
  expect_account = cfg.project_id
}
```

The block lowers to `provider_config("google", { project: .., credentials:
.. })`, which reaches the provider at a second Configure as `settings` once
the evaluation knows it (the provider serves nothing until then; see "The
Kubernetes provider"). `env.var("NAME")` is the built-in `env` provider's
extern (`provider env`), answering the
process environment's variable as a `secret(string)`: never persisted, and
recorded in the plan file only by its label and its value's digest keyed
with the stack's plan key (`inputs.env`: `{"sensitive": "env.var/NAME",
"digest": ..}`, as a secret `--set`), so `apply PLAN` with the variable
changed or unset is a stale plan naming it; an unset one is an error
naming it. A provider's
configuration may not read what that provider serves itself (its externs,
its resources' attributes): that is a compile error naming the chain of
rules. Reading another provider's values is the lazy configuration: the
settings wait on its nulls. `expect_account = t` is checked, not sent:
Configure answers with the account the provider's credentials reach, when
it can tell (the mock reports its `account` setting), and a run whose
provider reports another account, or none, refuses to plan, naming both
(an expected account a secret reaches, an `env.var`'s, by its label,
`provider/NAME#expect_account`, never its value)
(`deployment pngu[env=prod]: refusing to plan: provider google reports
account renfry-dev, but the program expects renfry-prod`).

The workspace keeps the engine apart from the transport. `dform-core`
(`crates/dform-core`: parser, engine, planner, executor, printer) speaks to
providers through a synchronous, completion-based trait
(`plugin::backend`: `submit` a call, take the `next_completed` answer),
whose calls and answers are the protocol's own messages (`dform-wire`,
prost only); it builds with no network stack. Three backends implement it:
the process backend (`dform-grpc`: a spawned executable over gRPC; the
`dform` binary's, the only production path), and the direct and wire
backends (the mock, `dform-mock`, linked in, its calls queued; the wire
backend encodes and decodes every message through prost). `dform-direct`
is the command line over the direct backend (`DFORM_BACKEND=wire` for the
wire one, `DFORM_SEED=N` to answer the calls in flight in an order the seed
picks), for tests and benches; a plugin executable is out of its reach.
Crates select the backend, not cargo features.

`dform provider check PATH` is the conformance suite: it runs every method
against the provider at PATH with a synthetic schema and prints one line per
check, failing if any deviates. A provider that serves its own schema (a real
API's) instead of the one it is given is checked with the `examples` its
Schema returns: documents of its own types to plan, create, update, replace
and delete. The mock passes it:

```bash
cargo run -- provider check crates/dform-mock/schemas/fake.df   # the mock
cargo run -- provider check ./my-provider         # any plugin executable
```

```dform
type_provider(net.vpc, "fakecloud")                         # who owns the type
type_attr(net.vpc, "id", "string", ["computed", "id"])      # Flags: required computed id
type_attr(db.postgres, "endpoint", "string", ["computed"])  #   sensitive nullable optional_computed
type_attr(net.vpc, "cidr", "string", ["force_new"])         #   force_new
type_list_key(k8s.deployment, "spec.template.spec.containers", ["name"])  # list merge keys
type_mint(db.postgres, "endpoint", "{name}.db.fake")        # optional: how the mock mints it
type_retry(db.postgres, 5)                                  # optional: Read attempts (default 3)
type_replace(k8s.deployment, "create_first")                # optional: create_first, destroy_first, either (default)
type_doc(net.vpc, "cidr", "The network's IPv4 range.")      # optional: a path's description ("" the type's)
```

Built-in mock schemas: `fake` (the demo's), `gke` (pngu.df), `k8s` (fifteen
Kubernetes kinds; try `cargo run -- -C examples/k8s plan`)
and `aws-mock` (twelve AWS types in the Terraform provider's shape, with its
Optional+Computed attributes and keyless sets; try
`cargo run -- -C examples/aws plan`). Each example names its
provider with a `provider` statement.
A `required` attribute the program does not set is a plan error. Lists with
`type_list_key` are diffed by key (`spec.template.spec.containers[name=web].image`),
lists of type `set` as sets. A `type_mint` string may use `{type}`, `{name}`,
`{attr}`, `{hash}`, `{n}` and `{doc:PATH}` (the program's value at PATH, e.g.
Kubernetes' `generateName`); a template that is only `{doc:PATH}` takes the
value there whatever its type (the gke mock's cluster `zones` are its
`node_locations`).

`computed` + `id` is a fresh value (an identity), `computed` + `sensitive` is a
secret, `computed` alone is open (proposal E §2.2). `optional_computed` is
Terraform's Optional+Computed: the program may set it, else Apply picks it.
A value Apply picked is read back but never sent again: the document an
update sends holds only the Optional+Computed paths the program contributes
to, so the provider keeps what it chose.
Setting a plain `computed` attribute is a compile error naming the resource
and the path.

`force_new` is the provider's "requires replace": an update that changes that
path (or one under it) is planned as a replace, `-/+` (see "Deletes and
replacement" below). The fake schema's vpc and subnet `cidr` are `force_new`.

The facts are injected into the program, so rules can read them and
`dform query type_attr` lists the schema (`dform provider schema NAME`, a
provider's alone). A run injects the rows of
the types its program, its inputs and state name (and the types they
alias), plus every `type_provider`: a derived Kubernetes schema is 16k
facts. The providers are asked for only those types' rows too (the
Schema request's `types`). A `query` or `why` of a schema predicate, a
rule reading one for a type it does not spell out (`type_attr(T, ...)`),
or a rule wanting a resource whose type is built at runtime
(`want(t, a) where t = "k8s.${k}"`), sees all of it. `type_doc` rows, the
descriptions the language server shows, are never injected.

## The Kubernetes provider

`dform-provider-k8s` (built with dform, `crates/dform-k8s`) is a real provider: it
speaks the same protocol against the API server of the cluster the
environment names (`KUBECONFIG`, else `~/.kube/config`, else the pod's service
account). Select it by path; the program the mock plans applies to a cluster
unchanged:

```dform
provider k8s { source = "./providers/k8s" }   # a directory holding dform-provider-k8s
provider k8s { source = "bin/dform-provider-k8s" }        # an executable
```

- A program may name the cluster itself, as a managed cluster's kubeconfig
  arrives: `provider_config("kubernetes", { kubeconfig: K })` with `K` the
  text of a kubeconfig (its current context), or `{ host: H, ca: C, token:
  T }` (`client_certificate` and `client_key` instead of `token`; `ca` and
  those as PEM or base64 of it; optionally `namespace`). The value may be a
  secret (an input declared `secret(string)`): the provider receives it at
  a second Configure once the program's evaluation knows it, keeps it in
  memory, and never writes or prints it. Until then the provider serves
  nothing (the environment's kubeconfig is not read) and its resources wait
  on the null; the schema is the snapshot's. A secret computed value of
  another provider's resource (a cluster's `kubeconfig` as a sensitive
  attribute) cannot reach it yet: secrets travel as labels, and no
  protocol call hands one provider's secret to another.
- Types are `k8s.<group>.<version>.<kind>`, the core group as `core` and the
  kind in snake_case (`k8s.apps.v1.deployment`, `k8s.core.v1.config_map`,
  `k8s.networking.k8s.io.v1.ingress`): a type name is a lowercase qualified
  name. The mock's short names (`k8s.deployment`) are aliases of them
  (`type_alias` in `crates/dform-mock/schemas/k8s.df`), so `examples/k8s/stacks/k8s_demo.df`
  plans the same against either.
- The schema is derived at Configure from the cluster's `/openapi/v3`, and
  cached as `k8s-openapi.json` in the stack's state directory (fetched again
  when the server's index changes); the schema derived from it is cached
  beside it as `k8s-schema.json`, keyed by the document's hash, and so is
  the snapshot's when offline. `x-kubernetes-list-map-keys` are
  `type_list_key`, list-type `set` a `set`; the leaves of `status.*` and the
  server-written metadata are computed (`metadata.uid` the identity);
  `metadata.name` (a program may set `metadata.generateName` instead) and
  `metadata.namespace` (default: the kubeconfig's) are `optional_computed`
  and `force_new`; a property its object lists as `required` is required
  where the object is set; a Secret's `data` and `stringData` are sensitive;
  `type_retry` is 5; a Deployment, Service or ConfigMap is replaced
  `create_first`, a Namespace `destroy_first`. An OpenAPI `description`
  is its path's `type_doc` (the kind's is the type's): the snapshot keeps
  them (868K; 213K without).
- A field the API server defaults is `optional_computed`: one whose schema
  has a `default` other than the zero value (the snapshot keeps them:
  ports' `protocol`, a few volume sources), and, as a fallback for what the
  document does not mark, a Service's `spec.clusterIP`, `spec.clusterIPs`,
  `spec.type`, `spec.sessionAffinity`, `spec.ipFamilies`,
  `spec.ipFamilyPolicy`, `spec.internalTrafficPolicy`; a workload's
  `spec.progressDeadlineSeconds`, `spec.revisionHistoryLimit`,
  `spec.strategy.type`, `spec.updateStrategy.type`,
  `spec.podManagementPolicy` and the StatefulSet PVC retention policy; a
  Job's or CronJob's `spec.backoffLimit`, `spec.completions`,
  `spec.parallelism`, `spec.completionMode`, `spec.podReplacementPolicy`,
  `spec.suspend`, `spec.concurrencyPolicy` and history limits; and a pod
  spec's (a Pod's, a template's, a CronJob's job template's) `dnsPolicy`,
  `restartPolicy`, `schedulerName`, `terminationGracePeriodSeconds` and
  containers' `imagePullPolicy`, `terminationMessagePath`,
  `terminationMessagePolicy`.
  Read returns their live values as computed, so a ref to one
  (`ref(k8s.service, "web", "spec.clusterIP")`) is a null until the object
  exists, then the cluster's value, as against the mock. An update never
  sends such a value unless the program sets it, so the server's value
  stays unowned rather than becoming `dform`'s (and neither `status` nor
  `metadata.managedFields` is ever sent). Objects are never
  flagged (a default object would carry the server's `rollingUpdate` beside
  a program's `strategy.type = "Recreate"`); a path inside a list element
  is flagged but never resolved (a ref cannot name one).
- The value model has no float. A float crosses the protocol as `Float`
  and the engine reads it as a string, the shortest decimal that parses
  back to the same number (`0.5`, `1e+300`). In the snapshot's kinds a
  number is only ever a quantity, which the server takes as a string too;
  a float field of a custom resource that a program sets reaches the server
  as a string.
- A Secret's `stringData` reads back from `data` (base64-decoded, the keys
  dform applied).
- Remote ids are `NAMESPACE/NAME` (`NAME` for a cluster-scoped kind); Read
  and Import GET the object. Writes are server-side apply as the field
  manager `dform`, never forced: a field another manager owns fails the
  action naming the manager and the field. What Read returns as
  configuration is only the fields `dform` owns (`metadata.managedFields`),
  so a server default or another manager's field is never a diff. Plan is a
  dry-run apply (`dryRun=All`) diffed against the world, the object named
  by its remote id when the document leaves the name to the server; a change to a field
  the server will not change in place plans a replacement. A generated name
  is picked by the provider (`generateName` plus five characters), since
  server-side apply needs a name. Delete propagates in the background and
  waits for the object to go. Watches are not used.
- Every object dform applies carries the label `dform.io/stack` (the
  deployment's name as a label value), and a Create's idempotency key rides
  on the annotation `dform.io/idempotency-key`; neither is configuration.
  The provider has the `managed` capability: a Create whose answer was lost
  (a generated name nobody knows yet) is found by listing the type with that
  label as the selector and matching the annotation, so the next run maps
  it, and deletes it if the program has dropped it.
- The provider has the `inventory` capability: `world.k8s.service["shop/web"].spec.selector.color`
  reads the live object, whoever manages it. The objects of the kinds the
  program reads are listed in every namespace, each named by its remote id;
  `cloud_attr` holds its leaves but `status` (`spec.ports[0].port`), and
  `cloud_computed` its `status`. Offline the inventory is empty, and so it
  is for a provider configured by `provider_config` (discovery runs before
  the program is evaluated, and is not run again).
- With no cluster in reach, or `DFORM_K8S_OFFLINE` set, the provider is
  offline: the schema is the checked-in snapshot of Kubernetes v1.36.0's
  document (`crates/dform-k8s/openapi-snapshot.json`, trimmed to the mock's
  kinds, Pod, ReplicaSet and RBAC by `crates/dform-k8s/trim_openapi.py`), Plan
  diffs locally, and Read, Apply and Import fail naming why.

```bash
cargo build
cargo run -- provider check target/debug/dform-provider-k8s   # creates and deletes a ConfigMap
DFORM_K8S_OFFLINE=1 cargo run -- plan my.df                    # no cluster: the snapshot's schema
```

`tests/k8s_offline.rs` runs without a cluster: offline, and against a fake API
server (conformance, a whole apply, a field-manager conflict).
`tests/k8s_cluster.rs` runs against a real one in a namespace
`dform-test-<random>` it deletes afterwards, only when
`DFORM_K8S_TEST_KUBECONFIG` names its kubeconfig (a kind cluster:
`kind create cluster --name dform-test --kubeconfig target/kind.kubeconfig`;
with rootless podman, `KIND_EXPERIMENTAL_PROVIDER=podman`): the demo
converging, an update, a delete, a field-manager conflict, drift from a
`kubectl patch`, a Create whose answer was lost, a Secret's `stringData`,
a kubeconfig held as a secret, and a world read of a live object.

## Computed values come from Apply

Plan never invents a computed value. The evaluator mints one labeled null
`?T["N"].attr` per wanted resource and computed attribute (proposal E §2.5), at
normal rank for `computed` and at `@default` for `optional_computed`, so a
program's own value wins. A `ref(T, N, Attr)` to such an attribute reads that
cell: the null, or the program's value. Once `N` exists, the world's value
replaces the null before anything is derived (round 0, through the state's
identity mapping), so a steady-state stack shows no nulls. Apply mints ids,
endpoints and secrets per the schema and fills the nulls in dependency order.

```bash
cargo run -- -C examples/demo plan
# + net.subnet["network.main::private-us-test-1a"]
#   vpc_id = ?net.vpc["network.main::vpc"].id
```

Every address dform prints is the source term that names it, `T["A"]` (`A`
the full address, a module instance's scope `m.i::` included), and an
attribute of it is `.path` after it: plan lines, the apply order, nulls,
secret labels, `state show`, `dev graph` and diagnostics. Every address the
command line takes is read the same way (`why`, `query`, `state show`,
`state mv`, `dev show`, `--chaos`), so an address copied from a plan pastes
into a program, a query or a command; quote it for the shell
(`why 'net.vpc["network.main::vpc"].cidr'`).

The plan is the Z-set `desired - world` (proposal E §2.8): per address a
create, a delete, an update, or nothing. The first line counts it in those
terms, `plan: 3 deformations (2 create, 1 update), 4 pending, 1 undetermined`,
and the sections follow in this order; what cannot be decided yet is said so:

- `moved T["Old"] -> T["New"]` lines first, one per `moved/3` rename of state.
- `definite:` deformations that run in this tick, grouped by resource: `+`
  create, `~` update, `-` delete, `>` adopt, `-/+` and `+/-` replace,
  `- T["A"]  (deposed)` for an object a replacement deposed. An update diffs a keyless set, or
  a list with merge keys (`containers[name=web]`), by element: an element that
  is new or gone is one `+`/`-` line with its leaves, not every later index
  shifting. Map leaves print one per line.
- `pending on ?nulls (resolves after tick N):` deformations held until those
  nulls resolve: downstream of an Apply that resolves a null something is
  stuck on, or an update whose new value is an open null against the world's
  value. Their diffs are shown now. The hint appears when the nulls' owners
  are scheduled by this plan.
- `pending groups:` resource rules stuck on a null (`gke_nodepool.? x
  unknown, on ?gke_cluster/pngu#zones`): how many there will be is not known.
  A resource rule that reads a predicate with a stuck instance is one too
  (`(reads node_pool_up("np-a"), which is stuck)`): it may derive after the
  boundary, so `apply` runs another tick for it.
- `undetermined:` policies that cannot be decided yet, `decided after tick
  N`; and denies that read a predicate with a stuck instance, which `may
  derive after tick N`. They are never reported as satisfied.
- `shadowed:` contributions at a losing rank that disagree (a warning), and
  `conflicts:` cells whose contributions disagree at the winning rank, each
  naming the resource, the path and every witness. A conflicted address is not
  a deformation; the plan still prints, then refuses with the deny.
- `denied:` denies over the plan itself (`lifecycle prevent_destroy: the plan
  would replace T["A"]`); the plan still prints, then refuses.
- `apply order:`, one address per line under each `tick N`: which tick each deformation runs
  in, from the dependency DAG and the nulls it waits on (a `+/-` replacement's
  deposed object is deleted the tick after).
- `(drift: ...)` marks an update where a fresh null meets a value the world
  already has: the identity mapping is stale.
- `stack NAME is undeformed`: nothing to do, nothing stuck (the only line).

`plan --why` prints under each deformation why it is planned: `why`'s
tree compressed to one line per leaf. `by FILE:LINE` is the statement
that derived it, as `why` prints it; each `because` line is a leaf of that
derivation, the facts in between dropped: a fact the program states at its
`file:line`, a table's row at its `path:line` (`because data/zones.csv:4
zone("us-test-1c", 3)`), a `--set` or `--data` as its flag, an extern's
answer, a world fact, a fact found absent. A create is explained by its
`want`, an update (a drift, a replace) by the winning contributions to each
attribute it changes, a delete by state alone (`because no statement derives
it now; state has it`). Of several derivations the one with the fewest
leaves is shown. Secrets print as their label, as everywhere:

```
$ dform plan net --why
+ net.subnet["private-us-test-1c"]
  cidr = "10.0.3.0/24"
  ...
  by stacks/net.df:9  resource net.subnet "private-${z}" { .. } where zone(z, n)
  because stacks/net.df:7  net.vpc["main"].cidr = "10.0.0.0/16"
  because data/zones.csv:4  zone("us-test-1c", 3)
```

Colour: `--color auto|always|never` (global; `auto`, the default, colours
when stdout is a terminal and `NO_COLOR` is unset; errors on stderr
likewise) paints the plan by its semantics: `+` green, `~` yellow, `-` red,
`-/+` and `+/-` magenta, a `?` null cyan, `(sensitive)` dim, addresses and
section headers bold, conflicts and denies red with the witnesses' names
bold, a pending group's line in the warning colour (bold yellow); apply's
question and `apply: complete` likewise. `--json` and the plan file are
never coloured.

`plan --json` prints the same report as one JSON document, the thing CI and
editors consume: `stack`, `undeformed`, a `summary` of counts, then the
sections as arrays in the order above (`definite`, `pending`,
`pending_groups`, `undetermined`, `shadowed`, `conflicts`, `apply_order`,
`unscheduled`, `moved`, `denied`). A deformation is `{action, address,
type, name, changes}` (`address` as the text prints it), a replace with `create_first`, a deposed delete with `deposed:
true`, and with `--why` a `why` array of `{kind, at, text}` (`kind` is
`rule` for the `by` line, else `fact`, `input`, `extern`, `world`, `plan`,
`absent` or `state`; `at` the `file:line` when there is one). A change is `{op, path, before, after}` (`op` is `set`, or
`add`/`remove` for a set element, with its `leaves`); a null is `{"null":
LABEL, "class": CLASS}` and a secret `{"sensitive": LABEL}`.

`dform query stuck` lists the stuck rule instances. A rule can read
them too, `stuck(RuleId, HeadPattern, Bindings, Nulls)`, and
`may_derive(RuleId, HeadPattern, Nulls)` (a resource rule that may derive
after a boundary): a policy such as `deny "one tick" { rule: r, on: n }
where stuck(r, _, _, n)` refuses any plan with a stuck instance. `stuck/4` is derived above every rule that can stick,
so a reader must not itself be able to stick (read it into fresh variables
only) and nothing it derives may feed such a rule; otherwise the program is
rejected with the negative cycle.

`apply` runs in ticks. A tick applies every definite deformation in dependency
order and holds what is pending. At the boundary the results come back as
world facts, round 0 resolves the nulls they answer, the program is
re-evaluated and policy is checked again; a deny there stops the run with the
reason printed. `--max-ticks N` (default 8) is a safety valve for a loop
that never settles, not a way to stop early:

```bash
cargo run -- -C examples/gke apply                  # asks again at tick 2
cargo run -- -C examples/gke apply --set zones=1   # one zone: stops after tick 1
```

At a boundary apply also compares the refreshed world with what it last saw
(the tick's refresh and its Apply responses). A change under an address whose
deformation is pending for this boundary stops the run before the next tick,
with the change printed (`the world changed under a pending deformation after
tick N:`) and the deny that stops it (see "Policy over the plan" below); a
change anywhere else is reported as `drift after tick N:` and the
run goes on, the next tick deforming it back:

```bash
cargo run -- -C examples/gke dev \
  --chaos 'mutate=gke_cluster["pngu"].deletion_protection=false' apply gke_two_phase   # drift, tick 2 undoes it
```

Deletes and replacement. Deletes run after every create and update, in
reverse dependency order (a delete has no desired document left, so state
records each object's dependencies when it is applied). Which way a
replacement goes is the schema's `type_replace(T, Order)`: `destroy_first`
(`-/+`), `create_first` (`+/-`), or `either` (the default), where it is
`-/+` unless `lifecycle(r, "create_before_destroy")` says `+/-`. That fact
on a `destroy_first` type is an error naming the type; on a `create_first`
type it is redundant. In the mocks a Kubernetes Deployment or Service and an
`aws_instance` are `create_first`, a Namespace and an `aws_s3_bucket`
`destroy_first`, the fake `net.vpc` `either`. A `-/+` replace deletes the old
object, then creates the new one under the same name. A `+/-` one: the new object is
created first under a free name (`main-2`), the old one is *deposed* (kept in
state's `deposed` section), and a boundary follows; the next tick moves what
depends on it to the replacement and then deletes the deposed object
(`- T["A"]  (deposed)`). A deposed object left by a failed apply is deleted by
the next one, once nothing that depends on it is still pending.

Either way the replacement is a new object, so every null that named the old
one (its id, its other computed values) is unresolved again: an existing
object that reads one is `pending on ?T["A"].id (resolves after tick N)` and is
updated to the new value the tick after the replacement; a new object that
reads one is created after it in the same tick. `dev --chaos fresh-ids` makes
the mock mint a new id on every create, so the difference shows:

```bash
cargo run -- -C examples/demo dev --chaos fresh-ids apply dform env=staging
cargo run -- -C examples/demo dev --chaos fresh-ids apply dform env=prod   # tick 1 replaces vpcs and subnets; tick 2 updates their readers
```

Lifecycle is plain facts the planner reads (and policy can read too). Each
takes the resource as a reference: a resource in scope by its name, one of
a module instance or one no longer in the program by its address, and many
at once by a rule that binds them with `in`:

```dform
lifecycle(main, "prevent_destroy")                   # a delete or replace of it is a deny
lifecycle(main, "create_before_destroy")             # replace creates first (type_replace either)
moved(net.vpc, "network.main::vpc", net.vpc["network.core::vpc"])  # rename without destroy
ignore_changes(main, "tags.owner")                   # set on create, then ignored
lifecycle(pg, "prevent_destroy") where env == "prod", pg in db.postgres   # every prod database
```

`moved(T, Old, new)` rewrites state's identity from `Old` to `new` before the
diff, so renaming a component instance with a `moved` fact per resource plans
undeformed (`moved T["Old"] -> T["new"]` is printed; `apply` persists it). The
old side is text, since it names a resource that no longer exists; the new
side is a reference. It applies only while state maps `Old` and not `new`, so
the facts can stay (once `new` is renamed in turn, write it as its address,
`T["new"]`).
`ignore_changes` leaves the path in a create; once the object exists it
drops the path from the desired document and from the world's, and an
update keeps the world's value there (or its absence). `prevent_destroy`
blocks `plan` and `apply` with `lifecycle prevent_destroy: the plan would
delete T["A"]`.

Policy over the plan. Once the plan is computed its deformations go back to
the evaluator as facts and the program is evaluated once more (the policy
pass): `deformation(Kind, r, Before)` per deformation, `r` the resource as a
reference that prints as its address, `T["A"]` (`Kind` is
`create`, `adopt`, `update`, `drift`, `pending`, `replace`, `delete`,
`delete_deposed` or `remaining`; `Before` a digest of the world document it was planned
against, `absent` for none) and `world_digest(r, Now)`. The lifecycle
denies are rules over them (`zset::POLICY_RULES`): `prevent_destroy` reads
`lifecycle/2` and a `delete` or `replace`, and at a phase boundary the held
deformations come back as `pending` with the digest they were planned
against, so the world moving under one is a deny too; so do the remaining
deformations of an interrupted apply, as `remaining`, when it resumes.
`why` explains them (the injected facts print as `plan`, or `plan (tick
N)` when given at an apply tick), and a policy can read the same facts,
binding the resource with `in` to read its attributes and comparing it with
a resource by `==`:

```dform
deny "no deletes here: ${r}" where deformation("delete", r, _)
warn "replacing a database" { pg } where deformation("replace", pg, _), pg in db.postgres
deny "the core network stays" where deformation(_, r, _), r == main, env == "prod"
```

```bash
cargo run -- -C examples/demo why 'deny(m)'    # the deny, the lifecycle fact and the deformation it read
```

Only policy may read the deformation: a resource rule over it would make the
plan depend on itself, and is an error.

`apply --parallel N` (default 1) walks a tick's dependency DAG with at most N
Apply calls in flight: a create or update waits for what its document
references, deletes wait for everything else. Output order does not change
with N. The calls in flight run at once, and each is put on the executor's
clock from the time its provider says it took; on the mock (chaos `latency`)
the difference shows there:

```bash
cargo run -- -C examples/demo dev --chaos 'latency=net.vpc["network.main::vpc"]:100' \
  --chaos 'latency=net.vpc["network.peer::vpc"]:100' apply dform env=staging --parallel 4   # the two vpcs overlap: 100ms, not 200ms
```

An apply that fails or is killed can be resumed: before a tick's first Apply
call its deformations are written to state as in flight, each with the world
document it was planned against, and each answered call takes its action out.
The next `apply` prints `resuming the apply interrupted at tick N; remaining:
...`, refreshes, and finishes the remaining actions, listed after the plan
under `resumed from the apply interrupted at tick N:`, each create its
idempotency key found nothing for marked `(retried with its idempotency
key: nothing it made was found)`, before it asks; if the world changed
under one of them it prints the change and stops before any Apply call, on
the deny the policy pass derives from `deformation(remaining, ...)` (run
`apply` again to plan against the world as it now is).

`plan --out PLAN.json` writes the plan file: the inputs (each program file with a
digest of its content, each `--input-file` with its digest keyed like a
sensitive value's, since an input file may hold a secret, `--set`,
`--data`, `--provider`, `--world`, `--inventory`), a digest of the
refreshed world, and the deformation delta: each deformation's action,
before and after values (redacted as the plan prints them; a sensitive one
as `{"sensitive": label, "digest": HMAC}`), the nulls it
waits on and the tick it runs in; the pending groups, each with its head
pattern, its rule and the null-free bindings of its stuck instance
(redacted); the nulls round 0
resolved and the ones the delta still carries; the tick schedule; the
extern answers the plan read; the commit each `git` input relation's ref
named; and the plan's digest, with what needs an approval (see
"Approvals"). `apply PLAN.json` takes its inputs from the
file (flags given on the command line must match them), refreshes and re-evaluates at
every tick, and refuses unless the delta it computes is the file's:
Terraform's stale-plan rule, stated for Z-sets. Every deformation must be in
the file with the same action, the same before-state and the same desired
values (a null the file carries matches what it has resolved to); every
deformation the file has not run yet must still be one; a new address is
allowed only where a pending group the file records derives it (its
`want` unifies with the group's head, and a firing of the group's rule
holds every binding the group recorded), and even then the apply stops
before that tick, as every unattended apply does; and a deposed
object's delete the tick after its `+/-` replacement. It prints the difference
and stops before applying anything of that tick. So with a plan file, drift
anywhere stops the run at the boundary, where a plain `apply` reports it and
goes on. A sensitive value is compared by its digest: HMAC-SHA256 over its
bytes, keyed by 32 random bytes the stack keeps beside its state
(`state.key`, made on the first `plan --out`), so a secret that changed
between plan and apply is refused and the file never carries the bytes:

```bash
G=examples/gke/stacks/gke_two_phase.df
cargo run -- dev --world w.json plan $G --out plan.json
cargo run -- apply plan.json                          # tick 1; tick 2's nodepools it could not name
# the world moves after tick 1: the file refuses
cargo run -- dev --chaos 'mutate=gke_cluster["pngu"].name="other"' apply plan.json
```

A `sensitive` computed value never leaves the provider: what dform sees, stores
in consumers and prints is its label, `(sensitive T["N"].attr)`. A value at a
`sensitive` path the program sets prints as `(sensitive)` in a diff, and as its
label wherever else it appears. Everything dform prints goes through one
redactor: `plan` (text, `--json`, the plan file), `show`, `query`, `why`,
`graph`, and the policy messages on stderr, whose context quotes values and
rule text.

Secrets are also checked statically (E DR-19): one dataflow pass over
the predicate signatures labels every position a secret reaches, from a
`sensitive` schema attribute, an input declared `secret(T)` (`input pw:
secret(string)`) or an extern column declared `-value: secret(T)`, through
rules, `format`, arithmetic, lists and objects. A rule that would leak one
is a compile error with a span, before anything is evaluated:

| code  | the secret reaches |
|-------|--------------------|
| E0301 | a comparison, a builtin predicate or an inspecting function (`len`, `split`, `inet_*`) |
| E0302 | a negated literal |
| E0303 | an aggregate other than `collect_*` (`count` leaks cardinality) |
| E0304 | a public place: a resource attribute the schema does not mark `sensitive`, a setting, an output or input not declared `secret(T)`, a `deny`/`warn` |
| E0305 | a resource address (`want`, a resource name, `ref`, `scoped`) |

An input's own refinement (`input pw: secret(string) check len(pw) >=
12`) is where a secret may be checked; its deny does not print the value.
`declassify(V, Reason)` is the one way a secret leaves on purpose: its
value is `V`, public to the pass (what is inside it may be inspected), and
the rule also derives `declassified(Site, Reason)`, `Site` where the rule
is written, for a policy to read or deny:

```dform
output pw_len = declassify(len(pw), "its length is public")
deny "declassified at ${at}: ${r}" where declassified(at, r)
```

The value of an input or output declared `secret(T)` prints as its label,
`(sensitive input/#pw)`, in `query`, `why` (the `--set` leaf included) and
`show`. The plan file records a secret input's `--set` as its label and
digest, so `apply PLAN` asks for `--set pw=...` again and refuses another
value.

## Approvals

Policy decides what needs an approval. `requires_approval(r, Reason)` is an
ordinary relation a program derives over `deformation/3` in the policy
pass; `r` is the resource, a reference, which the plan, the plan file and
the approval check print as its address (`T["A"]`). No rows, no token
needed:

```toml
[stacks.app]
approvals = 'jwks_file("approvers.jwks.json")'
```

```dform
key env: enum("staging", "prod") = "staging"

requires_approval(r, "a replace in prod") where env == "prod", deformation("replace", r, _)

# Optional: who may approve what. Without it, any key of the trust root may.
approver_allowed(who, r) where requires_approval(r, _), who in ["alice", "bob"]
```

A plan with rows prints a `needs approval:` section, each deformation and
its reason, and the plan's digest, `plan digest: sha256:...` (a plan that
writes a file says its digest on stderr, `plan file: PLAN (plan digest:
sha256:...)`); `plan --json` has them as
`needs_approval` and `digest`. The digest is sha256 over the canonical JSON
(sorted keys, no whitespace) of the plan file without its `digest` field:
the delta, the inputs, the commit each `git` input relation's ref named
(`git_commits`; `apply PLAN` refuses when one moved) and the extern
answers, with every secret already the stack's HMAC of it. The plan file
records it (`digest`) and the rows (`needs_approval`).

A token is a signed statement: the approver, the plan digest, the stack and
its key, and an expiry. Two shapes are accepted, nothing vendor-specific:

- a JWT (RS256, ES256 or EdDSA) with the claims `digest`, `stack`, `key`
  (`{"env": "prod"}`; `{}` for an unkeyed stack), `sub` (the approver) and
  `exp`;
- a DSSE envelope, `payloadType` `application/vnd.dform.approval+json`,
  whose payload is `{stack, key, digest, approver, expires}` (`expires` RFC
  3339, UTC), signed with Ed25519; the envelope may also be given in base64.

The trust root is a stack setting: `approvals = 'jwks("https://...")'`, a
JWKS document fetched at apply time (with `curl`) only when the copy cached
beside the state is older than an hour (a failed fetch falls back to a stale
copy, with a warning), or `jwks_file("path")` (relative to the project root) for
offline use; a list of them is fine. A second argument, `jwks(URL, ISSUER)`,
is the `iss` a JWT from it must name. A key is found by the token's `kid`
(the envelope's `keyid`); a DSSE signature needs an Ed25519 (`OKP`) key.

`apply PLAN --approval FILE` verifies the token offline before any Apply
call: the signature against the trust root, the digest against the plan
file's (recomputed from its content: a file edited after the plan is
refused), the stack and its key against the deployment, the expiry, and,
when the program states `approver_allowed`, that it holds for the approver
and every deformation that needs the approval. A missing or invalid token is
a refusal naming what failed. The stale-plan check then guarantees that what
is applied is what was approved. A plain `apply` of a plan that needs an
approval is refused (plan with `--out`, have the digest approved, apply the
file). A plan file's pending group bounds what a later tick may create
for it: the group's rule, with the bindings it was stuck with (see the
plan file above). Provider credentials stay the environment's: a provider inherits
dform's environment, and dform mints and exchanges no tokens.

In controller mode a deformation that needs an approval is held (`tick N:
proceed: held, needs approval (Reason): T["A"]`) and the plan's digest is
published: a log line, `tick N: approval needed: plan digest sha256:...`,
and `approval-pending.json` beside the state. A token for that digest
releases it when it arrives through the input relation `approval/1` (the
token's text; `input approval(token) from facts("approvals.facts")`) or
as a file in the drop directory `approvals/` beside the state (`event
approval`); `tick N: approved by WHO: plan digest ...`. A token for another
plan is ignored, one that fails otherwise is logged (`approval refused:
...`). `examples/bootstrap/stacks/workload.df` holds a prod rollout this way.

The approval service is not dform's. `dform-approve` (built with dform,
`crates/dform-direct`) is the example signer, a local Ed25519 key:

```bash
cd examples/approvals
A="cargo run -q -p dform-direct --bin dform-approve --"
$A keygen approver.key > approvers.jwks.json      # the trust root: jwks_file("approvers.jwks.json")
cargo run -- apply approvals env=prod
cargo run -- plan approvals env=prod --set cidr=10.1.0.0/16 --out plan.json
$A sign approver.key --digest sha256:... --stack approvals --key env=prod \
  --approver alice --ttl 3600 > approval.json      # --format jwt; --format fact for approval/1
cargo run -- apply plan.json --approval approval.json
```

`examples/approvals/stacks/approvals.df` is that program.

## The audit log

Every deployment has an append-only audit log beside its state,
`state.audit.jsonl` (`<stem>.state.audit.jsonl` beside a `--world` file),
which moves with the state on a rekey or a handover. In a bucket, where an
object is written whole, the log is kept in segments of 100 entries,
`state.audit/000001.jsonl`, ...: an entry rewrites only the last segment
(conditionally on what it read, so concurrent entries never fork the
chain), and the log is the segments in order (after a `state.audit.jsonl`
written before segments, which the first segment continues). Each entry is a line
of canonical JSON: `seq`, `time` (UTC), `kind`, `prev` (the previous entry's
`hash`), the kind's fields, and `hash`, sha256 over the entry without it.
The kinds:

- `plan`: the digest, the plan file (if any), the inputs, the pinned git
  commits, the rows that need an approval, and who (`plan --out`, and every
  apply of the plan it applies);
- `approval`: the verified statement and the token, or `not required`, or
  why it was refused;
- `apply_start`: who, dform's version, the providers and the protocol
  version;
- `action`: the kind, the address, the result (and the error), the remote
  id, and a digest of the redacted diff;
- `tick`: the world as the executor saw it, as an HMAC with the stack's key;
- `apply_end`: `ok`, `declined` (the confirmation was answered no), or
  `failed` and the error;
- `controller`: each event, holds for approval and the run's result; and
  `rekey` and `handover`.

Who is `DFORM_ACTOR` when the environment sets it (say a CI job's OIDC
subject), else `user@host`. Secrets never appear: a diff is a digest of its
redacted form, where a sensitive leaf is already the stack's HMAC of it.
Nothing in plan or apply reads the log as truth (E DR-16).

```bash
cargo run -- -C examples/demo log dform env=staging                     # SEQ TIME KIND field=value ...
cargo run -- -C examples/demo log dform env=staging --since 2026-09-28  # or --since SEQ; --json for one JSON array
cargo run -- -C examples/demo log verify dform env=staging              # the chain holds, or the first broken link
```

`log verify` checks that every entry's hash is its content's, that its
`prev` is the entry before's hash and that `seq` counts from 1, and fails
naming the first entry that breaks the chain: an edited entry by its own
hash, a removed or reordered one by the next entry's `prev`.

`dform diff --since REF TARGET` explains what changed between applies:
each apply since REF (a sequence number, a time or a prefix of one, or a
git commit an apply recorded), each deformation it applied with why it was
planned, as `plan --why` prints it, and then the inputs and the stated
rows (a table's, the program's facts) that differ between the apply
before REF and now. The log holds no value, so the explanations are
computed again: each apply by the program as it was then, read at the
commit its `apply_start` recorded (the project copied out of git and
evaluated with the inputs its `plan` entry recorded; a secret one is
recorded only as a digest, so it is left out), or by the program now
when it is the program then by the digests the `plan` entry recorded.
Outside a repository the program now explains every apply, and a `note:`
says where it changed since. An update is explained by the attributes
whose values differ from the apply before's, a delete by what derived it
at the apply before. `--json` prints one document: `applies`, each with
its `deformations` and their `why`, and `changed` (`added` and `removed`
rows, `inputs` with `before` and `after`). Secrets print as their label.

```
$ dform diff --since 9 net
apply 11 2026-10-02T15:34:17Z by simon@host at 7e11a8c78679: ok
+ net.subnet["private-us-test-1c"]
  by stacks/net.df:9  resource net.subnet "private-${z}" { .. } where zone(z, n)
  because stacks/net.df:7  net.vpc["main"].cidr = "10.0.0.0/16"
  because data/zones.csv:4  zone("us-test-1c", 3)
changed since apply 3 2026-10-01T09:12:40Z:
  + data/zones.csv:4  zone("us-test-1c", 3)
```

`--audit-sink CMD` (or the stack's `audit_sink = "CMD"`) also pipes each
entry, a JSON line, to `sh -c CMD`, once per entry: a SIEM forwarder, say.
A sink that fails is a warning, never a failed apply; the local log is
authoritative.

## Asking the fact store

`dform query` evaluates a pattern, or a conjunction of body literals, against
the final fact store and prints a table with one column per variable:

```bash
cargo run -- -C examples/demo query 'attr(net.vpc, n, "cidr", c)' dform env=prod
# N                    C
# "network.main::vpc"  10.20.0.0/16
# "network.peer::vpc"  10.21.0.0/16
# (2 rows)
cargo run -- -C examples/demo query 'attr(t, a, "cidr", c), want(t, a), t != net.subnet'
cargo run -- -C examples/demo query 'want(net.vpc, "network.main::vpc")'    # yes / no
cargo run -- -C examples/demo query want                                    # every want fact
cargo run -- -C examples/demo query 'net.vpc["network.main::vpc"]'          # its attributes: path, value
cargo run -- -C examples/demo query 'net.vpc["network.main::vpc"].cidr'     # one attribute's value
```

`query --json` prints one document: `{query, count, facts}` for a predicate
name, `{query, columns, count, rows}` for a pattern, values spelled as in
`plan --json`.

Secrets print as their label, `(sensitive T["A"].p)`: a value at a
`sensitive` path, and any value equal to it or string containing it, so a
rule that forwards a secret does not leak it either.

Rows print values as the program writes them: a reference is the address
it names, `google_sql_database_instance["db"].name`, an unknown its
`?T["A"].p` label.

`dform why PATTERN` prints how a fact was derived, from the provenance
circuit every evaluation records (proposal E §3, DR-10), in the program's
own terms: each statement that fired, as written, at its `file:line` (a
block shows the entry that fired, the rest elided as `..`; a pack's or a
module instance's statement names it), then `with` the statement's
variables as they were bound, by their names in the source, and under them
each computed term of the statement with its value: an interpolation, a
function call, a read (`cfg.backup_days = 14`), a lookup
(`settings[env] = settings["prod"]`), an unknown as its `?` label. Under
that are the facts the firing read, recursively, each spelled as the
program names it: `net.vpc["main"]` for a resource, `net.vpc["main"].cidr
= 10.0.0.0/16` for an attribute, `input env = "prod"` and `let cfg =
"prod"` for a cell, `settings["prod"].backup_days = 14` for a settings
row, a relation as `zone("us-test-1a", 1)`. An attribute is `merged from N
contributions`, each with its value, its rank when it is not normal
(`@default`) and the statement that made it. A fact given to the run says
where it came from: its `file:line`, `--set env=prod`, the provider
schema, the world, the plan for the facts the planner hands to the policy
pass. Variables are allowed and every match is printed. A fact derived
more than one way shows its first derivation and `... N more
alternatives`; `--all` shows them all. `--core` prints the same tree in
the core's spelling: the lowered rules by id (`by r17: head :- body`),
their variables, facts as relations, the aggregate as `Σattr`. An address
as plan prints it is a pattern too: `why 'T["A"]'` explains the resource's
`want`, `why 'T["A"].path'` the attribute's `attr`. An `attr`/`arg`
pattern may name part of an object attribute, by dotted path or by object
value, and then shows only the contributions that hold it:

```bash
cargo run -- -C examples/demo why 'attr(net.vpc, "network.main::vpc", "tags.team", "platform")' dform env=prod
# net.vpc["network.main::vpc"].tags = {component: "network", env: "prod", team: "platform"}
#   merged from 2 contributions
#   ├─ {team: "platform"}
#   │    policies/baseline.df:10  set r.tags = { team: "platform" } where r in resource   (policy baseline)
#   │    with r = net.vpc["network.main::vpc"]
#   │    └─ net.vpc["network.main::vpc"]
#   │         modules/network.df:11  resource net.vpc vpc { .. }   (module network instance main)
#   ...
#   └─ ... 1 other contribution (--all)
cargo run -- -C examples/tour why 'db.postgres["orders"].backup_days' tour env=prod
# db.postgres["orders"].backup_days = 14
#   merged from 2 contributions
#   ├─ type_refine("db.postgres", "backup_days", "range(1, 35)")   provider schema
#   └─ 14
#        stacks/tour.df:155  resource db.postgres orders { .. backup_days = cfg.backup_days .. }
#        with cfg.backup_days = 14
#        ├─ let cfg = "prod"
#   ...
```

`dform dev graph` prints Graphviz DOT, nodes and edges sorted:

```bash
cargo run -- -C examples/demo dev graph | dot -Tsvg > resources.svg   # resource DAG: A -> B when A reads B (a ref, a null)
cargo run -- -C examples/demo dev graph --strata                     # partition graph, a cluster per stratum, negative edges dashed
cargo run -- -C examples/demo dev graph --relation vpc_peer/2        # any binary relation of the fact store
```

`dform dev effects` prints, per scope (the stack, each module instance, each
pack in use), what it reads
(inputs by name, settings leaves by path, world types, externs by name,
another instance's outputs), writes (cells as `(type, path)` partitions,
`*` for a variable type or path, settings leaves, another instance's input
cells) and offers (its declared outputs, with their types). Read off the
lowered program's rule heads and bodies and the partition graph; no
evaluation.

```bash
cargo run -- -C examples/demo dev effects                  # every scope, text
cargo run -- -C examples/demo dev effects --json           # one JSON document
```

## Chaos: failure and latency injection

`dform dev --chaos SPEC apply` (repeatable) makes the fake provider misbehave, the way a
real cloud does. Deterministic: nothing sleeps and nothing is random. The world
file keeps a `tick` counter; every `apply` is one tick.

| SPEC | Effect |
|------|--------|
| `fail=T["N"]` | Apply of `T["N"]` fails before it reaches the world |
| `timeout=T["N"]` | Apply of `T["N"]` takes effect, then times out: the world has it, state does not, until the next run finds it (see below) |
| `crash=T["N"]` | the provider process dies (exit 137) as it is called to Apply `T["N"]`: the action fails, nothing after it runs, and the next `apply` resumes (the mock linked in, `dform-direct`, is gone from that call on instead) |
| `stop-after=N` | dform itself stops, as if killed, once `N` Apply calls have returned (counted across the run's ticks), each persisted: nothing still in flight is waited for, the tick never ends, and the next `apply` resumes. The executor's knob, so it works with any provider |
| `read-lag=T["N"]:K` | the first `K` Reads of `T["N"]` after it is created return nothing (eventual consistency) |
| `mutate=T["N"].PATH=JSON` | once per run, after the first tick `T["N"]` exists at, the world sets its `PATH` to `JSON` (drift) |
| `latency=T["N"]:MS` | Apply of `T["N"]` takes `MS` on a simulated clock, reported, never slept; the world's `timeline` records each call's start and end |
| `fresh-ids` | every Create mints new ids (the world keeps a `serial`), as a real cloud does; without it a destroy-first replacement under the same name gets its predecessor's id |

```bash
cargo run -- -C examples/demo dev --chaos 'fail=net.subnet["network.main::private-us-test-1a"]' apply dform env=staging
cargo run -- -C examples/demo dev --chaos 'mutate=net.vpc["network.main::vpc"].cidr="10.9.0.0/16"' apply dform env=staging
```

Refresh reads every object state maps; a Read that returns nothing is retried
up to the type's `type_retry(T, Attempts)` (a schema fact, default 3), each
retry logged on stderr as `retry T["N"] read (2/3)`. A lag within that budget is
not drift; an object still missing after the last attempt is taken as gone
(`read T["N"]: nothing after 3 attempts; taken as gone`).

Addresses are written as the plan prints them, `T["N"]` (quote the spec for
the shell), and must name a resource of the stack. The world is
saved after every action, and state (the identity mapping) is written after
every Apply call that returns, so a failed or killed apply leaves exactly what
a real cloud would: a failure or a crash at action N leaves the N-1 identities
before it in state.

A call whose outcome dform does not know (it timed out, the provider
crashed, or dform stopped with it in flight) is recorded as `uncertain` in
state, and the next run resolves it before it plans, a `resolved: ...` line
on stderr each. Every Create and Replace carries an idempotency key
(`ApplyRequest.idempotency_key`, written to state before the tick's first
call): the next run asks the provider for the object that key made (the
`managed` capability's `provider.created` Query). Found, state maps it;
not found, the Create is sent again with the same key, and the protocol
says the same key twice never makes two objects. An uncertain delete is
resolved by a Read; an uncertain update is planned again from what the
refresh Reads. The mock records each key on its object and answers the
lookup; the Kubernetes provider has no lookup, and puts the key on the
object as the annotation `dform.io/idempotency-key` instead: a Create with
the same key finds the object by name and answers with it (a generated
name's suffix comes from the key, so the name is the same too).

## dform model (current)

The grammar is `docs/grammar.md` (edition 2026; proposal H's surface, one
spelling per construct, `proposals/H-small-surface.org`). Every `.df` file
starts with `edition 2026`; the first token decides what a statement is,
and a newline ends it. Case decides nothing: names are resolved. A constant
is quoted (`"prod"`), a path that is data too (`"tags.team"`), a variable
is a lowercase name bound where it is written, an input or a `let` is read
by its name (`env == "prod"`), and a resource in scope by its name
(`vpc.cidr`); anywhere else a resource is its address, `T["A"]`
(`net.vpc[b]`, `db.postgres["database.main::db"]`), the spelling `plan`
prints and every command takes. `.` is static and `[ ]` a key computed at
run time. A dot is a reference where it is a whole value (a field:
`vpc_id = vpc.id`) and a read everywhere else. `-` and `/` are operators,
so hyphenated names are strings (`"us-east-1"`). A syntax error names
`file:line:col` and what was expected, and parsing goes on to the next
statement, so every error in a file is reported at once.

`dform fmt [PATH...]` formats files in place (no PATH: the project's `.df` files):
each construct in its normal form (a body on one line when it fits in 100
columns, else `where { .. }`; `{ a }` for `{ a: a }`; `==` between bound
sides; docs/grammar.md "Formatting"), two-space indentation per open
bracket, block or body, one space around operators and after commas,
`{ a: 1 }` inside braces, at most one blank line, and no comma where a
newline already separates block entries. A formatted file prints back byte
for byte, and a file with a syntax error is reported, not rewritten.
`--check` rewrites nothing and fails listing the files that would change.

- Core intent IR (what the surface lowers to; `why --core` and `strata` print it):
  - `want(Type, Name)` declares a resource instance.
  - `arg(Type, Name, KeyPath, Value)` contributes attributes (KeyPath supports dots).
  - `ref(Type, Name, "attr")` expresses dependencies.
  - `collect_set(x)` / `collect_list(x)`, `count(x)`, `sum(x)`, `min(x)`,
    `max(x)` aggregate in a head, per group of the head's other arguments.
    `sum` folds ints, `min`/`max` ints or strings; a group with a value of
    another kind derives a deny, and one whose value is a null is stuck.
  - `deny("message", ctx)` and `warn(..)` are the checks, read after evaluation.

- The surface:
  - Every statement is `head where body`, and a block is a head:
    `resource Type name { key = value ... } where B`: the one clause, after
    the block, is a query (a resource per match), a field's reads hoist
    into the block's body; a name in quotes interpolates
    (`"private-${z}"`).
  - `head where body`, `head where { lit NL lit }`; `let name = term where
    body` is a value, read by name.
  - `set r.tags = { team: "platform" } where r in resource`: a contribution.
  - `deny "msg" { key: v } where body`, `warn ...`; the message interpolates.
  - `x in net.vpc` ranges over the wanted resources of a type; `r in T`,
    `has r.p`, `not r.p` (not true, absent included); `x in list`, and
    `x = list[i]` for the index too.
  - `let cfg = settings[env]` is a value whose type is the row's
    reference; `cfg.gke.pods_cidr` reads through it.
  - settings blocks: `settings prod { db.backup_days = 14 }`.
  - `output k: T = t where body`: an output in one statement.
  - literals: lists `[a, b]` and objects `{ k: v }` (`{ a, b }` is `{ a: a, b: b }`).
  - list comprehensions: `[x | pred(x), pred2(x)]` (lowers to a `collect_list` rule).
  - expression terms: `ib = ia + 1` lowers to `IB = add(IA, 1)`.
  - `import "path"` includes another file, once.

- Schemas and wildcards:
  - `decl pred(field_one: type, field_two)` declares a relation by its
    columns (a type optional); named arguments match by them:
    `pred(field_one: x)`.
  - A `decl` of a relation no rule defines declares one a provider feeds
    (it may have no rows).
  - `decl pred(a, b) mixed` lets a predicate have both ground facts and rules (E §2.6); without it, one that has both is a compile error naming the rule and the fact.
  - `_` is a placeholder (matches anything, never binds, never read: `_.p` or `_` as a value is an error); `settings _ @default { .. }` contributes to every settings row that exists. `_x` is an ordinary name.

## Externs

An extern is a relation a provider answers on demand, with a binding
pattern: `+` columns are inputs, `-` columns answers. A program does not
declare one: `provider NAME {}` brings the provider's externs into scope,
with their modes from its schema.

```dform
provider file
provider random

resource google_monitoring_dashboard pngu {
  dashboard_json = file.json["files/dashboard-pngu.json"]
}
```

A body literal of an extern is asked once the literals before it bind its
`+` arguments (an input that is a null waits), and the answers are facts of
the extern with those inputs (`why` shows them as extern calls). An extern
under `not`, in a recursive rule, stated by the program, or with an input
nothing before it binds is a compile error. Evaluation is by rounds: every
call the rules demand is asked once, then the program is evaluated again,
until no call is new.

`file`, `env` and `random` are built-in fact providers, declared like any
provider and needing no `dform.toml` source:

| provider | externs                                                   | answered by |
|----------|-----------------------------------------------------------|-------------|
| `file`   | `file.json(+path, -value)`, `file.text(+path, -value: string)`, a path from the program's directory | dform |
| `env`    | `env.var(+name, -value: secret(string))`; `env.var(NAME)` as a term reads it | dform |
| `random` | `random.password(+key, -value: secret(string)) persist`   | the provider the stack configures as `random` (the mock in examples/crud-api), which holds the secret |

A program that writes `extern file.json(..)` is told to write `provider
file {}` instead; `extern` is the schema's word (provider schemas, the
compiler's tests). Another provider's externs are, for now, still declared
in the program until its schema is read at compile time (DESIGN.org R-24),
and the mock answers them from `providers/<name>/externs.df` beside the
provider's schema: facts of the extern, the rows whose `+` columns are the
inputs.

The plan file records the answers the plan read (not a call with a
`secret(...)` column), and `apply PLAN` asks none of them again. A
`persist` extern's answers are kept in state and never asked again, so a
generated password stays the same across runs. `dform state taint STACK EXTERN
ARGS...` forgets one of them (its input values written as `--set` takes a
value), so the next plan asks the provider again:

```bash
dform state taint p random.password app    # the next plan generates a new one
```

A `secret(T)` column (`random.password`'s value) never enters dform (E DR-19). The Query names the secret columns
(`QueryRequest.secret`), and the provider answers each with where it holds
the value: a SECRET null whose `held` names the provider, the deployment,
the extern and its inputs, the column, and the value's keyed digest (with a
key derived from the deployment's plan key, `digest_key` at Configure). The
run has a secret null, labeled `random.password/crud-api-db#2` (the extern,
its inputs, the column from 1); a sensitive field it reaches goes to the
provider in the Apply document as that label and where it is held, and the
provider reads the value there, inside the call. A persisted answer keeps
the label, the digest and the reference in state, and a replay hands the
reference on; a provider that answers a secret column with its value is
refused. The mock keeps what it answered in its world (`held`), a real
provider in its own store (a secret manager).

What dform keeps of a world document beyond a run, the in-flight record of
an interrupted apply and the controller's baseline, holds a leaf at a
`sensitive` path as its keyed digest, `"(sensitive hmac-sha256:..)"`, and
the resume and the controller compare the world with it the same way.

## Tables

A table is an input relation whose rows are a data file's, typed column by
column:

```dform
input peering(env: enum("dev", "stg", "prod"), name: string, peer_network: string) from csv("data/peerings.csv")
input pins(app: string, image: string) from yaml(git("ops.git", "env/${env}", "pins.yaml"))
```

The formats are `csv` (a header naming the columns), `json` and `yaml` (a
list of objects), and `toml` (the rows as `[[peering]]` entries). A row has
every column and nothing else; a cell is its column's type (a CSV cell is
read as an `int`, a `bool` or an `inet` when the column is one, a string as
an `inet` in any format), and a row that is not is an error naming the file
and line: `data/peerings.csv:3: column env: "qa" is not enum(dev, stg,
prod)`. A `secret` column is a compile error: rows are read in the clear.
The loader never reshapes: transforms belong in rules. Paths are relative
to the declaring file; `peering(env: e, name: n)` reads a row by its
columns' names.

A table is an extern (see "Externs"): the source, `path` or `git(repo, ref,
path)` with holes (`${env}`), is its bound input, so rules may compute it; a
source that reads the table's own rows is the extern-in-a-recursive-rule
compile error. The rows are its answers: the plan file records them, and
`apply PLAN` reads none again. `why` names each row's line,
`data/peerings.csv:3` (`ops.git@a9d0f11:pins.yaml:12` from git).

A git source's ref is resolved to a commit first (a ref that names none is
an error naming the repository and the ref, never an empty table), and the
rows are read at that commit (a bare repository works). The plan file holds
the commit, so `apply PLAN` applies what plan saw even when the branch has
moved since. State keeps the commit each deployment was last applied from,
and a plan whose ref names another commit says so before the plan:

```
pins: ops.git env/prod 3b1c7e0 -> a9d0f11
```

The controller watches what the last run's tables read: a changed file,
or a ref that names another commit, is an input event (`input pins changed
(git ops.git env/prod:pins.yaml)`).

A keyed stack's `config` (dform.toml) is a table of its settings, per
deployment, `{k}` the value of its key `k`:

```toml
[stacks.dform]
config = 'yaml("config/dform/{env}.yaml")'
```

Every leaf of the document (a mapping; a CSV with the columns `path` and
`value`) is a contribution at the normal rank to the settings row named by
the key's value (several keys' joined by `/`), at the leaf's dotted path:
`db: { backup_days: 14 }` in `config/prod.yaml` is `set settings["prod"].db.backup_days =
14`, and wins over an `@default` layer per leaf. A leaf at a path the program
neither writes nor reads is a deny naming the file and line (a typo).
The demo's per-environment settings are `config/dform/{env}.yaml`; its CIDRs
are strings there, made inets by `inet(...)` where they are used.

## Escape hatches

### List membership

`x in e` "explodes" a list, a setting's included, into rows (it lowers to the
built-in `member(List, Item)`):

```dform
host_ip(e, ip) where ip in settings[e].vm.ips
```

### Discovery facts

The fake backend can inject facts from `dform.state/inventory.json`:

- `cloud_exists(Type, Name)`
- `cloud_attr(Type, Name, Path, Value)`
- `cloud_computed(Type, Name, Path, Value)`

These are intended to model provider data sources / inventory. A provider
with the `inventory` capability answers them; a run asks it only about the
types the program reads the world of (`world.T[e].p`, `x in world.T` with
`T` a constant; every type for `query` and `why`, or when a type is
computed).

To reference discovered values in resource attributes without manually joining
`cloud_attr/cloud_computed`, you can use `cloud_ref(Type, Name, Attr)` as a value
term. In the fake backend it resolves against `dform.state/inventory.json`.

`Attr` is a path string, dotted and indexed: `"tags.owner"`, `"subnets[0].id"`.

### Adopt existing resources

`adopt(r, RemoteName)` marks a desired resource `r`, a reference, as existing already.
Planning will produce an `Adopt` action (`>` in plan output) instead of `Create`.

```dform
adopt(net.vpc["network.main::vpc"], "existing-prod-vpc") where env == "prod", "existing-prod-vpc" in world.net.vpc

set net.vpc["network.main::vpc"].adopted_id = cloud_ref(net.vpc, "existing-prod-vpc", "id") where env == "prod"
```

`net.vpc["network.main::vpc"]` is an address: resource `vpc` of module
instance `network.main`, spelled as `plan` prints it.
```

### Stack inputs

A program declares its inputs, typed, with an optional default and an
optional refinement:

```dform
key env: environment = "staging"                    # the target gives it: `dform plan app env=prod`
input replicas: int = 2 check 1 <= replicas, replicas <= 10
input allowed_cidrs: list(inet) = []
input owner: string                       # required: no default

type environment = enum("dev", "staging", "prod")   # an alias: the enum wherever it is written
```

Inputs, keys and imports are the file's header: after `edition`, before
the body (`import`, then `key`, then `input`), so a file says what it
takes first; one written below the body is an error, and `dform fmt`
moves it. Each is read as a relation, `env(E)`. An input is a cell of the attribute
aggregate: the default is an `@default` contribution, `--set replicas=3`
a normal one that wins (and `why` shows both). A `key` is an input the
target gives instead (`dform plan app env=prod`), and its value names the
deployment (see "Keyed stacks"); `--set` of one is an error. `--input-file FILE.df`
(repeatable) gives inputs as facts, one per input, `env(prod).
allowed_cidrs([inet("10.0.0.0/8")]).`, each a normal contribution stated
where the file states it; the plan file records each input file's digest.

Types are `int`, `string`, `bool`, `inet`, `enum(a, b, ...)`, `list(T)`,
`set(T)` and objects `{ k: T }` (`addr`, `ref(...)` and `any` are
unchecked). `type NAME = TYPE` names a type anywhere a type is written; an
imported file's aliases are in scope, and a module's once it says `export
type NAME` (docs/grammar.md "Type aliases"). A `--set` value is read as its input's type (an `inet` parses,
a `string` takes the text) and checked before evaluation: `--set
replicas=two` is an error naming the input and its type, and so is `--set` of an input
the program does not declare. A value the program computes (a module
instance's input) is checked after evaluation and a wrong type blocks the
plan. A required input with no value is an error at its declaration. `check
R` refines the input (`R` names it by its name; see Refinement types).

A program with no `input` declarations reads `--set k=v` as the fact
`input("k", v)`.

### Refinement types

A `check` on an input, on an attribute of a `type` block, or a provider
schema's `type_refine(T, Path, C)` fact refines a value:

```dform
type settings {
  db.backup_days: int check 1 <= db.backup_days <= 35
  gke: { control_plane_cidr: inet check inet.prefix_len(control_plane_cidr) == 28 }
}
type gke_cluster { zones: list(string) check len(zones) >= 3 }
```

A `check` over the value alone that fits the checkable table is a
constraint in the attribute's cell: `lo <= x <= hi` is `range(Lo, Hi)`,
`inet.prefix_len(x) <= N` (`>=`, `==`) is `prefix_len_le(N)` / `prefix_len_ge(N)`,
`len(x) <= N` is `len_le(N)` / `len_ge(N)`, `x in [..]` or `x == v` is
`enum([..])`, `matches(x, "re")` is `regex("re")`; a `type` block's `int`,
`string`, `bool`, `inet` or `enum(...)` is a type check. A schema writes
the same terms: `type_refine(net.subnet, cidr, prefix_len_le(24)).` The
constraint is rank-blind: it is checked against the value that wins, so an
`@override` cannot get past it. A violation is `deny("refinement
violated", {type, addr, path, constraint, value, reason, at, witnesses})`
and the plan lists it with the conflicts; a literal that violates one is a
compile error naming both places. A value that carries a null is checked
when the null resolves: the plan prints `? refinement on ?T["A"].p deferred`
in its undetermined section, and a violation found at the boundary stops
apply like any deny between ticks (`examples/refine`: `dform apply --set zones=2`
there stops after tick 1; its default, three zones, applies). On a path the
schema marks `sensitive` the engine never checks the value: the refinement
goes to the provider as an Apply assertion, checked once the secret is
materialized, and a provider whose Schema does not declare
`checks_refinements` makes it a compile error (E0306). Anything else (one
bound alone, another attribute, a user predicate) lowers to a deny with the
refinement's place, the attribute and the others it names read as their
values (`inet.prefix_len(net) >= inet.prefix_len(wide)`); a call to a function the
evaluator does not have, or a `matches` pattern that does not compile, is
a compile error there; a secret input's refinement is always one, and it does
not print the value. A `type` block's flags are not supported yet (they
come from the provider's schema).

### Modules

A module groups rules behind an interface; an instance of it scopes them
(E DR-3, Terraform-module-like):

```dform
module network {
  input vpc_net: inet                        # set by each instance
  input zones: list(string) = ["a", "b"]     # a default: @default rank

  resource net.vpc vpc { cidr = vpc_net }
  zone_index(z, i) where z = zones[i]        # private
  ...
  output vpc: net.vpc = vpc                  # an address output
  output private_subnet_ids: list(ref(net.subnet)) = [s.id | s in net.subnet]
}

instance network main { vpc_net = settings[env].network.main.vpc_net }
instance database main { subnet_ids = network.main.private_subnet_ids }
```

Inside an instance:

- resource names are scoped, `network.main::vpc` (its address from outside
  `net.vpc["network.main::vpc"]`), in `want`, `arg`, `attr`, `adopt` and
  `ref`; inside the module `T[e]` is relative to the instance;
- every predicate the module defines is private to the instance: another
  instance's `zone_index` is a different relation, and reading it from
  outside is an error naming the module. A value leaves the instance
  through an output the stack wires: the demo's database and kubernetes
  modules each give `output iam_need = {..} where ..`, and the stack writes
  `iam_need("app", n.action, n.resource) where n = database.main.iam_need`;
- `input k: T [= D] [check R]` is read by its name `k` inside the module.
  The instance's `k = v` (under its `where` clause) is a normal-rank contribution to the cell `(input, m.i, k)` of the
  attribute aggregate and `D` an `@default` one, so `why` shows both. An
  instance that sets an undeclared input, or leaves out one with no
  default, is a compile error. `check R` refines the input (`R` names it
  by its name; see Refinement types);
- `output k: T = t [where B]` declares an output and gives it its value in
  one statement (the type is optional), read anywhere as `m.i.k`
  (`output(m.i, k, V)`); an output typed by a resource type (`output vpc:
  net.vpc = vpc`) is the scoped address of the instance's resource. `network[i].vpc` reads it with a
  variable instance.

The module reads every global relation; cross-instance values go through
outputs.

### Policies

Policies are packaged as policy packs and used explicitly. A pack is a
module applied once: its own relations are private, and it writes any
attribute without a grant; ranks are the ownership model and `why` names
each contribution's pack. The stratifier partitions a write by its head's
constant type and path (`*` where the head has a variable).

```dform
policy baseline {
  set r.tags = { team: "platform" } where r in resource
  deny "db must be private" { resource: pg } where ...
  warn "prod should enable audit logging" { env: "prod" } where ...
}

use baseline
```

Every contribution to one attribute meets in one lattice cell; objects merge
per key, and a list path several sources contribute to is declared a set:

```dform
type_lattice(iam.policy, "statements", "set")
```

Settings are the same aggregate:

```dform
type_lattice(settings, "audit.sinks", "set")
set settings["prod"].audit.sinks += ["s3"]

settings prod {
  audit.sinks += ["cloudwatch"]
}
```

## Testing

The denies are the tests; there is no test or scenario syntax (R-32).
`dform test [TARGET] [K=V..]` evaluates the program once for every
combination of its inputs against an empty mock world (the provider's
schema, no world, no state, nothing written), and every deny must hold in
each. The input space: an input the target pins (a key's `K=V`) or
`--set` pins is that value; an `enum` input takes each of its values, a
`bool` both; a key whose type is not an enum takes each value a
deployment of the stack was applied with; any other input takes its
default, and one with none is an error naming it (pin it, or give it an
enum type). More than 4096 combinations is an error asking to pin some.

```dform
deny "prod keeps 14 days of db backups" where env == "prod", not db.postgres["database.main::db"].backup_days == 14
deny "dev has no database" where env == "dev", _ in db.postgres
```

It prints one line per combination, `ok` or `denied` (or `error`, for
one that does not compile), and the command that plans it, then each
deny under a denied one; it exits non-zero if any failed:

```
test dform: 3 combinations of env
ok      dform plan dform env=dev
ok      dform plan dform env=staging
ok      dform plan dform env=prod
test dform: 3 combinations, 0 failed
```

`dform test shop env=prod` pins the key, so only prod's combinations
run. A what-if plan is `plan --set k=v`.

```bash
cargo run -- -C examples/demo test dform
cargo run -- -C examples/demo test dform env=prod
```

## Controller mode

`dform controller run` is the second executor over the same evaluator: the
plan is the diff a reconciler applies, so nothing in the language changes.
It waits for an input relation's source or the world file to change, then
does what `apply` does (refresh, evaluate, plan, the policy pass, ticks
until the plan is undeformed or `--max-ticks`), gated by policy, and logs
one line per event and per tick:

```
04:22:01 event start
04:22:01 tick 1: plan: 3 deformations (3 create)
04:22:01 stack workload is undeformed
04:22:07 input release changed (file release.facts)
04:22:07 event input release
04:22:07 tick 1: plan: 1 deformation (1 update)
04:22:07 stack workload is undeformed
04:22:12 event world dform.state/workload/remote.json changed
04:22:12 drift k8s.deployment.web spec.replicas: 3 -> 5 (auto_reconcile)
04:22:12 tick 1: plan: 1 deformation (1 update)
04:22:12 stack workload is undeformed
```

```bash
cargo run -- -C examples/bootstrap controller run workload                   # poll every 500ms
cargo run -- -C examples/bootstrap controller run workload --poll 100 --max-events 3   # stop after 3 events
cargo run -- -C examples/bootstrap controller run workload --once            # what changed since the last run
```

The target names the stack, and of a keyed stack one deployment
(`controller run 'app[env=prod]'`). `--poll MS` (default 500)
is how often the sources and the world file are looked at: polling, no file
notification. `--once` handles what changed since the last run (`event
resync` when nothing did) and exits; `--max-events N` exits after N events
(the start counts). A run that fails is logged (`error: ...`) and the
controller goes on watching; a failure of the first run ends it. Times are
UTC. Every run re-reads and re-evaluates the whole program.

Input relations feed facts from outside the program, re-read whenever
their source changes (a table's too, see "Tables"); `plan` and `apply`
read them too:

```dform
input release(image) from facts("release.facts")
input approve(t, a) from facts(git("ops.git", "main", "approvals.df"))
```

A source is a `.df` file of facts (`edition 2026` first) of the
relations declared from it; a fact of any other predicate is an error
naming it. Paths are relative to the declaring file. A `git` source is read
at the ref with `git show REF:PATH` (a bare repository works) and changes
when the ref names another commit.

The controller keeps `controller.json` beside the stack's state (in its
store: a directory, or the bucket of an s3 stack, as are its `approvals/`
drop directory and `approval-pending.json`): the stamps
of the sources and the world file as its last run left them, and the world
as that run accepted it (the baseline). It says what changed (`event start`,
`event input NAMES`, `event world`, `event resync`) and hands the world's
difference from the baseline to the program as facts, `drift(T, A, Path,
Before, After)` (one per leaf, list elements by index; `Path` "" and `After`
`absent` for an object that is gone). Policy decides about it, and the
controller gates every tick on the policy pass:

- `hold(T, A, Reason)` holds `T["A"]`'s deformation: `tick N: proceed: held,
  Reason: T["A"]`, for as long as the policy derives it.
- `requires_approval(r, Reason)` holds a deformation until a signed
  approval of the plan's digest arrives (see "Approvals").
- Drift of `T["A"]` is corrected when every drifted path is
  `auto_reconcile(T, A, Path)` or `approve(T, A)` holds, or the event is an
  input change. Otherwise the deformation is held (`drift at PATH needs
  approval`) and its baseline kept, so it is held again at every event
  until an approval or an input change.
- Held drift is per resource, not per path: a deformation is one action,
  so an object with `auto_reconcile` drift (replicas) and other drift (the
  image) holds the whole update, replicas included. Per-path holding waits
  for `ignore_changes` to address list paths.

The controller parses the program once and again only for a file whose
text changed, and drops what an event registered for diagnostics with the
event, so a long-running controller does not grow per event.

The rest of the plan still applies. `examples/bootstrap/stacks/workload.df` is the
demo: replicas are `auto_reconcile`, any other drift waits for `approve`,
and in prod a rollout waits for an approval of its plan (`approval/1`).

## Bootstrap and handover

One program creates the cluster and installs dform in it; the workload's
state then moves where the in-cluster controller reaches it, a bucket, and
the controller runs it from there. `examples/bootstrap/` is the demo, on
the mocks and MinIO (`crates/dform-s3/minio.sh`; its test,
`tests/controller_bootstrap.rs`, takes the fake S3 server when MinIO is not
there):

```bash
eval "$(crates/dform-s3/minio.sh start)"
cargo run -- -C examples/bootstrap apply bootstrap   # 3 ticks
cargo run -- -C examples/bootstrap stack handover workload \
  --to 's3("dform-test", "renfry/workload", {endpoint: "http://127.0.0.1:9000"})'
cargo run -- -C examples/bootstrap controller run workload
crates/dform-s3/minio.sh stop
```

The bucket must exist (dform makes none; the test makes `dform-test`).

`stacks/bootstrap.df` (stack `bootstrap`, mock GCP from `providers/gcp/schema.df` and
mock Kubernetes) creates the network, the subnetwork and the cluster in
tick 1; the node pools (one per zone, and the zones are the cluster's)
and the `dform-system` namespace (its provider is configured from the
cluster's endpoint and CA) in tick 2; and the `dform-controller`
Deployment, whose args name the workload stack, in tick 3, once its node
pool is up. `stacks/workload.df` (stack `workload`) is a namespace, a
Deployment whose image is the `release` input relation, and a Service.

`role = "bootstrap"` in a stack's `[stacks.NAME]` marks the stack that creates what the
controller runs in: it stays batch. `dform controller run` refuses it (by its
program or by the registry), and it is never handed over.

`dform stack handover NAME --to BACKEND` (NAME a deployment, `app[env=prod]`, of a keyed stack) moves the deployment's
objects (state, plan key, audit log, published outputs, controller memo)
to the backend, under the deployment's lock, and records it in the
registry, `dform.state/stacks.json` (`{"state": ..., "backend": ...}`
beside the plain state paths, absolute; the controller's `event world` line
names it relative to the root). The mock's world goes with the state into
a directory; for a bucket it stays in `dform.state/NAME/` (it is the
provider's). Every later run of the stack uses it, whatever its
`backend` says; a batch `apply` of a handed-over stack is refused (the
controller runs it), `plan` is not. The stack's state is found in the
registry, else where its backend says; the target must hold none
of a deployment's objects, and the stack not be locked. Backends:

- `local("DIR")`: a directory, relative to the project root, as a stack's
  `backend` is.
- `s3("BUCKET", "PREFIX", {endpoint: "URL", region: "R"})`: the
  deployment's own prefix of a bucket (a handover between prefixes, or
  between buckets, works alike).
- `k8s("namespace/name")`: the in-cluster backend. For now it stands in as
  the directory `k8s/namespace/name` inside the state directory of the
  registered `role = bootstrap` stack (there must be exactly one; an apply
  of a bootstrap stack registers it).

## Editors: the tree-sitter grammar

`tree-sitter-dform/` is a tree-sitter grammar for `.df` files, for
editors only: the compiler keeps its own parser (`crates/dform-core/src/syntax/`). It has
`queries/highlights.scm`, `queries/indents.scm` and `queries/locals.scm`
(nvim-treesitter capture names; a `#|` doc comment is a comment,
captured `@comment.documentation` too), and the generated `src/parser.c` is
committed, so an editor builds it with a C compiler and no tree-sitter
CLI. A small external scanner (`src/scanner.c`) makes a newline outside
brackets end a statement and reads a string's text around its `${e}`
holes.

The highlight query captures a dot in a field-value position as
`@variable.reference` (a field's value, a head or `output` argument, an
element of a list or object there, a comprehension's item) and leaves a
dot anywhere else a plain read, as proposal G (G-6) lowers them. This is
the syntax's answer: a chain whose head is a `let` of a settings row
or an instance output is a read in any position, which only the resolver
(and a language server) knows.

`tests/treesit_agreement.rs` holds the two parsers together: every file
the compiler reads parses without an ERROR or MISSING node, both trees
have the same statements, blocks and literals at the same byte ranges, a
`tests/syntax/err` file has a tree-sitter error exactly when it has a
syntax error, and the reference capture fires on field values in
`dform.df` and not on reads. After changing `grammar.js`, from
`tree-sitter-dform/`:

```bash
tree-sitter generate      # rewrites src/parser.c; commit it
npm run corpus            # rebuild test/corpus/repo/ from the repository's .df files
tree-sitter test
```

(`npm install` there brings the CLI as a dev dependency if `tree-sitter`
is not on the PATH.)

### Emacs: `dform-ts-mode`

`editors/emacs/dform-ts-mode.el` is a `treesit` major mode for `.df`
files (Emacs 29.1+; developed against Emacs 30/32): font-lock from
`highlights.scm` (the reference capture gets `dform-reference-face`,
underlined by default, a doc comment `font-lock-doc-face`), indentation from `indents.scm`, and imenu and
defun navigation for rules (by head predicate), modules, instances,
resources (by type and name), stacks and policies.

With straight.el, from a local checkout of this repository:

```elisp
(use-package dform-ts-mode
  :straight (:local-repo "/path/to/dform" :files ("editors/emacs/*.el")))
```

With `package-vc`, from a Git remote:

```elisp
(use-package dform-ts-mode
  :vc (:url "https://example.com/simon/dform.git" :lisp-dir "editors/emacs"))
```

Either way, `M-x treesit-install-language-grammar` (language `dform`)
builds the grammar; a local checkout of this repository is found
automatically (`dform-ts-mode.el` looks for `tree-sitter-dform/` next
to `editors/`), a standalone install needs the repository's URL and
`:source-dir "tree-sitter-dform/src"` in `treesit-language-source-alist`.

The eglot half: an `eglot-server-programs` entry for `dform lsp` (the
language server below), and two commands, `dform-select-environment`
and `dform-why-at-point`, wired to `eglot-execute-command` (command
names `dform.selectEnvironment` and `dform.why`). No `lsp-mode`
dependency.

`editors/emacs/test/dform-ts-mode-test.el` holds the `ert` tests
(font-lock faces and an indentation round trip on the fixtures under
`editors/emacs/test/fixtures/`), run with:

```bash
emacs --batch -Q -L editors/emacs -l ert \
  -l editors/emacs/test/dform-ts-mode-test.el \
  -f ert-run-tests-batch-and-exit
```

## Language server

`dform lsp` serves the language server protocol on stdin and stdout
(`crates/dform-lsp`, lsp-server; synchronous, one evaluation at a
time). An edit re-evaluates the edited file's project once edits pause
for 300 ms: every stack discovery finds, as `dform plan` does (one
evaluation, `dform_core::deployment`), up to the plan's policy pass, with
the open buffers' unsaved text. Evaluation is read only: the
deployment's recorded world and state are read, never written, nothing
is applied. State and published outputs in a bucket are read when the
server has the s3 backend's credentials (as `dform plan` takes them);
without, the deployment is evaluated as if nothing were deployed there,
with a warning. The providers are the mock linked into
the server; a real provider process is started only when the client
sets the initialization option `"dform.lsp.real_providers": true`. On
examples/demo an evaluation takes about 30 ms in a release build.

- *Diagnostics of the selected environment*: parse and compile errors at
  their spans; each `deny` and `warn` at the rule that derived it (a
  conflict at a contribution), the contributions below it as related
  information; lint warnings.
- *Contributors hover*: on an attribute in a resource block, a rule's or
  fact's name, or an attribute read in a rule's body (`a.cidr`): each
  attribute's collapsed value, the provider's description of its path
  (`type_doc`), the winning rank and every contribution with its rank and
  owner (rule, `file:line:col`, pack or module instance), then the
  derivation as `dform why --core` prints it.
- *Docs at point*: on a declared name, where it is declared or used, its
  declaration's first line and doc comment (docs/grammar.md "Doc
  comments"); an alias its definition; a module, or an instance, the
  module's docs and its inputs and outputs with theirs; `m.i.k` the
  output's; a schema type its description; a builtin or a keyword its
  signature, summary and an example (`engine::REFERENCE`). Point on
  anything else (whitespace, a comment, a literal, a variable) has no
  hover; the deployment is the `dform/environment` notification's, never
  a hover's.
- *Signature help* (`textDocument/signatureHelp`) in a call of a builtin
  (`inet.subnet(`) or of an extern the project declares (`dns.lookup(`,
  `dns.lookup[`): the signature, the argument point is in, and the
  builtin's summary or the extern's doc comment. A call being typed has
  one too.
- *Schema completion*: a resource block's paths (type, flags,
  refinements and description from the provider's schema facts) and an
  enum path's values; types after `resource`; an instance block's module inputs and
  `module.instance.`'s outputs; elsewhere the builtins and keywords a
  word starts, each with its signature.
- *Quick fixes* (`textDocument/codeAction`), each on its diagnostic: an
  unknown name (quote it), a predicate with both facts and rules (`decl
  p(a, b) mixed`), the collision lint (interpolate the key into the name, or
  say `isolated = true` on the stack), a required attribute nothing sets
  (a typed placeholder in the resource's block) and a ref to an address
  no rule wants (guard the block on it: `} where "other" in net.vpc`). An edit
  to a formatted file leaves it formatted.
- *References* of a predicate, an input or value name (a `{ k }` field
  included), a `let` or type alias, a module, an instance, a resource (by
  its name in scope), a settings row or a policy, across the
  project's files and unsaved buffers, read in the resolver's order
  (docs/grammar.md "Names"); a relation a module or pack defines
  is its own (two modules' private `helper` are two). On an attribute path (a field of a
  resource block, `r.p` in a body, an `attr` literal): every rule
  contributing to that cell, across modules and policy packs, as the
  hover lists them.
- *Rename* of the same names. Renaming a resource, an instance or a
  module whose addresses have state in the selected deployment adds, in
  the same edit, `moved(T, "old", new)` per address just after the
  declaration's block, so the next plan is a move and not a destroy and a
  create; an address written as a string at the top of a program
  (`net.vpc["network.main::vpc"]`, `"network.main::vpc" in net.vpc`) is
  renamed with it. A rename is checked: the selected deployment is evaluated with
  the edit applied to the buffers, and the rename is refused, naming
  what changed, if it adds a diagnostic or changes the plan in anything
  but the renamed addresses. `prepareRename` refuses keywords, builtins,
  schema types, attribute paths, provider names and dform's own
  relations (`data`, `attr`, ...), and an instance whose name is also a
  string a dynamic index `m[e]` may read (naming where the string is).
- Formatting (`dform fmt`'s formatter) and go-to-definition of modules,
  policies and predicates.

Two commands (`workspace/executeCommand`), which the Emacs mode binds:

| Command | Argument | |
|---|---|---|
| `dform.selectEnvironment` | a map of key values (`{"env": "prod"}`), `"env=prod"`, `"default"`; none: the choices | evaluate every stack of the workspace as that deployment; a key a stack does not have is ignored |
| `dform.why` | `{textDocument, position}` | the derivation text `dform why` prints for the attribute under the cursor (also shown as a message) |

With no argument `dform.selectEnvironment` returns `{current, choices}`,
each choice a `label` and its `keys` (key values from the
key inputs' enum types). After a selection the server sends the
notification `dform/environment` with `{label, deployments}`
(`deployments`: `dform[env=prod]`), for a mode line.

## Testing

`cargo test` runs the integration tests under `tests/` (one file per
concern: adoption, chaos, k8s, aws, ...) and every crate's own tests.
The protocol tests (`tests/protocol_*.rs`, `dform provider check`), the
executor's parallel and resume tests and every golden `plan` run on more
than one backend (`tests/common`'s `Backend`): the same cases, the same
output. `--parallel` schedules are exact on the direct backend, which
answers in simulated time.

`tests/s3.rs` plans and applies examples/demo with an s3 backend: two
concurrent applies, a killed holder taken over and resumed, a stale
holder fenced off. It runs against a fake S3 server in the test process
(`dform-s3`'s `fake`), and again against a real one when
`DFORM_S3_TEST_ENDPOINT` names it: `eval "$(crates/dform-s3/minio.sh
start)"` starts MinIO in rootless podman and prints the variables, and
`crates/dform-s3/minio.sh stop` removes it and its volume.

`tests/model.rs` is a model test of the executor: a seed picks a random
program over the fake schema and a few versions of it (refs, force_new
cidrs, `prevent_destroy`, `create_before_destroy`, `moved`), objects in the
cloud dform does not manage, and a schedule of applies and plan-file
applies under random chaos (every knob, `--parallel`, the order calls in
flight answer in, drift between a plan and its apply), then one apply
without chaos. Every run is the command line in the test's process over
the direct backend, behind a recorder that sees every provider call. It
checks that state on disk holds every answered Apply call by the
provider's next call, that no Apply names an object dform does not manage
or an address the plan file does not list, that a plan file is refused
exactly when a fresh plan differs, that nothing dform made is missing from
state, that no Create meets an object already there, and that the last
apply ends undeformed or on a deny. A Create or Replace that took effect
unanswered is excused only as an object state does not know yet, until the
next run resolves it (see "Chaos"). CI runs 300 seeds; the nightly
workflow 10^4. A failure prints the seed and the schedule minimized, and
replays with:

```bash
DFORM_MODEL_SEED=N DFORM_MODEL_SCHEDULE='apply p=2 stop-after=1' cargo test --test model
DFORM_MODEL_SEEDS=10000 cargo test --test model   # more seeds (DFORM_MODEL_START offsets them)
```

It must catch the executor persisting once per tick instead of after
every Apply call (`executor::hooks`, dform-core's `test-hooks` feature,
which only the root package's dev-dependency turns on; CI checks the
`dform` binary does not carry it).

Golden (snapshot) tests pin `plan` and `strata` output for a table of
example and adversarial programs: `tests/golden.rs`, snapshots under
`tests/golden/<program>/<case>.<plan|strata>.txt`. Accept a change (after
reviewing the diff) with:

```bash
UPDATE_GOLDEN=1 cargo test --test golden -- --test-threads=1
```

See `tests/golden/README.md` for details.

`crates/dform-proptest` holds the property tests, with the mock linked
in (the direct backend). `tests/unknowns.rs` generates stratified
programs from a small grammar of rule shapes over a fixed schema whose
computed paths are fresh, open and secret (references, reads of computed
attributes in comparisons, builtins and `in`, negation over predicates
and wants with stuck instances, aggregates, denies over all of them),
plans each on an empty world, resolves every null to a value of its
class (distinct fresh ids; a random constant for an open one; a secret
stays in the world), plans again through refresh and round 0, and checks
E §4.2: a definite deformation keeps its document up to substitution, a
deny that fires afterwards was reported undetermined, a new resource is an
instance of a reported pending group, and a decided negation does not
flip. A failure prints the shrunk program as source. `PROPTEST_CASES`
sets the case count (1024 by default; the nightly workflow,
`.github/workflows/nightly.yml`, runs 10^4):

```bash
cargo test -p dform-proptest
PROPTEST_CASES=10000 cargo test -p dform-proptest --test unknowns
```

Two of its tests plant a bug in Rule 3 (dform-core's `test-hooks`
feature, which only this crate's dev-dependency turns on) and pass only
if the property catches it within the default count. The regressions
at the end of `tests/unknowns.rs` are shrunk failures it found.

The parser's suite is `tests/syntax.rs`: every `.df` file in the repository
and `tests/syntax/ok/` (E §7's programs among them) parses and prints back
byte for byte, and each `tests/syntax/err/*.df` fails with the diagnostics
pinned in its `.txt` (accept with `UPDATE_GOLDEN=1 cargo test --test syntax`).

## Performance

The evaluator is semi-naive over an operator IR (`crates/dform-core/src/ir/ops.rs`: Scan,
Join, Extern, AntiJoin, Filter with Stuck as its third output, Map,
Distinct, Agg, and a Fix per stratum), with a hash index per relation and
key the rules read through (`crates/dform-core/src/ir/store.rs`). Its output is the naive
loop's, byte for byte, circuit node ids included.

`benches/scale.rs` generates programs from a mock schema of 10^3 types (a
fresh `id` everywhere, an open `endpoint` on every tenth), N resources
(each with a `parent` ref to an earlier one, one in a hundred stuck on a
`format` over an endpoint), a recursive closure over the first thousand,
and a 500-rule policy pack, then plans them with the command line over
the direct backend (the mock linked in), in a child process:

```bash
cargo bench --bench scale                  # 10^3 resources (CI-sized)
cargo bench --bench scale -- --full        # 10^4 and 10^5 resources
cargo bench --bench scale -- --resources 5000 --out /tmp/gen   # keep the programs
cargo bench --bench scale -- --process                         # also `dform` over gRPC
cargo bench --bench scale -- --bin path/to/other/dform         # ... another build of it
```

It reports partition-graph nodes, provenance bytes per fact, stuck
instances per null, the evaluator's read count (index lookups plus tuples
read, deterministic), and per backend the plan's wall time with
/proc/loadavg and instructions (when `perf` is installed; over gRPC the
provider process's included), and on the direct backend the provider's
share: the time spent in the mock's calls.

## Status

This is an MVP:

- semi-naive evaluator with hash indexes (see Performance)
- functions declared in `std/*.df` (docs/grammar.md "Functions"): the prelude's constructors `int`, `string`, `inet`, `ip`, `iprange` and `format`, `len`, `ref`, `scoped`, `cloud_ref`, `declassify`; `inet.subnet`, `inet.host`, `inet.addr`, `inet.contains`, `inet.overlaps`, `inet.prefix_len`, `ip.unspecified`, `str.split`, `str.lower`, `str.upper`, `list.len`, `list.join`; arithmetic `+ - * / %`; aggregates `collect_*`
- list helper predicate: `member(List, Item)` and `member(List, Index, Item)` (Index starts at 0)
- safe(ish) negation: `not` requires the atom be ground at evaluation time

Provider model (in progress): the demo uses the mock provider, `dform __provider fake`, a
separate process behind the plugin protocol (tests and benches may link it in
instead, `dform-direct`) that supplies schema facts
(`providers/<name>/schema.df`) and discovery facts (inventory), and supports plan/apply
against a world file, with chaos injection.
