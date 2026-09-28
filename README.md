# dform (prototype)

`dform` is a tiny Datalog-like language for describing infrastructure intent as:

- facts: inputs and discovered data
- rules: derive desired resources
- constraints: enforce production invariants

This repo currently uses a **fake backend** (no AWS/GCP/Azure) so we can iterate on language + semantics.

Authoring guidance: `docs/best_practices.md`.

## Quick start

The repo includes a demo program at `dform.df` that imports reusable chunks from `modules/*.df`.

```bash
cargo run -- plan
cargo run -- apply
cargo run -- plan --set env=prod
cargo run -- strata    # evaluation order: the partition graph's strata
```

State is scoped to a stack. Until a `stack` statement exists, the stack is the
basename of the first `--file` without its extension: `dform.df` is stack
`dform`, `pngu.df` is stack `pngu`. Two programs never see each other's
resources.

- Core state (Terraform-style address -> remote mapping): `.dform/<stack>/state.json`.
- The fake backend's world (what "exists"): `.dform/<stack>/remote.json`.
- Discovery inventory, shared by every stack: `.dform/inventory.json`.

`--inventory PATH` points at the discovery inventory file directly (see
"Discovery facts" below). Default: `<world dir>/inventory.json` when `--world`
is given and that file exists there, else `.dform/inventory.json`. This is how
`examples/adopt_demo.df` runs from a clean clone with no `.dform/` setup:

```bash
cargo run -- --file examples/adopt_demo.df --inventory examples/world/inventory.json plan --set env=prod
```

`--world PATH` (plan and apply) points the fake backend at a world file
instead: the JSON of what "exists", each resource's configured `attrs` and its
`computed` values. Plan refreshes from it, apply writes it back, and the stack's
state sits beside it as `<stem>.state.json`. Edit the world file and re-plan to
see drift:

```bash
cargo run -- plan --world examples/world/dform.json    # steady state: no changes
```

A world file with no state beside it is adopted whole: everything in it is
taken as this stack's. `examples/world/<stack>.json` is the fixture format for
tests.

A `.dform/` written before state was scoped (`.dform/state.json`,
`.dform/remote.json`) is moved into the `dform` stack on the next run.

## Providers are schema files

The fake backend can pretend to be any provider: a provider is a schema file of
plain facts, `providers/<name>/schema.df`, selected with `--provider NAME`
(repeatable; default `fake`). `--provider path/to/schema.df` loads a file
directly. A `providers/<name>/schema.df` in the working directory wins over the
schemas built into the binary (`fake`, `gke`).

```prolog
type_provider(net.vpc, fakecloud).                    % who owns the type
type_attr(net.vpc, id, string, [computed, id]).       % Flags: required computed id
type_attr(db.postgres, endpoint, string, [computed]). %   sensitive nullable optional_computed
type_list_key(k8s.deployment, spec.template.spec.containers, [name]).  % list merge keys
type_mint(db.postgres, endpoint, "{name}.db.fake").   % optional: how the mock mints it
```

