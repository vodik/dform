# dform reference

Every command and flag, the provider model, state backends, keyed stacks,
approvals and the audit log, as the tool behaves today. The README is the
introduction; `docs/grammar.md` is the language.

`dform` is a Datalog-like language for describing infrastructure intent as:

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
uses the modules beside it (`baseline.df`, `database.df`, ...) by their
paths from the project root.

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
stack per file), modules (every other `.df`, named by its path:
`config.df` is `config`, `modules/net.df` is `modules.net`),
`config/<stack>/<key>.yaml`, `data/`, `providers/<name>/` and a
gitignored `dform.state/`.
The root is the nearest directory up from the working directory holding a
`dform.toml`; `dform init [NAME]` makes one (and puts `dform.state/` in the
nearest `.gitignore`). Every path a program states resolves from the project
root: module paths, document and table sources, `file.text`, provider
sources and trust roots. Outside a project, `plan` and the `dev`
views run on a program file with no state; `apply`, `stack`,
`state` and `log` refuse (a `dev --world` run keeps its state beside the
world file, and runs anywhere). A stack is a file, named after itself:
`stacks/shop.df` is the stack `shop`. With no `stacks/` directory the
root's `.df` files are the stacks, so `dform.toml` beside `shop.df` is a
project with one stack; any file runs by path, named after itself. A
file that is not a stack with a `key` is an error, and so is an
`instance` of a stack. `docs/layout.md` has the convention; every
example under `examples/` follows it, and test-only programs are under
`tests/fixtures/`.
Each example's `README.md` says what it shows and lists the commands to
run from its directory (`dform` there is `cargo run --` in a checkout);
`tests/examples.rs` runs them: plan, apply, and plan again, up to date.

`dform.toml` is small; programs stay in `.df` files. It holds
the project's name and the dform versions it takes, each provider's source
and version (Cargo's semver syntax: `"2.1"` is `^2.1`), each stack's
operational settings, `[stacks.NAME]` for `stacks/NAME.df` over
`[defaults]`, and globs discovery skips. A stack's settings are a closed
list: `backend`, `role`, `approvals`, `audit_sink` and
`isolated`; a term is written as a string, `{stack}` the stack's name
and `{k}` the value of its key `k` (in `backend`), and any other key is
an error naming the list (`config` is gone: a stack's settings document
is `set from` in its file, "Giving inputs"). A
`[stacks.NAME]` no file is is an error. Policy reads it as facts,
`project_provider(Name, Constraint)`, `project_default(Key, Value)` and
`project_stack(Name, Key, Value)`. No inputs and no key values: a
deployment is named by its target.

```toml
[project]
name = "shop"
edition = "2026"                                 # the language edition: required
dform = ">=0.1"

[providers]
aws = { source = "aws-mock", version = "2.1" }   # `use aws` in a program
google = { source = "providers/gcp" }            # a path under the root
k8s = { source = "k8s", timeout = "2m" }         # each call's timeout (60s by default)
ovh = { path = "~/src/dform/target/debug/dform-provider-ovh" }  # the executable itself

[defaults]
backend = 'local("state/{stack}")'   # or 's3("bucket", "dform/{stack}", {...})'
lease_duration = "60s"               # an s3 backend's lease (the default)
lease_renewal = "20s"                # how often its holder renews it (the default)

[stacks.shop]                        # stacks/shop.df
backend = 's3("acme-state", "shop/{env}")'
approvals = 'jwks("https://sso.acme.example/keys")'
isolated = true

[discovery]
exclude = ["scratch/**"]
```

A provider's `path` names its executable, relative to the root, absolute,
or under `~/`, until a registry serves it (R-24); a `[providers]` table
has a `source` or a `path`, not both.

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
default)` (`plan --json`: `deployment` and `key_defaults`). `apply` also takes a plan file (`dform apply plan.json`).

`apply` prints the plan and asks `Apply these N changes to
shop[env=prod]? [y/N]` (`Apply this change to ..` for one); only `y` or
`yes` proceeds. `--yes` (`-y`)
applies without asking, as a script or CI does: with no terminal to ask on
and no `--yes`, apply refuses at once, naming the flag. A plan that is up to date
asks nothing, nor does `apply plan.json` (the file was reviewed; approvals
guard it). The one exception is the plan's `warning` (R-80): a rule the
plan deletes everything of, or a relation it empties, since the last
apply is asked for on its own after that question, also under `--yes`
or of a plan file, and with no terminal apply refuses naming
`--allow-empty RULE`, which (or the stack's `allow_empty`) lets it
through. Nothing is written before the answer: an
apply reads (refresh, the lookups that resolve uncertain calls), plans and
asks, and only a `y` writes state (`moved` renames included) or calls
Apply. A later tick whose plan holds an address no earlier tick listed (a
pending group's member: `iam.policy[?]` at tick 1, named once the endpoint
it is built from exists) asks again: its plan is printed, headed `tick 2
1 change, now that tick 1 reported`, then `Apply tick 2 to D? [y/N]`. A `n` there stops the apply with what
the earlier ticks did in state; the audit log's `apply_end` says
`declined` and the `tick`, and the next apply resumes. An unattended
apply (`--yes`, `apply plan.json`, `--approval`) has nobody to ask: it
applies the ticks whose addresses the plan named and stops before the
first tick that would add one, after writing state: `apply stopped after
tick 1: tick 2 adds 1 change the plan could not name
(iam.policy[?] on ?db.postgres["orders"].endpoint); run apply again to
plan them against the world as it now is`. It exits non-zero, nothing
applied that was not printed, and `apply_end` says `stopped`; the next
apply plans them as its tick 1, by name. There is no strict mode and no
resource-level target: a plan that needs a second tick applies tick by
tick, and a program that wants to apply part of itself is two stacks.

The stack is the unit of partial work. `apply X` in a project applies the
deployments X reads (`use stacks.platform`, then `platform[env="prod"].x`,
a key written out or X's own, `platform[env=env].x`) first,
and theirs before them, each a run of its own with its own plan,
confirmation and state, then X; nothing that reads X. The first line
is the `stacks:` line, the deployments in apply order: `stacks:
platform[env=prod], then shop[env=prod] below, in apply order:
shop[env=prod] reads its outputs; each is planned, confirmed and applied
in turn`, and each run is headed `== NAME` and prints its own plan, its
own ticks counted there (a later stack's plan reads what the earlier
ones applied, so it cannot be counted before they run). A dependency whose apply fails or is declined stops the
run before its reader; stacks that read each other are an error naming
the cycle. A `--set` goes to each stack of the run that declares the
input (one none declares is the target's error); `--input-file` is the
target's. A plan file,
a `--world` fixture and a program outside a project apply only
themselves. `apply` with no target, in a project of several stacks under
the working directory, applies every one of them (each with its default
key) in dependency order, each run headed and confirmed on its own; a
`--set` no stack declares is an error.

| Commands | |
|---|---|
| `plan`, `apply`, `why`, `why-not`, `query`, `diff`, `test`, `fmt`, `log` | on a target |
| `output TARGET [NAME]` | a deployment's outputs |
| `stack list`, `stack rekey`, `stack unlock` | the project's stacks |
| `state show`, `state taint`, `state forget-host`, `state mv` | a deployment's state |
| `provider check`, `provider schema` | providers |
| `dev strata`, `dev graph`, `dev effects`, `dev --world W --inventory I --provider P --chaos C COMMAND` | the mock and the evaluator |
| `doc [TARGET]` | the doc comments as Markdown, on stdout |
| `init [NAME]` | make the working directory a project |
| `completions zsh\|bash\|fish` | a completion script |
| `lsp` | the language server, on stdin and stdout (see "Language server") |

Controller mode (`controller run`, `stack handover`) is experimental:
`DFORM_EXPERIMENTAL=1` lists it in `--help` and the completions, and
docs/experimental/controller.md describes it.

`dform output TARGET` lists a deployment's outputs as of its last apply
(what other stacks read): the scalars as a key/value table, each relation
(`output p`) as its own table headed by its name, its columns its
`decl`'s. `dform output TARGET NAME` prints one for the shell, so it is
always safe to redirect: a string's bytes exactly (no quotes, no
folding, no newline added), another scalar as the program spells it, a
relation's rows tab-separated. `--json` prints either as JSON. A secret
output lists as `secret`; state keeps no bytes of it, so `output TARGET
NAME` of one is an error.

```bash
dform output app env=prod                 # url  "https://..", then each relation's table
curl "$(dform output app env=prod url)"   # the bare value
dform output app env=prod zone            # prod-a<TAB>0, a row per line
```

`dform stack list` is a result set, one row per deployment with state:
its stack (with its key) and file, where its state is when that is a
bucket, its last apply (time, actor and the project's commit, from the
audit log) and a saved plan not yet applied. `dform state show TARGET`
prints the deployment's objects, one row per address with its provider
and remote id, then its outputs as a key/value table (a secret output as
`secret`: state keeps no bytes of it); `--address ADDR` only the one at `ADDR` (it, `state mv`, `log` and `stack unlock`
need the deployment's key, not the program's other inputs); `dform state mv FROM TO TARGET` gives the
object at the address `FROM` the address `TO`; `dform stack unlock TARGET` removes
an apply lock whose holder is gone (breaks an s3 backend's lease). `dform provider schema NAME` prints a
provider's schema facts. `dform doc` prints the project's doc comments
(`#|` lines above an item, docs/grammar.md "Doc comments") as Markdown: per
file, each documented item's kind and name, its first line, its
description and its other keys (`owner`, `since`, `deprecated`, ...);
`dform doc TARGET` only its program's files (the stack's and every file
it imports); either ends with the standard library, each std/*.df
function's signature, summary and example. `dform completions zsh > _dform` completes stack
names, key values (from the key inputs' enum types) and deployments with
state. `dform version` prints dform's version and the release of the time
zone database built into it (`tzdb 2025b`): a `time`'s zone is read from
that, never from the host, so a plan is the same on every machine; and
whether the build has the wasm host (`wasm host in`), the experimental
`--features wasm` build that runs a provider component
(docs/providers.md).

State is scoped to a stack. One program is one stack, named after its
file (`stacks/demo.df` is `demo`), and its operational settings are
`dform.toml`'s `[stacks.demo]`, over `[defaults]`:

```toml
[stacks.demo]
backend = 'local("state/demo")'   # where state, world and lock live, relative to the
                                  # project root; default dform.state/<name>; or
                                  # s3(...), see "State backends"
approvals = 'jwks("https://...")' # optional: who may approve a plan; see "Approvals"
audit_sink = "logger -t dform"    # optional: each audit entry to a command; see "The audit log"
isolated = true                   # a keyed stack's deployments do not share names
wait = "30m"                      # optional: how long a tick waits on open nulls (10m);
                                  # see "Timeouts, retries and waiting"
allow_empty = ["net.subnet"]      # optional: what a plan may empty without the
                                  # guardrail's warning; see "Computed values come
                                  # from Apply"
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

State keys each resource by its type and address, `T::A`, A the
resource's path (R-112: `net.vpc::main.vpc`, a quoted segment as
written, `net.vpc::k3s."a.b"`); the plan file, `--json` and the mock's
world use the same path. State written before R-112, with `/` in its
addresses (`main/vpc`), is pre-release and is not migrated.

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
`stack rekey` (it moves the deployment's objects between prefixes, or
between a bucket and a directory), `state taint`, `state show`, `state mv`, `log`,
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
outside the key are parameters of a deployment: they change it in place.
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
every deployment, and they collide in a shared account: a warning per
provider, listing each name at its field. The value flows through bindings, interpolation,
calls, lookups (`buckets[env]`), module inputs and refs; a read
that only gates the resource's block, or feeds another field, does not
count, and neither does it for `rekey`'s list. `isolated = true` in the
stack's `[stacks.NAME]` says each key value deploys into its own account
(or world), and turns the lint off; `dform.df`'s says so, since its
identity module's names are fixed. A provider whose `use` block
reads the key, or a value that depends on it, or a computed attribute of
a resource (`use k8s { kubeconfig = k3s.kubeconfig }`, read back
from a server each deployment creates) reaches a per-deployment account
already, and its names are not linted (R-117).

`use stacks.app` then `app[env="prod"].k` reads one deployment's outputs
(the core's `stack_output("app[env=prod]", k, V)`).

Cross-stack values: `output k = t` at the top of a program is a stack
output. `apply` records the stack's outputs whose values are known in its
state, publishes them beside it as their own object, `outputs.json`, and
records where the deployment's objects are (an absolute directory, or
`s3://...`) in `dform.state/stacks.json`; every other program reads them
after `use stacks.net`, as `net.vpc_id` (the fact `stack_output("net",
"vpc_id", V)`), from `outputs.json`
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

A project reads the outputs of another project's stacks by mounting that
project as a package in its `dform.toml` (R-65):

```toml
[packages.platform]
path = "../platform"
```

`use platform.stacks.cluster` then `cluster[env="prod"].endpoint` reads
the deployment through the backend that project's own `dform.toml` names
in `[defaults] backend` (`{stack}` the stack's name; without one, its
`dform.state/`), a keyed deployment under its key's segment, and the
object is that deployment's `outputs.json`. A reader needs read access to
that object only. A deployment the package has not applied has no
outputs. The package's other files are modules like this project's:
`use platform.config`.

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
`fakecloud`, and `dform-provider-k8s`, `k8s`) built otherwise:
`rebuild: cargo build --workspace`.

The mock can pretend to be any provider: a provider it plays is a schema file of
plain facts, `providers/<name>/schema.df`, selected by the program's
providers' `use`s (`use gke`, or `use aws { source =
"providers/aws-mock" }` for a directory or `.df` file relative to the
program's file), by the manifest's `[providers]` entry of the name, or,
overriding them, with `dform dev --provider NAME` (repeatable). There is
no default: a program with no provider's `use` starts none, and
`plan`, `apply`, `query`, `why` and `test` refuse it, "the program names no
provider: add `use NAME` (dform.toml names its source) or run under
`dev --provider`". A `use` or `instance` with no entries is written
without braces (`use fake`, `instance network blue`); `fmt` drops a
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
error before anything is planned, naming the resource, the providers' `use`s
and the known schemas (the built-in ones and `providers/*/schema.df`) that do
declare it: `provider fake does not declare google.compute_subnetwork;
declared by: gke`. A provider's types are named under it (`aws.vpc` is
provider aws's), so the error says which `use` to add. A
type the program declares itself with a `type` block is the mock's to
play. A mock playing several providers on one link (`use google`
and `use k8s` on mock schemas) takes no settings from any of them.

A provider's `use` block's settings other than `source` configure the provider,
and read like any rule reads: inputs, value names, tables and `env.var`.
A keyed deployment configures its providers by its key:

```dform
use env

