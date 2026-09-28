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
```

State is scoped to a stack. Until a `stack` statement exists, the stack is the
basename of the first `--file` without its extension: `dform.df` is stack
`dform`, `pngu.df` is stack `pngu`. Two programs never see each other's
resources.

- Core state (Terraform-style address -> remote mapping): `.dform/<stack>/state.json`.
- The fake backend's world (what "exists"): `.dform/<stack>/remote.json`.
- Discovery inventory, shared by every stack: `.dform/inventory.json`.

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

`computed` + `id` is a fresh value (an identity), `computed` + `sensitive` is a
secret, `computed` alone is open (proposal E §2.2). `optional_computed` is
Terraform's Optional+Computed: the program may set it, else Apply picks it.

The facts are injected into the program, so rules can read them and
`cargo run -- query type_attr` lists the schema.

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
  - resource merge fields: `tags += { team: platform }` lowers to `arg_add(..., "tags", {..})`.
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

Merge behavior for `arg_add/4` can be controlled per keypath:

```prolog
merge_rule(tags, map_merge).
merge_rule(iam.policy, statements, set).
```

Settings can be layered similarly via `setting_add/3`:

```prolog
merge_rule(setting, audit.sinks, set).
setting_add(prod, audit.sinks, ["s3"]).

settings prod {
  audit.sinks += ["cloudwatch"]
}.
```
```

## Status

This is an MVP:

- naive forward-chaining evaluator
- basic built-ins: `format`, `concat`, `ref`, `scoped`, `cidrsubnet`, `collect_*`
- networking built-ins: `ip`, `inet`, `iprange`, `inet_host`, `inet_addr`, `inet_subnet`, `inet_contains`, `inet_overlaps`, `ip_unspecified`
- math built-ins: `add`, `sub`
- list helper predicate: `member(List, Item)` and `member(List, Index, Item)` (Index starts at 0)
- safe(ish) negation: `not` requires the atom be ground at evaluation time

Provider model (in progress): the demo uses an in-process `fakecloud` provider that supplies
catalog facts (type ownership/capabilities) and discovery facts (inventory), and supports
plan/apply against a simulated world.