Built-in mock schemas: `fake` (the demo's), `gke` (pngu.df), `k8s` (fifteen
Kubernetes kinds; try `cargo run -- --file examples/k8s_demo.df --provider k8s plan`)
and `aws-mock` (twelve AWS types in the Terraform provider's shape, with its
Optional+Computed attributes and keyless sets; try
`cargo run -- --file examples/aws_demo.df --provider aws-mock plan`).
A `required` attribute the program does not set is a plan error. Lists with
`type_list_key` are diffed by key (`spec.template.spec.containers[name=web].image`),
lists of type `set` as sets. A `type_mint` string may use `{type}`, `{name}`,
`{attr}`, `{hash}`, `{n}` and `{doc:PATH}` (the program's value at PATH, e.g.
Kubernetes' `generateName`).

`computed` + `id` is a fresh value (an identity), `computed` + `sensitive` is a
secret, `computed` alone is open (proposal E §2.2). `optional_computed` is
Terraform's Optional+Computed: the program may set it, else Apply picks it.
Setting a plain `computed` attribute is a compile error naming the resource
and the path.

The facts are injected into the program, so rules can read them and
`cargo run -- query type_attr` lists the schema.

## Computed values come from Apply

Plan never invents a computed value. The evaluator mints one labeled null
`?T/N#Attr` per wanted resource and computed attribute (proposal E §2.5), at
normal rank for `computed` and at `@default` for `optional_computed`, so a
program's own value wins. A `ref(T, N, Attr)` to such an attribute reads that
cell: the null, or the program's value. Once `N` exists, the world's value
replaces the null before anything is derived (round 0, through the state's
identity mapping), so a steady-state stack shows no nulls. Apply mints ids,
endpoints and secrets per the schema and fills the nulls in dependency order.

```bash
cargo run -- plan
# + net.subnet.network.main::private-us-test-1a
#   vpc_id = ?net.vpc/network.main::vpc#id
```

A `sensitive` computed value never leaves the provider: what dform sees, stores
in consumers and prints is its label, `(sensitive T/N#Attr)`. A value at a
`sensitive` path the program sets prints as `(sensitive)`.

## Chaos: failure and latency injection

`apply --chaos SPEC` (repeatable) makes the fake provider misbehave, the way a
real cloud does. Deterministic: nothing sleeps and nothing is random. The world
file keeps a `tick` counter; every `apply` is one tick.

| SPEC | Effect |
|------|--------|
| `fail=T/N` | Apply of `T/N` fails before it reaches the world |
| `timeout=T/N` | Apply of `T/N` takes effect, then times out: the world has it, state does not |
| `read-lag=T/N:K` | Read (plan's refresh) returns nothing for `T/N` for `K` ticks after it is created |
| `mutate=T/N:PATH=JSON` | after the tick, the world sets `T/N`'s `PATH` to `JSON` (drift) |
| `latency=T/N:MS` | Apply of `T/N` is recorded as taking `MS`, reported, never slept |

```bash
cargo run -- apply --chaos fail=net.subnet/network.main::private-us-test-1a
cargo run -- apply --chaos 'mutate=net.vpc/network.main::vpc:cidr="10.9.0.0/16"'
```

Addresses are `TYPE/NAME` and must name a resource of the stack. The world is
saved after every action and state keeps every action that returned, so a
failed apply leaves exactly what a real cloud would.

## dform model (current)

- Core intent IR:
  - `want(Type, Name).` declares a resource instance.
  - `arg(Type, Name, KeyPath, Value).` contributes attributes (KeyPath supports dots).
  - `ref(Type, Name, Attr)` expresses dependencies.
  - `collect_set(X)` / `collect(X)` aggregates unique items (set-like).
  - `collect_list(X)` aggregates items with duplicates (multiset-like).
  - `constraint("message") :- ... .` enforces invariants.

- Ergonomic sugar (implemented as a lowering pass):
  - `resource Type Name { key = value, ... } :- ... .` lowers to `want/arg`.
  - resource merge fields: `tags += { team: platform }` is a plain contribution; how contributions merge is the path's lattice (see `docs/best_practices.md`, "One Merge Law").
  - record atoms: `setting{env: prod, key: db.backup_days, value: 14}.` (optional)
  - settings blocks: `settings prod { db.backup_days = 14 }.` (commas optional) lowers to `setting(prod, db.backup_days, 14).`
  - literals: lists `[a, b]` and objects `{k: v}`.
  - list comprehensions: `[X | pred(...), pred2(...)]` (lowers to a `collect_list(...)` rule).
  - expression terms: `IB = IA + 1` lowers to `IB = add(IA, 1)`.
  - `when <guard> { ... }.` applies a guard to each statement inside.
  - `import "path" as ns.` includes another file (simple namespacing).

- Schemas and wildcards:
  - `decl pred { field1, field2, ... }.` enables record-style matching: `pred{field1: X}`.
  - `_` is an anonymous wildcard term (matches anything, never binds).

## Escape hatches

### List membership

`member(List, Item)` is a built-in predicate that lets you "explode" list settings into rows:

```prolog
host_ip(Env, Ip) :-
  setting(Env, "vm.ips", Ips),
  member(Ips, Ip).
```

### Discovery facts

The fake backend can inject facts from `.dform/inventory.json`:

- `cloud_exists(Type, Name)`
- `cloud_attr(Type, Name, Path, Value)`
- `cloud_computed(Type, Name, Path, Value)`

These are intended to model provider data sources / inventory.

To reference discovered values in resource attributes without manually joining
`cloud_attr/cloud_computed`, you can use `cloud_ref(Type, Name, Attr)` as a value
term. In the fake backend it resolves against `.dform/inventory.json`.

`Attr` supports dotted and indexed paths like `"tags.owner"` or `"subnets[0].id"`.

### Adopt existing resources

`adopt(Type, LocalName, RemoteName)` marks a desired resource as existing already.
Planning will produce an `Adopt` action (`>` in plan output) instead of `Create`.

```prolog
adopt(net.vpc, scoped("network.main", vpc), "existing-prod-vpc") :-
  env(prod),
  cloud_exists(net.vpc, "existing-prod-vpc").

arg(net.vpc, scoped("network.main", vpc), "adopted_id", cloud_ref(net.vpc, "existing-prod-vpc", id)).
```

### Components and Modules

You can group rules into a scoped component instance:

```prolog
component network main {
  resource net.vpc vpc { cidr = "10.0.0.0/16" }.
}.
```

Inside a component:

- resource names are automatically scoped with `scoped("comp.inst", Name)`.
- `ref/3` defaults to referring to component-local resources.
- `output(Key, Value)` is sugar for `output(Scope, Key, Value)`.
- The compiler injects `component_scope(Comp, Inst, Scope)` facts.

You can also define reusable modules and instantiate them (Terraform-module-like):

```prolog
component_def network {
  resource net.vpc vpc { cidr = Cidr } :- param(vpc_cidr, Cidr).
}.

use network main { vpc_cidr = "10.0.0.0/16" }.
```

### Policies

Policies are packaged as policy packs and applied explicitly:

```prolog
policy_pack baseline {
  deny("db must not be public", {resource: Db}) :- ...
  warn("prod should enable audit logging", {env: prod}) :- ...
}.

apply_policy baseline.
```

Every contribution to one attribute meets in one lattice cell; objects merge
per key, and a list path several sources contribute to is declared a set:

```prolog
type_lattice(iam.policy, statements, set).
```

Settings are the same aggregate:

```prolog
type_lattice(settings, audit.sinks, set).
setting_add(prod, audit.sinks, ["s3"]).

settings prod {
  audit.sinks += ["cloudwatch"]
}.
```

## Testing

`cargo test` runs the integration tests under `tests/` (one file per
concern: adoption, chaos, k8s, aws, ...) plus `cargo test --lib` for the
engine's own unit tests.

Golden (snapshot) tests pin `plan` and `strata` output for a table of
example and adversarial programs: `tests/golden.rs`, snapshots under
`tests/golden/<program>/<case>.<plan|strata>.txt`. Accept a change (after
reviewing the diff) with:

```bash
UPDATE_GOLDEN=1 cargo test --test golden -- --test-threads=1
```

See `tests/golden/README.md` for details.

## Status

This is an MVP:

- naive forward-chaining evaluator
- basic built-ins: `format`, `concat`, `ref`, `scoped`, `cidrsubnet`, `collect_*`
- networking built-ins: `ip`, `inet`, `iprange`, `inet_host`, `inet_addr`, `inet_subnet`, `inet_contains`, `inet_overlaps`, `ip_unspecified`
- math built-ins: `add`, `sub`
- string/coercion built-ins: `to_int`, `to_string`, `len`, `lower`, `upper`, `split`, `join`
- list helper predicate: `member(List, Item)` and `member(List, Index, Item)` (Index starts at 0)
- safe(ish) negation: `not` requires the atom be ground at evaluation time

Provider model (in progress): the demo uses an in-process `fakecloud` provider that supplies
schema facts (`providers/<name>/schema.df`) and discovery facts (inventory), and supports
plan/apply against a world file, with chaos injection.