use google {
  project = gcp.project_id                                # an input the env's `set` gives
  credentials = env.var("GOOGLE_CREDENTIALS_${env}")      # a secret, per key
  expect_account = gcp.project_id
}
```

The block lowers to `provider_config("google", { project: .., credentials:
.. })`, which reaches the provider at a second Configure as `settings` once
the evaluation knows it (the provider serves nothing until then; see "The
Kubernetes provider"). `env.var("NAME")` is the built-in `env` provider's
extern (`use env`), answering the
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
and delete. Every provider's Schema is checked to name each type under
the provider its `type_provider` row gives it (`k8s.secret` is provider
`k8s`'s), and one that serves its own schema to name its types under its
handshake's name. The mock passes it:

```bash
cargo run -- provider check crates/dform-mock/schemas/fake.df   # the mock
cargo run -- provider check ./my-provider         # any plugin executable
```

```dform
type_provider(net.vpc, "fakecloud")                         # who owns the type
type_attr(net.vpc, "id", "string", ["computed", "id"])      # Flags: required computed id
type_attr(db.postgres, "endpoint", "string", ["computed"])  #   sensitive nullable optional_computed
type_attr(net.vpc, "cidr", "string", ["force_new"])         #   force_new write_only
type_attr(ovh.instance, "user_data", "string", ["write_only"])  # the API never answers it
type_list_key(k8s.deployment, "spec.template.spec.containers", ["name"])  # list merge keys
type_mint(db.postgres, "endpoint", "{name}.db.fake")        # optional: how the mock mints it
type_retry(db.postgres, 5)                                  # optional: Read attempts (default 3)
type_replace(k8s.deployment, "create_first")                # optional: create_first, destroy_first, either (default)
type_doc(net.vpc, "cidr", "The network's IPv4 range.")      # optional: a path's description ("" the type's)
extern_decl("ovh.image", "+region, -name, -id, -distribution")  # a data source, no `extern` line needed
```

A `write_only` attribute (R-106) is one the API takes and never answers:
state keeps the digest of the value last applied beside the resource
(`written`, keyed with the stack's key, never the value), and Plan
compares the program's value with it: the same is no change, another a
change (a replace when the path is `force_new`); with none kept (an
object made elsewhere) no change. An `extern_decl` declares a data
source to the compiler: a program reads it with no `extern` line.

Built-in mock schemas: `fake` (the demo's), `gke` (pngu.df), `k8s` (fifteen
Kubernetes kinds; try `cargo run -- -C examples/k8s plan`)
and `aws-mock` (twelve AWS types in the Terraform provider's shape, with its
Optional+Computed attributes and keyless sets; try
`cargo run -- -C examples/aws plan`). Each example names its
provider with a `use`.
A `required` attribute the program does not set is a plan error. Lists with
`type_list_key` are diffed by key (`spec.template.spec.containers[name=web].image`),
lists of type `set` as sets. A program writes a keyed list by element too:
`set c.resources.limits = { .. } @default where w in k8s.deployment, c in
w.spec.template.spec.containers` (or `containers[c.name].resources.limits`)
gives every container that does not set its own (docs/grammar.md, `set`),
and two authors' lists merge by key. A `type_mint` string may use `{type}`, `{name}`,
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
use k8s { source = "./providers/k8s" }   # a directory holding dform-provider-k8s
use k8s { source = "bin/dform-provider-k8s" }        # an executable
```

- A program may name the cluster itself, as a managed cluster's kubeconfig
  arrives: `provider_config("k8s", { kubeconfig: K })` with `K` the
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
  plans the same against either; so is `k8s.<kind>` for every other kind
  of the static schema whose kind no other group has (`k8s.storage_class`,
  `k8s.ingress_class`). A cluster's own kinds keep their full names.
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
- A provider configured by the program (`use k8s { kubeconfig = .. }`)
  serves the static schema until its settings are known: the snapshot,
  extended by the kinds the deployment's cluster served beyond it when it
  was last reached (its CRDs, cached at
  `dform.state/cache/schema/<deployment>/k8s.json`, keyed by the derivation
  and the cluster's document, written when the program's settings
  configure the cluster). A kind in neither waits on the provider for its
  schema (`later`) until the settings arrive; from that Configure on the
  provider serves its cluster's kinds, and dform plans such an object
  against it, untyped in dform's own schema until the next run loads the
  cache.
- With no cluster in reach, or `DFORM_K8S_OFFLINE` set, the provider is
  offline: the schema is the checked-in snapshot of Kubernetes v1.36.0's
  document (`crates/dform-k8s/openapi-snapshot.json`, every kind of the
  stable groups: core, apps, batch, autoscaling/v2, policy, networking,
  rbac, storage, scheduling, coordination, discovery, node,
  admissionregistration, apiextensions and certificates, trimmed by
  `crates/dform-k8s/trim_openapi.py`), the provider's static schema; Plan
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
# + net.subnet main.private-us-test-1a        network.df:24
#     vpc = main.vpc
```

A reference is the resource (R-43). Where an attribute points at another
resource, the schema types it `ref(T)` and the program gives it the
resource: `vpc = vpc`, `subnets = [ s | s in net.subnet ]`. The provider
gives its API the object's id once it exists; the plan prints the
resource by its address, `vpc = main`, before it exists and after
(`--json` shows the id). A program never reads an id: `x.id` is an
error naming `x`, and `ref(x)` writes the reference out where an attribute
that is no `ref(T)` needs the id as text.

The plan, `why`, `query`, `diff` and the editor's hints print an address
as the source names it (R-111): its type and its path, a copy's scope in
front, `net.vpc edge.left.vpc`, a value referring to it by the path
alone, `main.vpc`. The full address is the source term that names it,
`T["A"]` (`A` the path, a copy's scope included: `n.x`, `edge.left.vpc`), an
attribute of it `.path` after it: the plan file, `--json`, state, `plan
-q`'s apply order, `state show`, `dev graph` and errors print it. Every
address the command line takes is read that way (`why`, `query`, `state
show`, `state mv`, `dev show`, `--chaos`), and `why` also takes the
printed one, so an address copied from a plan pastes into a command;
quote it for the shell (`why 'net.vpc main.vpc'`, `why main.vpc.cidr`).

The plan is the Z-set `desired - world` (proposal E §2.8): per address a
create, a delete, an update, or nothing. It is printed grouped by tick
(R-79), the only grouping, each change with where it is derived:

```
$ dform plan apps env=prod
plan: 6 changes (3 create, 1 update, 1 replace, 1 delete) over 2 ticks, 1 approval, 1 undetermined

tick 1  4 changes, applies now
  + k8s.namespace apps                         stacks/apps.df:26
  + k8s.secret synapse.homeserver              synapse.df:41
  ~ k8s.deployment synapse.server              synapse.df:52
      spec.replicas: 1 → 2                     stacks/apps.df:14
  - k8s.config_map synapse.legacy              synapse.df:60
      data.mode was "legacy"
      because data/apps.yaml no longer has the row app("legacy")

tick 2  2 changes, after tick 1 reports
  waits on  synapse.web.ip
  + ovh.domain_record "matrix.vodik.xyz"       synapse.df:135
      target = synapse.web.ip
  ± k8s.persistent_volume_claim synapse.media  synapse.df:70  storageClassName is immutable

later   changes this plan cannot count yet
  k8s.job "migrate-v${schema}"                 one per release("crud_api", "schema", _)
  deny "prod keeps its data"                   stacks/apps.df:40  undetermined until tick 2

held for approval
  k8s.persistent_volume_claim synapse.media    replace of a volume in prod    baseline.df:38

apply: tick 1 once this plan's digest is approved (`--approval`), then tick 2 when tick 1 reports; `later` is planned again when tick 1 reports, and apply asks before what it adds
```

- The summary counts the changes the ticks hold, by kind, the ticks, and
  then the denies, approvals, undetermined policies and conflicts.
- `moved T["Old"] -> T["New"]` lines come first, one per `moved/3` rename
  of state.
- Every other line is one of two shapes (R-111). A change: its mark, then
  its address as the source names it, the type and the path, a copy's
  scope in front (`+ ovh.ssh_key k3s.admin`, `+ net.vpc blue.vpc`, a
  name holding a dot one quoted segment, `k3s."k8s-lab.vodik.xyz"`),
  then where it is derived, `FILE:LINE`. An attribute: `path = value`
  (`path: before → after` in an update), then `FILE:LINE` only when the
  value was written outside the change's own block: a policy, a `set`, a
  `--set` (its flag), an instance's input, a config module's `let`,
  followed through the inputs and `let`s that pass it on. A value written
  in its own block says nothing more. A reference is the address it
  names, `ssh_key = k3s.admin`, no `?`; a value another resource
  computes is the reference that reads it, `target = k3s.server.public_ip`
  (within a tick the executor applies in dependency order, so it is known
  when it is read; a value no change of the tick makes puts its change in
  a later tick, whose header says what it `waits on`). A secret is
  `(sensitive)`. A string past 60 characters elides its middle (`"ssh-
  ed25519 AAAA…2DK7 simon@framework"`). The full address, `T["A"]`, is
  the plan file's, `--json`'s and state's; `why`, `query` and `why-not`
  take it or the printed one (`why 'ovh.ssh_key k3s.admin'`, or its path
  alone, `why k3s.admin`, `why k3s.server.public_ip`).
- `tick N  K changes, applies now`: what this apply makes first. A change
  is `+` create, `~` update, `-` delete, `>` adopt, `±` replace (`(the new
  one first)` for a `create_before_destroy` one, whose deposed object is
  `- T a  (deposed)` in the next tick). An update diffs a keyless set,
  or a list with merge keys (`containers[name=web]`), by element: an
  element that is new or gone is one `+`/`-` line with its leaves, not
  every later index shifting. Map leaves print one per line. A copy
  (R-67) prints as its own entry, `+ network blue`, in bold, its resources
  indented under it with their full paths (`+ net.vpc blue.vpc`), a copy
  inside it nested again, inside the tick they run in.
- `tick N  K changes, after tick N-1 reports`: changes held until values
  a tick before makes are known, `waits on` each value (an output of a
  resource tick N-1 makes, a field of the world). Their diffs are shown
  now. A later tick of a running apply says `now that tick N-1
  reported`.
- `later   changes this plan cannot count yet`: a resource rule stuck on
  an unknown, by the address its statement names
  (`k8s.job "migrate-v${schema}"`), `one per ROW` when what it reads may
  gain rows, `if ROW derives` when one may, else what it `waits on`,
  never a count; a copy that may derive once, its resources under it; a
  deny or check `undetermined until tick N` (never reported as satisfied)
  or that `may hold at tick N`; a held change waiting on what this plan
  does not resolve. Every resource of a provider whose settings the
  program gives and this plan does not know (a kubeconfig read from a
  server still booting) is one, under `waits on  provider k8s (kubeconfig
  from k3s.kubeconfig)` and the dim note `which this plan does not
  resolve`, typed by the provider's static schema; one of a kind no
  schema has yet (a cluster's CRD) under `waits on  provider k8s for its
  schema`, its attributes as written. The summary counts them, `, N
  later`, and `why-not` names what such a resource waits on. A type whose
  namespace names no provider is the compile error it always was.
- `warning  this plan empties what the last apply derived` (R-80): a
  rule the plan deletes every resource of that it derived at the last
  apply, by its `FILE:LINE` and statement, with what it deletes (`deletes
  all 2 it derived at the last apply: ..`, three named and `and N more`)
  and the leaf that changed since (R-79's `because`, when there is one);
  and a relation of the program that had rows at the last apply and has
  none now (`active  had 1 row at the last apply, has none now`). A rule
  is one that binds variables (a `where` with a join, an `in`); a
  resource stated once is not. A broken join and a deliberate delete look
  alike to the plan, so it says so, and `apply` asks for each on its own.
  Under `--yes`, or of a plan file, `apply` still asks for each, on a
  terminal (`The plan deletes all 2 resources the rule at
  stacks/net.df:13 derived at the last apply. Apply it anyway? [y/N]`),
  and with none to ask on it refuses before changing anything, naming
  the flag: `apply --allow-empty RULE` (repeatable) or `[stacks.NAME]
  allow_empty` in dform.toml names those that may empty without a word: a rule's `FILE:LINE`, a resource type it derives, or
  the relation. What each apply derived is the audit log's `derived`
  entry; a plan with no apply before it warns of nothing. `--json`
  carries them as `warnings` (each `{rule, statement, relation, deletes,
  rows_at_last_apply, because}`), only when there is one.
- `denied`: denies over the plan itself (`lifecycle prevent_destroy: the
  plan would replace T["A"]`, the message as the rule wrote it), each
  with the change its firing read (`net.vpc main`) and where it is
  written; the plan still prints, then refuses.
- `held for approval`: each change a `requires_approval` row holds, its
  reason and where the row is derived; `plan digest: sha256:...` follows
  the plan.
- `shadowed`: contributions at a losing rank that disagree (a warning),
  and `conflicts`: cells whose contributions disagree at the winning rank,
  each naming the resource, the path and every witness (`! net.vpc
  main.cidr: two contributions disagree`), each once. A conflicted
  address is not a change; the plan still prints, then refuses.
- `(drift: ...)` marks an update where a fresh null meets a value the
  world already has: the identity mapping is stale.
- The last line says what `apply` does with this plan (R-12): which tick
  now, which after a report, and that `later` is planned again once the
  tick it waits on reports.
- `stack NAME is up to date`: nothing to do, nothing stuck (the only line).

How much each change says of why it is planned is a ladder (R-79,
R-111), the same on `plan`, `apply` and `diff --since`: `-q`, the
default, `-v`, `-vv`, or by name `--why=none|line|how|full` (`--why`
alone is `full`). `-q` and `-v` together, or either with `--why`, are a
usage error.

- The default (`line`): the two shapes above. On a change's line, where
  it is derived, `FILE:LINE` (a delete's where the last apply derived
  it), and for a replace the paths the schema declares immutable. On an
  attribute's, where its value was written when that is outside its own
  block. Under the change, a `because` line: the leaf of the derivation
  the last apply recorded that is false now (a delete: `because
  data/azs.yaml no longer has the row az("us-east-1b", 2)`; an update:
  `because input size is now 2 (was 1)`; a guard: `because input big is
  now false (was true)`), or for a create the leaf new since (`because
  data/azs.yaml:7 gained the row az("us-east-1c", 3)`). The last apply's
  program is evaluated as `diff --since` reads it: at the commit its
  audit entry recorded, or the program now with the inputs it recorded
  when only they changed; with no apply, or nothing to compare, there is
  no `because`. Sites are relative to the project's root, whatever
  directory the run is in. No other prose: no expression, no binding,
  no rank.
- `-v` (`how`): the same lines, saying how. On a change's line, the
  statement's variables bound (`network.df:24  with z = "us-test-1a"`).
  On an attribute's, the write that won: a create's own entry by its
  expression when it reads something (`inet.subnet(main.cidr, 8, n)`,
  `db.name`); anything else by the statement that wrote it with its
  place (`set db.backup_days = 14 where env == "prod"
  stacks/shop.df:22`), a `--set` by its flag; and the write it won over,
  by its place and both ranks (`--set size=2  @override over
  stacks/net.df:4 @default`), unless that write is in the change's own
  block. A secret by its label (`(sensitive random.password("db"))`), a
  long string whole.
- `-vv` (`full`): `-v`, and under each change its derivation, `why`'s
  tree compressed to one line per leaf. `by FILE:LINE` is the statement
  that derived it; each `because` line is a leaf, the facts in between
  dropped: a fact the program states at its `file:line`, a table's row at
  its `path:line`, a `--set` or `--data` as its flag, an extern's answer,
  a world fact, a fact found absent. A create is explained by its
  `want`, an update (a drift, a replace) by the winning contributions to
  each attribute it changes, a delete by state alone (`because no
  statement derives it now; state has it`). Of several derivations the
  one with the fewest leaves is shown.
- `-q` (`none`): the bare diff for scripts, laid out as the plan was
  before it was grouped by tick (`definite:`, `pending on ?NULLS
  (resolves after tick N):`, `pending groups:`, `undetermined:`,
  `denied:`, `apply order:`, `needs approval:`, `->` between a value's
  sides), addresses in full (`+ net.vpc["main"]`); only its words are the
  tool's (`plan: 3 changes (2 create, 1 update), 4 pending`, `stack NAME
  is up to date`). `tests/golden/*/*.plan-bare.txt` pins it byte for
  byte.

`diff --since` takes the same ladder: `-q` the changes alone, by their
full addresses; the default and `-v` each change with the first line of
why it was planned (where it was derived, or what changed); `-vv` every
line.

The page is 100 columns wide: a right column that does not fit says less
(the statement, then its entry, then `FILE:LINE` alone), and goes when
not even that fits; a `because` line is always its own.

Colour is a hint, never the only carrier: every colour has a character
beside it, and `NO_COLOR` or a pipe loses nothing. `--color
auto|always|never` (global; `auto`, the default, colours when stdout is a
terminal and `NO_COLOR` is unset; errors on stderr likewise) paints the
plan in the eight basic colours: `+`, `~`, `±`, `-` with the address
bold in green, yellow, magenta and red; the site column, `later`'s notes
and `(sensitive)` dim; `because` cyan; `held for approval` magenta,
`denied` and conflicts red, `warning` and a rule in `later` yellow; tick
headers and a copy's header bold. `--json` and the plan file are never
coloured.

`plan --json` prints the same report as one JSON document, the thing CI and
editors consume: `stack`, `up_to_date`, a `summary` of counts (`changes`,
each kind, `ticks`, `approvals`, `undetermined`, `conflicts`), `ticks`
(each `{tick, after, waits_on, changes, deposed}`), `later` (each with a
`kind`: `group` with its `address`, `reads`, `instance`; `deny`,
`refinement` with its `status`; `held` with its `changes`), `shadowed`,
`conflicts`, `moved`, `denied` (`{text, message, address, site}`),
`held_for_approval` and `apply`, the last line. A change is `{kind,
address, type, name, changes}` (`address` in full, `T["A"]`, as the
plan file has it), a replace with `create_first` and `immutable`, a
deposed delete with `deposed: true`, a held one with `held`; from `line`
it has its `site` (`{at, statement, entry, with, origin, rank, beat,
beat_at}`) and
`because`, and at `full` a `why` array of `{kind, at, text}` (`kind` is
`rule` for the `by` line, else `fact`, `input`, `extern`, `world`, `plan`,
`absent` or `state`; `at` the `file:line` when there is one). An attribute
change is `{op, path, before, after}` with its `site` (`op` is `set`, or
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
reason printed. A tick with nothing definite to apply, held on values the
world has not reached yet, waits for them (`--wait`, see "Timeouts, retries
and waiting"). `--max-ticks N` (default 8) is a safety valve for a loop
that never settles, not a way to stop early:

```bash
cargo run -- -C examples/gke apply                  # asks again at tick 2
cargo run -- -C examples/gke apply --set zones=1   # one zone: stops after tick 1
```

A provider whose settings the program computes from what a tick makes
(`use k8s { kubeconfig = k3s.kubeconfig }`, the kubeconfig read over
SSH from the server tick 1 creates) is configured at the boundary where
they become known, waiting for them as for any value (`--wait`) when the
read answers "not yet". The plan lists its resources under `later`
(`waits on  provider k8s (kubeconfig from k3s.kubeconfig)`); apply makes
tick 1, configures the provider, says so, each setting a secret reaches
as `(sensitive)` and `-v` adding what it is written as, never a value:

```
provider k8s: configured after tick 1: kubeconfig = (sensitive)
```

then plans what `later` held against it (a cluster's CRDs among them),
prints that tick and asks before it as it asked before tick 1; `--yes`
applies it. A plan file or an approval did not see that diff, so applying
one stops before the tick, saying it plans changes `later` held for a
provider's settings, which the approved plan did not show; the next apply
plans them as its tick 1. The settings go to the provider in its
Configure call only: they are not in state, the plan file, the audit log
(a `configure` entry names the provider, the tick and the settings' keys)
or any output. Settings that no wait brings stop the apply at that tick,
`nothing definite to apply, still waiting on .. provider k8s (kubeconfig
from k3s.kubeconfig)`.

At a boundary apply also compares the refreshed world with what it last saw
(the tick's refresh and its Apply responses). A change under an address whose
deformation is pending for this boundary stops the run before the next tick,
with the change printed (`the world changed under a pending change after
tick N:`) and the deny that stops it (see "Policy over the plan" below); a
change anywhere else is reported as `drift after tick N:` and the
run goes on, the next tick deforming it back:

```bash
cargo run -- -C examples/gke dev \
  --chaos 'mutate=google.container_cluster["pngu"].deletion_protection=false' apply gke_two_phase   # drift, tick 2 undoes it
```

Deletes and replacement. Deletes run after every create and update, in
reverse dependency order (a delete has no desired document left, so state
records each object's dependencies when it is applied). Which way a
replacement goes is the schema's `type_replace(T, Order)`: `destroy_first`
(`-/+`), `create_first` (`+/-`), or `either` (the default), where it is
`-/+` unless `lifecycle(r, "create_before_destroy")` says `+/-`. That fact
on a `destroy_first` type is an error naming the type; on a `create_first`
type it is redundant. In the mocks a Kubernetes Deployment or Service and an
`aws.instance` are `create_first`, a Namespace and an `aws.s3_bucket`
`destroy_first`, the fake `net.vpc` `either`. A `-/+` replace deletes the old
object, then creates the new one under the same name. A `+/-` one: the new object is
created first under a free name (`main-2`), the old one is *deposed* (kept in
state's `deposed` section), and a boundary follows; the next tick moves what
depends on it to the replacement and then deletes the deposed object
(`- T["A"]  (deposed)`). A deposed object left by a failed apply is deleted by
the next one, once nothing that depends on it is still pending.

Either way the replacement is a new object, so every null that named the old
one (its id, its other computed values) is unresolved again: an existing
object that reads one is `pending on ?T["A"] (resolves after tick N)` and is
updated to the new value the tick after the replacement; a new object that
reads one is created after it in the same tick. `dev --chaos fresh-ids` makes
the mock mint a new id on every create, so the difference shows:

```bash
cargo run -- -C examples/demo dev --chaos fresh-ids apply dform env=staging
cargo run -- -C examples/demo dev --chaos fresh-ids apply dform env=prod   # tick 1 replaces vpcs and subnets; tick 2 updates their readers
```

Lifecycle is plain facts the planner reads (and policy can read too). Each
takes the resource as a reference: a resource in scope by its name, one of
a copy's or one no longer in the program by its address, and many
at once by a rule that binds them with `in`:

```dform
lifecycle(main, "prevent_destroy")                   # a delete or replace of it is a deny
lifecycle(main, "create_before_destroy")             # replace creates first (type_replace either)
moved(net.vpc, "main.vpc", net.vpc["core.vpc"])  # rename without destroy
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
against, `absent` for none) and `world_digest(r, Now)`; and
`derived_at_last_apply(rule, n)` (R-80), from the last apply's `derived`
entry in the audit log: a rule that derived resources then, by its
`FILE:LINE`, with how many, and a relation of the program, by its name,
with its rows. The lifecycle
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
cargo run -- -C examples/demo dev --chaos 'latency=net.vpc["main.vpc"]:100' \
  --chaos 'latency=net.vpc["peer.vpc"]:100' apply dform env=staging --parallel 4   # the two vpcs overlap: 100ms, not 200ms
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
extern answers the plan read (a git table's or document's commit among
them); the names the program declares more than once, each under a
clause, `"guarded": [{"name": "db", "declarations": 2}]` (also in `plan
--json`; docs/grammar.md "Guarded declarations"); and the plan's
digest, with what needs an approval (see "Approvals"). `apply PLAN.json` takes its inputs from the
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
cargo run -- dev --chaos 'mutate=google.container_cluster["pngu"].name="other"' apply plan.json
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

A plan with rows prints a `held for approval` section, each change with
its reason and where the row is derived, and the plan's digest, `plan digest: sha256:...` (a plan that
writes a file says its digest on stderr, `plan file: PLAN (plan digest:
sha256:...)`); `plan --json` has them as
`needs_approval` and `digest`. The digest is sha256 over the canonical JSON
(sorted keys, no whitespace) of the plan file without its `digest` field:
the delta, the inputs and the extern answers (a git source's commit among
them, which `apply PLAN` reads again), with every secret already the stack's HMAC of it. The plan file
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
  commits, the documents the tables read (each path with its digest), the
  rows that need an approval, and who (`plan --out`, and every apply of the
  plan it applies);
- `approval`: the verified statement and the token, or `not required`, or
  why it was refused;
- `apply_start`: who, dform's version, the git commit, the providers and
  the protocol version; from a dirty tree also `dirty: true` and the
  tracked files it had `modified`;
- `action`: the kind, the address, the result (and the error), the remote
  id, and a digest of the redacted diff;
- `tick`: the world as the executor saw it, as an HMAC with the stack's key;
- `retry`: a provider call sent again (R-81): the tick, the provider, the
  call, the attempt and its budget (`of`), the delay, and why the last
  attempt failed (redacted);
- `wait`: a tick that waited on open nulls: the tick, what it waited on,
  since when, how long, and whether they `resolved` or the budget `expired`;
- `configure`: a provider configured from the program's settings at a
  tick's boundary (R-45): the tick, the provider, the settings' keys
  (never their values);
- `derived`: at the end of an apply that completes, what it derived
  (`record`), when it derived any: each rule that binds variables by its
  `FILE:LINE`, its statement and the resources it derived, and each
  relation of the program with its rows; the next plan's guardrail and
  its `derived_at_last_apply` read the last apply's (R-80);
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
planned, as much as the ladder asks (`-q` none, the default and `-v` the
first line, `-vv` every line, as `plan -vv` prints it; see "plan"), and
then the inputs and the stated
rows (a table's, the program's facts) that differ between the apply
before REF and now. The log holds no value, so the explanations are
computed again: each apply by the program as it was then, read at the
commit its `apply_start` recorded (the project copied out of git and
evaluated with the inputs its `plan` entry recorded; a secret one is
recorded only as a digest, so it is left out), or by the program now
when it is the program then by the digests the `plan` entry recorded.
Outside a repository the program now explains every apply, and a `note:`
says where it changed since: a program file, or a document the plan entry
recorded. An apply from a dirty tree is explained at its commit with a
`note:` naming the files it had modified. An update is explained by the attributes
whose values differ from the apply before's, a delete by what derived it
at the apply before. `--json` prints one document: `applies`, each with
its `changes` and their `why`, and `changed` (`added` and `removed`
rows, `inputs` with `before` and `after`). Secrets print as their label.

```
$ dform diff --since 9 -vv net
apply 11 2026-10-02T15:34:17Z by simon@host at 7e11a8c78679: ok
+ net.subnet private-us-test-1c
  by stacks/net.df:9  resource net.subnet "private-${z}" { .. } where zone(z, n)
  because stacks/net.df:7  net.vpc main.cidr = "10.0.0.0/16"
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
the final fact store and prints a result set, one column per variable (a
SELECT of the goal's variables):

```bash
cargo run -- -C examples/demo query 'attr(net.vpc, n, "cidr", c)' dform env=prod
# N           C
# "main.vpc"  10.20.0.0/16
# "peer.vpc"  10.21.0.0/16
cargo run -- -C examples/demo query 'attr(t, a, "cidr", c), want(t, a), t != net.subnet'
cargo run -- -C examples/demo query 'want(net.vpc, "main.vpc")'    # yes / no
cargo run -- -C examples/demo query want                                    # every want fact
cargo run -- -C examples/demo query 'net.vpc["main.vpc"]'          # its attributes: path, value
cargo run -- -C examples/demo query 'net.vpc["main.vpc"].cidr'     # one attribute's value
```

A bare predicate's columns are its `decl`'s fields, a core relation's own
(`want(type, address)`, `attr(type, address, path, value)`,
`deformation(kind, resource, before)`, ..), else `a`, `b`, ...

Every result set in dform (`query`, `output`, `test`, `stack list`,
`state show`, `dev strata`, `dev effects`) prints the same way, as a terse
SQL client would: a header line, aligned rows with two-space gutters and
no borders, `(N rows)` under a table past five rows. A string longer than
a screen is folded in its cell, its first line and `.. (5.1 KB, 112
lines)`. `--json` prints the rows as an array of objects keyed by column,
values spelled as in `plan --json`.

A secret prints as its size, `secret(32 B)` (in `--json`, as its label
`{"sensitive": "T[\"A\"].p"}`): a value at a `sensitive` path, and any
value equal to it or string containing it, so a rule that forwards a
secret does not leak it either.

Rows print values as the program writes them: a reference is the address
it names as the plan prints it (R-111), `google.sql_database_instance
db.name`, an unknown the attribute it stands for after a `?`,
`?k8s.service web.spec.clusterIP`.

`dform why PATTERN` prints how a fact was derived, from the provenance
circuit every evaluation records (proposal E §3, DR-10), in the program's
own terms: each statement that fired, as written, at its `file:line` (a
block shows the entry that fired, the rest elided as `..`; a used
module's statement names its `use`, a copy's its `instance`), then `with` the statement's
variables as they were bound, by their names in the source, and under them
each computed term of the statement with its value: an interpolation, a
function call, a read (`database.backup_days = 14`), a lookup
(`zone_index[z] = 1`), an unknown as its `?` label. Under
that are the facts the firing read, recursively, each spelled as the
plan prints it (R-111): `net.vpc main` for a resource, a copy's
`net.vpc blue.vpc`, `net.vpc main.cidr = 10.0.0.0/16` for an attribute, `input env = "prod"` and `let n = 3`
for a cell, a relation as `zone("us-test-1a", 1)` (its facts after its
signature, `decl zone(name: string, index: int)`, the columns as declared
or inferred, when any has a type). An attribute or an
input is `merged from N contributions`, each with its value, its rank when
it is not normal (`@default`) and the statement that made it: an input's
default, each `set` that gives it (`stacks/tour.df:139  set {
database.backup_days = 14 .. } where env == "prod"`, a
document's leaf by its `file:line`), `--set`. A fact given to the run says
where it came from: its `file:line`, `--set env=prod`, the provider
schema, the world, the plan for the facts the planner hands to the policy
pass. Variables are allowed and every match is printed. A fact derived
more than one way shows its first derivation and `... N more
alternatives`; `--all` shows them all. `--core` prints the same tree in
the core's spelling: the lowered rules by id (`by r17: head :- body`),
their variables, facts as relations, the aggregate as `Σattr`. An address
is a pattern too, as plan prints it or in full: `why 'net.vpc main.vpc'`,
its path alone `why main.vpc`, or `why 'net.vpc["main.vpc"]'` explains
the resource's `want`; `why main.vpc.cidr` (the longest prefix of the
path that names a resource is the resource, the rest its attribute) or
`why 'T["A"].path'` the attribute's `attr`. An input or a `let`
is named as the stack reads it: `why replicas`, `why nodes.count` (a leaf
of an object input, the contributions that give it), `why
traefik.acme_email` (a used module's). An `attr`/`arg`
pattern may name part of an object attribute, by dotted path or by object
value, and then shows only the contributions that hold it:

```bash
cargo run -- -C examples/demo why 'attr(net.vpc, "main.vpc", "tags.team", "platform")' dform env=prod
# net.vpc main.vpc.tags = {component: "network", env: "prod", team: "platform"}
#   merged from 2 contributions
#   ├─ {team: "platform"}
#   │    baseline.df:10  set r.tags = { team: "platform" } where r in resource   (use baseline)
#   │    with r = net.vpc main.vpc
#   │    └─ net.vpc main.vpc
#   │         network.df:15  resource net.vpc vpc { .. }   (instance network.vpc main)
#   ...
#   └─ ... 1 other contribution (--all)
cargo run -- -C examples/tour why orders.backup_days tour env=prod
# db.postgres orders.backup_days = 14
#   merged from 2 contributions
#   ├─ type_refine("db.postgres", "backup_days", "range(1, 35)")   provider schema
#   └─ 14
#        stacks/tour.df:145  resource db.postgres orders { .. backup_days = database.backup_days .. }
#        with database.backup_days = 14
#        ├─ input database = {backup_days: 14, multi_az: true}
#        │    merged from 4 contributions
#        │    ├─ {backup_days: 1} @default   stacks/tour.df:27
#        │    ├─ {multi_az: false} @default   stacks/tour.df:27
#        │    ├─ {backup_days: 14}
#        │    │    stacks/tour.df:139  set { database.backup_days = 14 .. } where env == "prod"
#   ...
```

`dform why-not PATTERN` (R-80) explains an absence: a resource address
(`T["A"]`), an attribute (`T["A"].path`, an attribute of a resource not
derived explains the resource) or a relation's row with constants
(`zone("us-east-1c", n)`). It finds the rules whose head could produce
it, by type and by the name's shape: an interpolated name is read
backwards (`"private-${z}"` against `"private-us-east-1c"` binds `z`), a
copy's scope is stripped, an attribute is matched by its path or the
attribute it is under. With what the address fixes bound, each such
rule's body is evaluated left to right against the final fact store,
and for each rule `why-not` prints its statement at `file:line` and the
first condition no row satisfies, with the bindings substituted:
a relation's literal says `no row` and `nearest:` up to three rows of the
same relation that differ in the fewest columns the literal fixes (the
columns the source states must match and are left out; with none that
do, the nearest rows whole; an attribute by its value there); a
comparison says `false` with the values it compared; a negation the row
that exists; a row a rule of the program derives (a copy's guard,
`instance network.vpc peer: not made`, a relation of its own) is
followed one level in, up to three, with the rule that did not derive it.
What no rule mentions gets one line and nothing invented:
`no rule derives aws.subnet["x"]: no resource aws.subnet is named like
it`; what is derived says so and points at `why`.

```bash
cargo run -- -C examples/demo why-not 'net.vpc["peer.vpc"]' dform env=dev
# net.vpc["peer.vpc"]: no rule derives it
#   network.df:19  resource net.vpc vpc { .. }   (instance network.vpc peer)
#     instance network.vpc peer: not made
#       stacks/dform.df:54  instance network.vpc peer { .. } where env != "dev"
#         env != "dev": false, with env = "dev"
```

`dform dev graph` prints Graphviz DOT, nodes and edges sorted:

```bash
cargo run -- -C examples/demo dev graph | dot -Tsvg > resources.svg   # resource DAG: A -> B when A reads B (a ref, a null)
cargo run -- -C examples/demo dev graph --strata                     # partition graph, a cluster per stratum, negative edges dashed
cargo run -- -C examples/demo dev graph --relation vpc_peer/2        # any binary relation of the fact store
```

`dform dev effects` prints a result set of `scope  effect  what`: per
scope (the stack, each copy of a component, each module used), what it
reads
(inputs by name, world types, externs by name, another copy's outputs),
writes (cells as `(type, path)` partitions, `*` for a variable type or
path, the input cells its `set`s give, another copy's input cells)
and offers (its declared outputs, with their types); and for the stack,
the providers it uses under a clause, `uses  provider aws when cloud
== "aws"`, a row per combination of the enum inputs the clause reads
(the clause as written when it reads more). Read off the
lowered program's rule heads and bodies and the partition graph; no
evaluation.

```bash
cargo run -- -C examples/demo dev effects                  # every scope, a row per effect
cargo run -- -C examples/demo dev effects --json           # the rows, an array of objects
```

## Timeouts, retries and waiting

Every call to a provider has a timeout: 60s, or the provider's
`[providers.NAME] timeout` in dform.toml (`500ms`, `30s`, `2m`). A call with
no answer by then is taken as one that may have taken effect, as a
`DEADLINE_EXCEEDED` from the provider is: `apply T["N"]: the provider P did not
answer the Apply T["N"] call within 2m (its timeout); the call may have taken
effect`. Its answer, if it comes later, is dropped. The calls dform makes to
that provider after it wait behind it, and each one's timeout starts again
when that late answer arrives.

A call that failed in a way worth trying again is sent again after a backoff:
the provider's `[providers.NAME] backoff` (1s by default), doubled each time up
to 30s, jittered to between half of it and all of it so that callers backing
off together do not retry together. At most `retries` times (5 by default);
then the run stops with the last error, the resource named: `apply
T["N"]: ... (gave up after 5 retries)`. What is worth trying again is read from
what the protocol already returns: a refusal (nothing changed) whose message is
or has a clause `retryable: ...` (how a provider marks one), carries an HTTP
status of 429 or 5xx (`(503)`, `HTTP 503`, `status 503`), or is the transport
failing (`status: Unavailable`). A timeout is retried for a call that changes
nothing (a Read, a Plan, a Query); a refusal of any other kind, and a provider
that crashed, never. Each retry is a line on stderr, `retrying the Apply
T["N"] call in 1.2s (retry 2 of 5): ERROR`, and a `retry` entry in the audit
log.

An Apply that timed out may have taken effect, so it is looked up before it is
sent again, and never sent twice blind. A Create is looked up by the
idempotency key it carried (the provider's `provider.created` answer, which a
provider with the `managed` capability gives): the object it made is adopted,
`apply T["N"]: the Create that timed out made ID; it is adopted, not made
again`; nothing found, it is sent again with the same key. A Delete is looked
up by a Read: gone, it took effect. An Update sends the same document again.
Where the provider cannot say what a key made, and for a Replace, the call is
not sent again: the apply stops, the call recorded as uncertain in state, and
the next apply resolves it before it plans (see `executor::resolve_uncertain`:
the same lookup, or the same key again).

A tick waits. When it has nothing definite to apply and what it is held on is
a value waiting can bring, it looks again until that changes: a computed value
of an object that exists and that the world has not reached yet (a Job's
`status.succeeded`, a cluster's endpoint), or an extern that answered "not
yet" (an open null in an output column, where a refusal is an error: a host
that does not answer yet). It refreshes and evaluates again every second,
backing off to every 10s, and says so on stderr every 10s:

```
waiting on k8s.job["migrate-v42"].status.succeeded since 02:14 (3m)
```

Once one changes the run goes on, the wait counted as a boundary: the next
tick is planned as at any boundary (an unattended apply stops before a tick
that adds what its plan could not name). The budget is `apply --wait 30m`, else
the stack's `[stacks.NAME] wait`, else 10m; `--wait 0s` does not wait. Past
it the apply stops, the state consistent and nothing of the tick in flight:
`apply stopped at tick 2: waited 10m (--wait) on k8s.job["migrate-v42"].
status.succeeded, still unknown; state is consistent: run apply again to wait
again`. A null waiting cannot bring (another stack's output not published yet,
a value of an object no tick makes) stops the tick at once, as `nothing
definite to apply, still waiting on ...`. Every wait is a `wait` entry in the
audit log.

```bash
cargo run -- -C examples/demo apply dform env=staging --wait 30m   # each tick waits up to 30m
```

```toml
[providers]
aws = { source = "aws", timeout = "2m", retries = 8, backoff = "500ms" }
```

A provider's table also grants it what it may use beyond the host's own
interfaces and the credentials it may open by name (R-13b;
docs/providers.md, "Grants and credentials"):

```toml
[providers.k8s]
source = "providers/k8s"
allow = ["wasi:sockets"]          # wasi:filesystem, wasi:http, wasi:sockets
credentials = ["kubeconfig:prod"] # KIND:NAME, applied by the host, never sent to the provider
```

## Chaos: failure and latency injection

`dform dev --chaos SPEC apply` (repeatable) makes the fake provider misbehave, the way a
real cloud does. Deterministic: nothing is random and nothing sleeps but `delay`. The world
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
| `delay=T["N"]:MS` | the first Apply of `T["N"]` in a run takes effect, then answers `MS` late, really slept: past a shorter `timeout` it times out (the one knob that sleeps) |
| `flaky=T["N"]:K` | the first `K` Apply calls of `T["N"]` in a run are refused as busy, `(503)`, changing nothing: retried with backoff |
| `not-ready=T["N"].PATH:K` | `T["N"]`'s computed `PATH` is absent from its first `K` Reads after its Create, as a status not reached yet: an open null a tick waits on |
| `not-yet=PRED:K` | the first `K` Query calls of the extern `PRED` in a run answer "not yet": an open null in every output column |

```bash
cargo run -- -C examples/demo dev --chaos 'fail=net.subnet["main.private-us-test-1a"]' apply dform env=staging
cargo run -- -C examples/demo dev --chaos 'mutate=net.vpc["main.vpc"].cidr="10.9.0.0/16"' apply dform env=staging
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
spelling per construct, `proposals/H-small-surface.org`). The edition is the
project's, `[project] edition = "2026"` in dform.toml, never a file's (a
program in no project is read in it); the first token decides what a statement is,
and a newline ends it. Case decides nothing: names are resolved. A constant
is quoted (`"prod"`), a path that is data too (`"tags.team"`), a variable
is a lowercase name bound where it is written, an input or a `let` is read
by its name (`env == "prod"`), and a resource in scope by its name
(`vpc.cidr`); anywhere else a resource is its address, `T["A"]`
(`net.vpc[b]`, `db.postgres["database.main.db"]`), the spelling `plan`
prints and every command takes. `.` is static and `[ ]` a key computed at
run time. A dot is a reference where it is a whole value (a field:
`endpoint = db.endpoint`) and a read everywhere else; the resource alone,
`vpc = vpc`, is the reference. `-` and `/` are operators,
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
    `max(x)`, `any(x)`, `all(x)` aggregate in a head, per group of the
    head's other arguments. `sum` folds ints, `min`/`max` ints or strings,
    `any`/`all` bools; a group with a value of another kind derives a deny,
    and one whose value is a null is stuck.
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
  - `x in net.vpc` ranges over the wanted resources of a type (with `x`
    a plan row's reference, `deformation(k, x, _)`, it tests the type, so
    a delete passes); `r in T`,
    `has r.p`, `not r.p` (not true, absent included); `x in list`, and
    `(i, x) in list` for the index too and `(k, v) in obj` for an
    object's entries; `i in 0..n` (`0..=n` inclusive) once
    per integer.
  - `let pg = db.postgres["main"]` is a value whose type is the
    resource's reference; `pg.endpoint` reads through it.
  - `set { db.backup_days = 14, db.multi_az = true } where env ==
    "prod"`: several contributions under one clause (see "Giving inputs").
  - `output k: T = t where body`: an output in one statement.
  - patterns: `(a, b) = pair`, `{ host, port } = conn` (the named fields,
    the rest ignored), `(repo, tag) = str.split(image, ":", 1)` (it fails
    when there is no tag), `zone({ name })` for named columns.
  - literals: lists `[a, b]` and objects `{ k: v }` (`{ a, b }` is `{ a: a, b: b }`);
    a string may span lines, kept as written (`str.dedent(s)` removes the
    indentation its lines share).
  - list comprehensions: `[x | pred(x), pred2(x)]` (lowers to a `collect_list` rule).
  - aggregates are bound in a body: `subnets(v, n) where n = count(s), s in
    aws.subnet, s.vpc_id == v` counts per group of the head's other
    variables (`v`); `let n = count(s) where s in aws.subnet` is one group
    (docs/grammar.md "Aggregates").
  - expression terms: `ib = ia + 1` lowers to `IB = add(IA, 1)`; a
    call's result is read in place, `not has oci.parse(c.image).digest`,
    `str.split(s, ":")[0]`.
  - binding (docs/grammar.md "Bodies"): `=` binds the side no other
    literal binds, `in` binds its left, a relation its free variables, an
    aggregate its name; `==`, `!=`, the orders, arithmetic, function
    arguments, reads, `has` and `not` need their names bound by another
    literal. Literal order is irrelevant; an unbound operand is an error
    at it, and `=` between two bound sides says to write `==`.
  - `use PATH [as N] [{ k = v }] [where B]` imports a module, a file by
    its path from the project root, once under `N`; `instance PATH N {
    k = v }` copies a component (see "Modules").

- Schemas and wildcards:
  - `decl pred(field_one: type, field_two)` declares a relation by its
    columns (a type optional); named arguments match by them:
    `pred(field_one: x)`.
  - A relation with no `decl` is typed by its uses (R-34): `az("a", 1)`
    gives `az(string, int)`, a rule's head takes its body's columns, a
    function's parameter or an attribute read types the column it reads,
    and `input p from FORMAT("path")` takes its first document's columns.
    Uses that disagree are a compile error naming both, a string literal
    is read as its column's type (`"10.0.0.0/8"` in a column
    `inet.contains` reads), and `n + 1` on a column of strings is an
    error, not a silent non-match; a column declared `any` takes every
    type. The editor's hover prints the signature.
  - A `decl` of a relation no rule defines declares one a provider feeds
    (it may have no rows).
  - `decl pred(a, b) mixed` lets a predicate have both ground facts and rules (E §2.6); without it, one that has both is a compile error naming the rule and the fact.
  - `_` is a placeholder (matches anything, never binds, never read: `_.p` or `_` as a value is an error). `_x` is an ordinary name.

## Externs

An extern is a relation a provider answers on demand, with a binding
pattern: `+` columns are inputs, `-` columns answers. A program does not
declare one: `use NAME` of a provider brings its externs into scope,
with their modes from its schema.

```dform
use aws

resource aws.subnet "private-${zone}" {
  availability_zone = zone
} where aws.availability_zone("available", zone, _)
```

A body literal of an extern is asked once the literals before it bind its
`+` arguments (an input that is a null waits), and the answers are facts of
the extern with those inputs (`why` shows them as extern calls). An extern
under `not`, in a recursive rule, stated by the program, or with an input
nothing before it binds is a compile error. Evaluation is by rounds: every
call the rules demand is asked once, then the program is evaluated again,
until no call is new.

`file`, `env`, `time` and `ssh` are built-in fact providers, declared like any
provider and needing no `dform.toml` source:

| provider | externs                                                   | answered by |
|----------|-----------------------------------------------------------|-------------|
| `file`   | `file.text(+path, -value: string)`, a path from the project root; the loaders, `yaml(path)` .. ("Documents") | dform |
| `env`    | `env.var(+name, -value: secret(string))`; `env.var(NAME)` as a term reads it | dform |
| `time`   | `time.now(-t: time)`, the current time in UTC; `time.now()` as a term reads it | dform |
| `ssh`    | `ssh.read(+host, +user, +path, -content: secret(string))` over SFTP; `ssh.run(+host, +user, +command, -stdout: string)` over exec; each as a term reads it | dform |

`ssh` is an SSH client inside dform, never the `ssh` binary or the
operator's ssh config. The host is an `ip` or a string, `NAME:PORT` for a
port other than 22. The key is the operator's: the agent's
(`SSH_AUTH_SOCK`) first, then `~/.ssh/id_ed25519` and `~/.ssh/id_rsa`
(a key with a passphrase is used through the agent); never one in the
program. A host's key is recorded in the deployment's state by the first
apply that meets it (type, SHA-256 fingerprint, when) and checked on every
contact after: a changed key is an error naming both fingerprints until
`dform state forget-host HOST [TARGET]` forgets it. A host that does not
answer yet (the connection refused, no answer within 10s, no route) and a
`read` of a path that does not exist yet are "not yet": the answer is an
open null, and an apply waits on it ("Timeouts, retries and waiting"),
asking again until the host answers or the wait's budget runs out. An
authentication failure, a changed host key, a file it may not read and a
command that exits non-zero (the error names the status and its stderr)
are errors. `ssh.run`'s stdout is the command's, byte for byte (a
trailing newline included).

```dform
use ssh
# A k3s server's kubeconfig, once cloud-init has written it.
let raw = ssh.read(server.public_ip, "ubuntu", "/etc/rancher/k3s/k3s.yaml")
```

`ssh.read`'s content is a secret from the first answer: `query` and `why`
print it by its call (`ssh.read["HOST,USER,PATH"]."4"`), and the plan
file records its keyed digest (`inputs.answers`), never the bytes; `apply
PLAN` reads it again and refuses the plan when the digest moved.

`random` is not a provider: `random.password` and friends are std
functions (below), and `use random` is an error saying so.

A program that writes `extern file.text(..)` is told to write `provider
file {}` instead; `extern` is the schema's word (provider schemas, the
compiler's tests). Another provider's externs are, for now, still declared
in the program until its schema is read at compile time (DESIGN.org R-24),
and the mock answers them from `providers/<name>/externs.df` beside the
provider's schema: facts of the extern, the rows whose `+` columns are the
inputs.

The plan file records the answers the plan read (not a call with a
`secret(...)` column, nor one that carries a secret), and `apply PLAN`
asks none of them again. Nothing else keeps an answer: every run asks
again, and `time.now()` is a new time on every plan.

What must stay the same across runs is kept by `memo.first(+key: string,
+candidate, -value)`, a built-in relation in scope with no `use`
(docs/grammar.md "Memo"): the first candidate ever given for a
key is the value on every later run. An apply keeps what it read in the
deployment's state; a plan keeps nothing. `dform state taint memo KEY
[TARGET]` forgets one, so the next run gives its candidate again, and
`why` names a kept value `memo, first kept <when>`:

```dform
use time
let created = memo.first("db-created", time.now())   # observed once
warn "rotate the database password" where {
  memo.first("db-created", time.now(), created)
  time.before(time.add(created, 30d), time.now())
}
```

```bash
dform state taint memo db-created    # the next apply keeps a new time
```

Generated secrets are std functions, derived rather than drawn:
`random.password(key[, length[, alphabet]])` (32 alphanumerics by
default; `"ascii"`, `"hex"`, `"base64"`), `random.bytes(key, length)`
(base64 text) and `random.signing_key(key)` (ed25519 in Synapse's format)
return `secret(string)`; `random.id(key[, length])` and
`random.uuid(key)` are public. Each is HKDF-SHA256 of the deployment's
master secret, `RANDOM_MASTER` in the environment or else a key derived
from the stack's key file (`state.key`, made on first use and moved with
the state), with the function, the deployment, the key and every knob in
the derivation: the same on every run, stored nowhere, and a new value
when a knob, the key or the master changes (rotate with a new key,
`"db-pw-2"`). A value that must be made once and survive a change of
master is `memo.first(KEY, random.bytes(KEY, 32))`: a memo of a secret
candidate is kept sealed with a key derived from the stack's key, never in
state in the clear, and opened in memory by the run that reads it.

A provider's `secret(T)` column never enters dform (E DR-19). The Query
names the secret columns (`QueryRequest.secret`), and the provider
answers each with where it holds the value: a SECRET null whose `held`
names the provider, the deployment, the extern and its inputs, the
column, and the value's keyed digest (with a key derived from the
deployment's plan key, `digest_key` at Configure). The run has a secret
null, labeled `kv.password/app#2` (the extern, its inputs, the column from
1); a sensitive field it reaches goes to the provider in the Apply
document as that label and where it is held, and the provider reads the
value there, inside the call; a provider that answers a secret column
with its value is refused. The mock keeps what it answered in its world
(`held`), a real provider in its own store (a secret manager).

What dform keeps of a world document beyond a run, the in-flight record of
an interrupted apply and the controller's baseline, holds a leaf at a
`sensitive` path as its keyed digest, `"(sensitive hmac-sha256:..)"`, and
the resume and the controller compare the world with it the same way.

## Documents and tables

Data that is not code is a document, loaded by the file provider's
loaders, spelled bare: `yaml(path)`, `toml(path)`, `json(path)`, `csv(path)`
(a list of objects by its header), each also over `git(repo, ref, path)`
(R-39). A loader call is a value, `let net = toml("data/network.toml")`,
read like any (`net.region`). `input p from DOC` destructures a document
into rows of a relation, typed column by column as the relation's `decl`
declares them (written once; `input p from ..` never re-spells the
columns):

```dform
input peering from csv("data/peerings.csv")
input pins from yaml(git("ops.git", "env/${env}", "pins.yaml")) where env != "dev"
input az from toml("data/network.toml")                  # its [[az]] tables
input link from toml("data/network.toml").peerings       # a selection
input service from yaml("teams.yaml").teams[*].services  # every team's
input vlan from vlans                                    # an input, a list of objects

decl peering(env: enum("dev", "stg", "prod"), name: string, peer_network: string)
decl pins(app: string, image: string)
```

A selector is a path into the document: `.name` a field, `[*]` every
element of a list, chained; a list at its end is its elements, a row per
object, and a column a row lacks is taken from the nearest enclosing
object that has it. A whole TOML document is its `[[p]]` tables by the
relation's name, so one document holds several relations. `from` takes
any document value too, an input or a `let`: the outside gives a table by
giving an input of a list of objects (`--set vlans=@vlans.yaml`). A `.df`
file of plain facts is a module (`use data.releases`, read
`releases.release(..)`), re-read like any program file; `facts(..)` is
gone.

Several `input p from ..` lines are one relation, their rows together,
and facts the program states join them. A module takes a relation from
its user with `input p` alone, and the user's `use` or `instance` block
gives its rows (`zone("a", 0)`, `zone(z, n) where az(z, n)`, or `zone
from csv("zones.csv")`); `output p` hands a relation out, read
`copy.p(x, ..)`, `c[t].p(x, ..)` or `stack[k=v].p(x, ..)`, one fact per
row (docs/grammar.md "Inputs and outputs").

The formats are `csv` (a header naming the columns), `json` and `yaml` (a
list of objects), and `toml` (the rows as `[[peering]]` entries). A row has
every column and nothing else; a cell is its column's type (a CSV cell is
read as an `int`, a `bool` or an `inet` when the column is one, a string as
an `inet` in any format), and a row that is not is an error naming the file
and line: `data/peerings.csv:3: column env: "qa" is not enum(dev, stg,
prod)`. A `secret` column is a compile error: rows are read in the clear.
A document's values read one way here, in a loader call and in
`json.decode`, `yaml.decode` and `toml.decode`: a number is an int (`2.0`
is `2`; a fraction is an error, a value's numbers are whole), a `null`
member of an object is absent (a null elsewhere is an error), a YAML tag
is an error naming its line, and a TOML datetime is a `time`.
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

A deployment's settings document, `set from yaml("config/${env}.yaml")`
in the stack, is read the same way, a leaf per input (see "Giving inputs").

## Escape hatches

### List membership

`x in e` "explodes" a list, an input's included, into rows (it lowers to the
built-in `member(List, Item)`):

```dform
host_ip(ip) where ip in vm.ips
```

`i in lo..hi` enumerates the integers from `lo` up to `hi`, and `i in
lo..=hi` up to and including it, for "once per i": a replica, a shard,
the n-th /24. Both ends are bound integers; a range anywhere but after
`in` is an error, and `int.range(lo, hi, step)` is the list.

```dform
resource compute.vm "${p}-${i}" { size = "small" } where pool(p, n), i in 0..n
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
adopt(net.vpc["network.vpc"], "existing-prod-vpc") where env == "prod", "existing-prod-vpc" in world.net.vpc

set net.vpc["network.vpc"].adopted_id = cloud_ref(net.vpc, "existing-prod-vpc", "id") where env == "prod"
```

`net.vpc["network.vpc"]` is an address: resource `vpc` of the module
`network` the stack uses, spelled as `plan` prints it.
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

Inputs and keys are the file's header: before
the body (`key`, then `input`), so a file says what it
takes first; one written below the body is an error, and `dform fmt`
moves it. Each is read as a relation, `env(E)`. An input is a cell of the attribute
aggregate: the default is an `@default` contribution, a `set .. where` a
normal one, `--set replicas=3` an `@override` that wins over both (and
`why` shows each). A `key` is an input the
target gives instead (`dform plan app env=prod`), and its value names the
deployment (see "Keyed stacks"); `--set` of one is an error. `--input-file FILE.df`
(repeatable) gives inputs as facts, one per input, `env(prod).
allowed_cidrs([inet("10.0.0.0/8")]).`, each a normal contribution stated
where the file states it, like a `set`'s; the plan file records each input file's digest.

Types are `int`, `string`, `bool`, `inet`, the quantities `bytes`, `cpu`
and `duration`, `time`, `enum(a, b, ...)`, `list(T)`, `set(T)` and objects
`{ k: T }` (`addr`, `ref(...)` and `any` are unchecked). A quantity is a
number with its unit, one token (`512Mi`, `1.5Gi`, `500m`, `2`, `1h30m`,
`30d`): bytes take binary units only (`20GB` is an error naming `20Gi`),
a cpu is cores or millicores, `m` is millicores in a cpu position and
minutes in a duration one (`500m` where nothing gives a type is an error
naming both; `cpu(500m)`, `duration(30m)` say which). It compares, sums
and takes a `min`/`max` in its base unit (`limits.memory > 2Gi`),
scales by a number (`512Mi * 2`), adds only within its dimension, over
its own dimension is a number (`limits.cpu / requests.cpu <= 4`), and
prints canonically (`1536Mi`, `2`, `1h30m`), which is also what `"${q}"`
and `string(q)` give. A provider takes it in the form its schema gives
the attribute (`bytes(quantity)` Kubernetes's string, `bytes(gib)` whole
GiB), so `storage = 20Gi` is one spelling for every provider. A `time`
is a zoned instant, `time("2026-10-02T09:00[Europe/Paris]")` or RFC 3339
with an offset, compared by its instant; adding a duration is
calendar-aware in its zone (`time.add(t, 1mo)`, `t + 1d` across a DST
change is a calendar day). The rotation idiom: `deny "rotate the key" {
key: k } where k in tls.key, time.before(time.add(k.issued, 90d), now)`,
`now` an extern's answer, never a function's. `type NAME = TYPE` names a type anywhere a type is written;
another module's alias is read through its name, `network.subnets`
(docs/grammar.md "Type aliases"). A `--set` value is read as its input's type (an `inet` parses,
a `string` takes the text) and checked before evaluation: `--set
replicas=two` is an error naming the input and its type, and so is `--set` of an input
the program does not declare. A value the program computes (an input of
a used module or a copy, a `set`'s) is checked after evaluation and a wrong type blocks the
plan. A required input with no value is an error at its declaration; one
a `set` gives, in the deployments none holds in. `check
R` refines the input (`R` names it by its name; see Refinement types).

An object input is declared by its fields, each with its own default and
check, and is one cell per leaf (docs/grammar.md "Inputs and outputs"):

```dform
input nodes {
  flavor: string = "b3-8"
  count: int = 1 check 1 <= count <= 3
}
```

`--set nodes.count=2` gives one leaf, read as its type (a path that names
no field is an error listing the fields); `--set nodes=@nodes.yaml` gives
each field the document has; `set nodes.count = 3 where env ==
"prod"` gives it from inside; `why nodes.count` shows the leaf's
layers. `input nodes: node_pool = { .. }`, an alias and an object
default, is the same input. A used module's inputs are the stack's too,
by the module's name: `--set traefik.acme_email=ops@example.com`.

A program with no `input` declarations reads `--set k=v` as the fact
`input("k", v)`.

### Giving inputs

Configuration is the inputs (R-38). The declaration gives the default, a
`set` contributes a value under a condition, and `--set` on the command
line wins over both:

```dform
set db.backup_days = 30 @override where env == "prod", region == "eu-west-1"
set { db.multi_az = true, db.backup_days = 14 } where env == "prod"
set { traefik.acme_email = "ops@example.com" } where env != "dev"   # a used module's input
set from yaml("config/${env}.yaml")
```

A `set`'s target is an input by its path: the program's own, a field of
an object input, a used module's (`traefik.acme_email`) or a copy's; a
path that names no field is an error naming the fields. `set { .. }` is
several under one clause and rank. A `set` holds where its clause does:
any condition, any subset of a composite key; one of the program's own
input with no clause is an error (give it a default). The layers are
ranks, never specificity: the default (`@default`) < a `set` (normal
unless marked) < `--set` (`@override`). Two that both hold and disagree
at the winning rank are a conflict naming both, so a broad one says
`@default` and a narrow one that should win says `@override`. The
deployment's value is the input's name, `db.backup_days`, and `why
db.backup_days` shows each layer at its `file:line`.

`set from DOC [@rank] [where B]` gives every leaf of a document (YAML,
JSON, TOML; a CSV with the columns `path` and `value`) to the input at its
dotted path, a string read as the input's type (`inet`, a quantity, a
time); a leaf at a path that is no input is a deny naming the file, the
line and the inputs there are. The demo's per-environment settings are
`config/dform/{env}.yaml`.

An input a `set` gives is the program's to decide: `dform test` does not
enumerate it, and a required one is missing only in a deployment none of
them holds in (a violation, `input k is required and has no value`). The
`settings` statement, its rows (`settings prod { .. }`, `settings[env]`,
`settings _`, `let cfg = settings[env]`) and dform.toml's `config` are
gone; each is an error naming the form to write.

### Refinement types

A `check` on an input, on an attribute of a `type` block, or a provider
schema's `type_refine(T, Path, C)` fact refines a value:

```dform
input db { backup_days: int = 3 check 1 <= backup_days <= 35 }
input gke { control_plane_cidr: inet check inet.prefix_len(control_plane_cidr) == 28 }
type google.container_cluster { zones: list(string) check len(zones) >= 3 }
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

Every `.df` file is a module, named by its path from the project root
(R-65): `config.df` is `config`, `modules/net.df` is `modules.net`,
`stacks/platform.df` is `stacks.platform`. A path is looked up, never
searched; `[packages.NAME] path = "../infra"` in `dform.toml` mounts
another project at `NAME`; `std` is in every scope. `use` imports a
module once under its name (or `as` one), its inputs bound by a block on
the `use` or by their defaults; `component NAME { .. }`, an item of a
module, is the thing copied many times, by `instance PATH NAME { .. }`:

```dform
# database.df: a module with inputs and a resource.
input backup_days: int
input subnet                                 # a relation its user gives
decl subnet(s: net.subnet)
resource db.postgres db { backup_days, subnets = [s | subnet(s), s in net.subnet] }
output iam_need = { action: "db.connect", resource: db }

# network.df: a component, copied once per network.
component vpc {
  input vpc_net: inet                        # set by each instance
  input zones: list(string) = ["a", "b"]     # a default: @default rank
  resource net.vpc vpc { cidr = vpc_net }
  zone_index(z, i) where z = zones[i]        # private to each copy
  output vpc: net.vpc = vpc                  # an address output
  decl private_subnet(subnet: net.subnet)
  private_subnet(s) where s in net.subnet
  output private_subnet                      # a relation, read a row at a time
}

# stacks/dform.df
use baseline
instance network.vpc main { vpc_net = inet(cidrs.main) }
use database {
  backup_days = 14
  subnet(s) where main.private_subnet(s)     # rows of its relation
} where env != "dev"
```

A module's import and a component's copy are one mechanism, stamped under
a name (`database`, `main`):

- resource names are scoped, `main.vpc` (its address from outside
  `net.vpc["main.vpc"]`), in `want`, `arg`, `attr`, `adopt` and `ref`;
  inside a component `T[e]` is relative to the copy, in a module a name
  it writes out is its own and a variable any resource its user sees;
- every predicate a component defines is private to the copy: another
  copy's `zone_index` is a different relation, and reading it from
  outside is an error naming the component. A module's are its import's,
  read as `database.p(..)`, its `let`s and inputs as `database.k`, its
  resources as `database.db`. A value leaves a copy through an output:
  the demo's database and kubernetes modules each give `output iam_need =
  {..} where ..`, and the stack writes `iam_need("app", n.action,
  n.resource) where n = database.iam_need`;
- `input k: T [= D] [check R]` is read by its name `k` inside. The
  block's `k = v` (under its `where` clause) is a normal-rank contribution
  to the cell `(input, n, k)` of the attribute aggregate and `D` an
  `@default` one, so `why` shows both. A block that sets an undeclared
  input is a compile error, and an input with no value is the error a
  stack input's is (`input database.backup_days is required and has no
  value`). A used module's inputs are the stack's too: `--set
  database.backup_days=14` gives one, `why database.backup_days` shows its
  layers, and `dform test` enumerates them. `check R` refines the input
  (`R` names it by its name; see Refinement types). `input p` alone is a
  relation the block gives the rows of, by rows or `p from FORMAT(..)`;
- `output k: T = t [where B]` declares an output and gives it its value in
  one statement (the type is optional), read anywhere as `n.k`
  (`output(n, k, V)`); an output typed by a resource type (`output vpc:
  net.vpc = vpc`) is the scoped address of the copy's resource.
  `network.vpc[t].vpc` reads every copy's, `t` the copy's name. `output
  p` exports a relation: `main.private_subnet(s)` reads the copy's rows,
  `network.vpc[t].private_subnet(s)` every copy's; `output k { f = t }`
  is an object;
- with a clause, the copy or the import exists only while it holds.

A name a module does not define reads outward, its user's: `env` in a
policy pack is the stack's. A module used from two stacks runs in both,
each in its own state. `instance` of a module, `use` of a component, a
copy with no name and `instance` of a stack are errors naming what to
write.

### Policies

A policy pack is a module of `set`, `deny` and `warn` statements, used
explicitly. It writes any attribute without a grant; ranks are the
ownership model and `why` names each contribution's module. The
stratifier partitions a write by its head's constant type and path (`*`
where the head has a variable).

```dform
# baseline.df
set r.tags = { team: "platform" } where r in resource
deny "db must be private" { resource: pg } where ...
warn "prod should enable audit logging" { env: "prod" } where ...

# stacks/dform.df
use baseline
```

Every contribution to one attribute meets in one lattice cell; objects merge
per key, and a list path several sources contribute to is declared a set:

```dform
type_lattice(iam.policy, "statements", "set")
```

A used module's inputs are the stack's to give, from a `set`
(`set baseline.audit.sinks = ["s3", "cloudwatch"] where env ==
"prod"`) or a document (see "Giving inputs").

## Testing

The denies are the tests; there is no test or scenario syntax (R-32).
`dform test [TARGET] [K=V..]` evaluates the program once for every
combination of its inputs against an empty mock world (the provider's
schema, no world, no state, nothing written), and every deny must hold in
each. The input space is the stack's inputs, each leaf of an object input,
and every used module's input its `use` block leaves to the stack
(`--set pg.public=true`): an input the target pins (a key's `K=V`) or
`--set` pins is that value; an `enum` input takes each of its values, a
`bool` both; a key whose type is not an enum takes each value a
deployment of the stack was applied with; any other input takes its
default, and one with none is an error naming it (pin it, or give it an
enum type). An input a `set` gives is the program's to decide in
the deployments it holds in, and no axis (R-38). More than 4096 combinations is an error asking to pin some.

```dform
deny "prod keeps 14 days of db backups" where env == "prod", not db.postgres["database.main.db"].backup_days == 14
deny "dev has no database" where env == "dev", _ in db.postgres
```

It prints a result set, a row per combination: its inputs, then `ok` or
`denied` (or `error`, for one that does not compile). Each that failed
follows as the command that plans it with its denies (or its error),
each with its doc comment (`#|` above the deny: the test's doc) beside
it; it exits non-zero if any failed:

```
test p: 4 combinations of env, public
env   public  result
dev   false   ok
dev   true    ok
prod  false   ok
prod  true    denied
denied  dform plan p.df --set env=prod --set public=true
  - a database is never public
test p: 4 combinations, 1 failed
```

`dform test shop env=prod` pins the key, so only prod's combinations
run. A what-if plan is `plan --set k=v`.

```bash
cargo run -- -C examples/demo test dform
cargo run -- -C examples/demo test dform env=prod
```

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
the syntax's answer: a chain whose head is a `let` of a reference
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
defun navigation for rules (by head predicate), components, `use`s (by
path), instances and resources (by type and name).

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
  owner (rule, `file:line:col`, the module used or the copy), then the
  derivation as `dform why --core` prints it.
- *Values at point* (R-20): on a read or the declaration of an input, a
  `let`, an output (`n.k`, `c[e].k`, `m.k`), an object input's field
  (`cidrs.main`) or a resource's attribute (`vpc.cidr`): its value for
  the selected deployment (one per copy, a null as the plan prints it,
  a secret by its label), then the cell's winning rank, every
  contribution with its rank and owner, and the derivation, as for an
  attribute above.
- *Inlay hints* (`textDocument/inlayHint`, R-20): at the end of each
  resource header's line, the plan's deformation of each object the
  block declares (`+ create`, `~ update`, `- delete`, `-/+ replace`,
  `undeformed`, `~ pending on ?T["a"].p`; `2× + create` for a
  component's copies), as many in all as `dform plan` plans; after each
  read of the values above, `= value`. The client shows them or not (the
  Emacs mode: `dform-ts-mode-inlay-hints`, off by default).
- *Explain* (a code action wherever something is derived, R-20): runs
  `dform.why` with `"document": true`, which writes the derivation to a
  read-only file under `$XDG_CACHE_HOME/dform/` and asks the client to
  show it (`window/showDocument`).
- *Docs at point*: on a declared name, where it is declared or used, its
  declaration's first line and doc comment (docs/grammar.md "Doc
  comments"); an alias its definition; a component, in its declaration or
  an instance's path, its docs and its inputs and outputs with theirs;
  `n.k` the output's; a schema type its description; a builtin or a keyword its
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
  enum path's values; types after `resource`; an instance block's
  component inputs and a copy's outputs after `n.`; elsewhere the builtins and keywords a
  word starts, each with its signature.
- *Quick fixes* (`textDocument/codeAction`), each on its diagnostic: an
  unknown name (quote it), `=` with both sides bound (write `==`), a predicate with both facts and rules (`decl
  p(a, b) mixed`), the collision lint (interpolate the key into the name, or
  say `isolated = true` on the stack), a required attribute nothing sets
  (a typed placeholder in the resource's block) and a ref to an address
  no rule wants (guard the block on it: `} where "other" in net.vpc`). An edit
  to a formatted file leaves it formatted.
- *References* of a predicate, an input or value name (a `{ k }` field
  included) and an object input's field (`nodes.count`), a `let` or type
  alias (bare or read through its module), an output, a component, a
  module a `use` names, an instance, a resource (by its name in scope,
  and its address written as a string, `net.vpc["main"]` in a
  `lifecycle` fact or a deny), a function, across the project's files
  and unsaved buffers and through `use` and instance scopes
  (`config.region`, `app_db.conn`, `platform[env].out`, an instance
  block's `k = v`), in strings' `${..}` holes too, read in the
  resolver's order (docs/grammar.md "Names",
  `dform_core::names`); a relation a component or a module file defines
  is its own (two components' private `helper` are two). On an attribute
  path (a field of a resource block, `r.p` in a body, an `attr`
  literal): every rule contributing to that cell, across components and
  modules, as the hover lists them.
- *Rename* of the same names. Renaming a resource or an instance whose
  addresses have state in the selected deployment adds, in
  the same edit, `moved(T, "old", new)` per address just after the
  declaration's block, so the next plan is a move and not a destroy and a
  create; an address written as a string at the top of a program
  (`net.vpc["main.vpc"]`, `"main.vpc" in net.vpc`) is
  renamed with it. A rename is checked: the selected deployment is evaluated with
  the edit applied to the buffers, and the rename is refused, naming
  what changed, if it adds a diagnostic or changes the plan in anything
  but the renamed addresses. `prepareRename` refuses keywords, builtins,
  schema types, attribute paths, provider names and dform's own
  relations (`data`, `attr`, ...), and an instance whose name is also a
  string a dynamic index `c[e]` may read (naming where the string is).
- Formatting (`dform fmt`'s formatter) and go-to-definition of every
  name the references find: its declaration (a relation's `decl`, else
  its first rule; an output's typed declaration; a resource two types
  name, the one the attribute's `ref(T)` takes, R-74); on a `use` or
  `instance` path, the file (or the component in it) the path names
  (R-65), and from a name a `use` binds, its file. A std function
  (`inet.subnet`) goes to its signature line, a provider type
  (`net.vpc`) to the line of its schema file that declares it and an
  attribute (`cidr = ..`, `vpc.cidr`) to its `type_attr` row; a type
  only a provider's run time declares has none, its hover says so. The
  files shipped inside dform (`std/*.df`, the built-in schemas) are
  extracted read-only under `$XDG_CACHE_HOME/dform/` (R-24). A place
  that names nothing answers empty, never an error.

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
- functions declared in `std/*.df` (docs/grammar.md "Functions"): the prelude's constructors `int`, `string`, `inet`, `ip`, `iprange`, `bytes`, `cpu`, `duration`, `time`, `url` and `format`, `len`, `ref`, `scoped`, `cloud_ref`, `declassify`; `inet.subnet`, `inet.host`, `inet.addr`, `inet.contains`, `inet.overlaps`, `inet.prefix_len`, `ip.unspecified`, `str.split`, `str.lower`, `str.upper`, `str.dedent`, `str.trim`, `str.replace`, `str.starts_with`, `str.ends_with`, `str.contains`, `str.format`, `str.pad_left`, `str.pad_right`, `str.len`, `str.slice`, `list.len`, `list.join`, `list.sort`, `list.sort_by`, `list.unique`, `list.flatten`, `list.zip`, `list.min`, `list.max`, `list.sum`, `list.contains`, `list.first`, `list.last`, `time.parse`, `time.format`, `time.in_zone`, `time.add`, `time.until`, `time.before`, `duration.parse`, `duration.total`, `bytes.to`, `cpu.to`, `regex.match`, `regex.capture`, `regex.replace`, `semver.parse`, `semver.satisfies`, `semver.compare`, `oci.parse`, `oci.pinned`, `oci.with_digest`, `hash.sha256`, `hash.short`, `base64.encode`, `base64.decode`, `url.parse`, `url.join`, `url.with_scheme`, `url.with_host`, `url.with_port`, `url.with_path`, `url.with_query`, `url.encode`, `path.join`, `path.dir`, `path.base`, `path.ext`, `path.rel`, `path.clean`, `json.decode`, `json.encode`, `yaml.decode`, `yaml.encode`, `toml.decode`, `toml.encode`; arithmetic `+ - * / %`; aggregates `collect_*`, `count`, `sum`, `min`, `max`, `any`, `all`, bound in a body (`n = count(x)`)
- list helper predicate: `member(List, Item)` and `member(List, Index, Item)` (Index starts at 0)
- safe(ish) negation: `not` requires the atom be ground at evaluation time

Provider model (in progress): the demo uses the mock provider, `dform __provider fake`, a
separate process behind the plugin protocol (tests and benches may link it in
instead, `dform-direct`) that supplies schema facts
(`providers/<name>/schema.df`) and discovery facts (inventory), and supports plan/apply
against a world file, with chaos injection.
