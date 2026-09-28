# dform / Datalog Best Practices

This document captures the patterns that make Datalog-style configuration scale.
These are the same ideas that make Terraform modules practical: reuse, composition,
and predictable behavior.

## Model Your Branching as Data

If you see lots of repeated guards like:

```prolog
arg(db.postgres, Db, backup_days, 14) :- env(prod), want(db.postgres, Db).
arg(db.postgres, Db, backup_days, 3) :- env(staging), want(db.postgres, Db).
```

Prefer a lookup table:

```prolog
settings prod {
  db.backup_days = 14
}.

settings staging {
  db.backup_days = 3
}.

arg(db.postgres, Db, backup_days, Days) :-
  want(db.postgres, Db),
  env(Env),
  setting(Env, db.backup_days, Days).
```

Why it scales:
- you change env behavior by editing facts, not duplicating rules
- rules stay reusable across stacks/components
- you can move the `setting/3` facts into policy packs later

## Keep Facts Ground (No Variables in Facts)

In dform, `Fact(...)` statements must be fully ground. If you want a statement to
apply to many bindings, it must be a rule:

Bad:

```prolog
arg(net.subnet, Sn, visibility, private).
```

Good:

```prolog
arg(net.subnet, Sn, visibility, private) :- want(net.subnet, Sn).
```

## Prefer Sugar That Lowers to Core IR

For authoring, prefer:

- `resource` blocks over repeated `want/arg`
- record atoms (`setting{...}`) over positional arguments
- object/list literals (`{k: v}`, `[a, b]`) over lots of `tags.foo` keypaths
- list comprehensions (`[X | ...]`) over hand-written `collect(...)` rules
- `when <guard> { ... }` to avoid repeating the same guard on many statements
- declare a set lattice for list attributes several sources contribute to (`type_lattice(iam.policy, statements, set)`)

## Explode Lists Into Rows With `member/2`

If humans want to write lists, but your infra wants one resource per item, convert the list into a relation:

```prolog
settings prod {
  vm.ips = ["10.0.0.10", "10.0.0.11"]
}.

vm_ip(Env, Ip) :-
  setting(Env, vm.ips, Ips),
  member(Ips, Ip).

# If you need stable indices (order-sensitive), use member/3:
vm_ip_indexed(Env, I, Ip) :-
  setting(Env, vm.ips, Ips),
  member(Ips, I, Ip).

resource compute.vm Vm {
  private_ip = Ip
} :-
  env(Env),
  vm_ip(Env, Ip),
  Vm = format("vm-%s", Ip).
```

This is the Pattern A win: derive one resource per row, not index-based `count`.

Note: Datalog has no intrinsic ordering, so dform's aggregates are deterministic:

- `collect_set(X)` returns a sorted list of unique values
- `collect_list(X)` returns a sorted list that may include duplicates

## One Merge Law for Shared Attributes

Every contribution to one attribute, whichever resource block, module or
policy pack it comes from, meets in one cell: the attribute aggregate
`attr(Type, Name, Path, Value)`. Rules that read `arg(...)`, `setting(...)` or
`output(...)` in a body read that collapsed value, never a single
contribution, and the stratifier runs them after every contribution is in.
Statement order never matters.

How a path merges is its lattice:

- a scalar or a list is one value (Flat): two sources with different values
  are a conflict;
- an object is a map, merged per key: `tags = { env: dev }` in a resource and
  `arg(T, N, tags, { team: platform })` in a policy pack give both tags;
  two sources disagreeing on one key are a conflict;
- a list declared a set is the union of every source at the highest rank
  present; a `@default` set is replaced wholesale by a normal one:

```prolog
type_lattice(iam.policy, statements, set).
type_lattice(settings, audit.sinks, set).
setting_add(prod, audit.sinks, ["s3"]).

settings prod {
  audit.sinks += ["cloudwatch"]
}.
```

`+=`, `arg_add` and `setting_add` are plain contributions, the same as `=`;
the lattice, not the operator, decides how they merge. A dotted path
`tags.team` contributes `{ team: V }` to `tags`.

A conflict is a `deny("conflicting attribute contributions", ...)` naming the
resource, the path, and every contributing rule with its value. It is resolved
by rank, not by order. `@default` after a value loses to a plain contribution,
`@override` beats it, and a disagreement at a losing rank is only a warning.
After a `resource` or `settings` header the rank applies to every field
without its own. The core form is `arg(T, N, P, V, default|normal|override)`.

## Layer Environments With `@default`

Write the common settings once at `@default` and let each environment
override only what differs; an override wins per leaf:

```prolog
env_name(staging).
env_name(prod).

settings E @default {
  db = { backup_days: 3, multi_az: false }
} :- env_name(E).

settings prod {
  db = { backup_days: 14, multi_az: true }
}.
```

The same shape gives org-wide defaults from a policy pack without reading the
attribute it defaults: `arg(T, N, tags, { team: platform }, default) :- want(T, N).`

Because the engine lowers these to the same small core, you keep composition and
predictability without paying the verbosity tax.

## Prefer Small, Composable Predicates

- Keep predicates narrow and reusable (`setting/3`, `env/1`, `stack/1`).
- Avoid embedding meaning into long strings; derive them from facts.
- Use helper predicates for readability rather than repeating long bodies.

## Use Stratified Negation for Defaults

Negation is most useful for defaults and "absence" checks.

Pattern:

```prolog
has_env() :- input("env", _).
env("staging") :- not has_env().
env(E) :- input("env", E).
```

Rules that rely on `not ...` should be:
- non-recursive through negation
- safe: the negated atom must be ground at evaluation time

## Treat Components as the Unit of Reuse

Components give you the Terraform module benefits:
- stable names/addresses via scoping
- local naming (`"vpc"`, `"db"`, `"cluster"`)
- explicit outputs for wiring

Inside a component:
- keep resources local (`want/2`, `arg/4`)
- publish only what consumers need via `output/2`
- import policy via global constraints or future policy packs

## Keep Resource Identity Stable

Prefer stable resource names and express change via attributes. If you bake lots
of configuration into the resource name, you create needless replacements.

Good:
- `net.vpc.network.main::vpc` (stable)
- `cidr` changes across envs

Risky:
- names that include computed values, timestamps, or random suffixes

## Make Constraints Read Like Policies

Constraints are strongest when written as invariants:

- "db must not be public"
- "prod db must be multi_az"

Avoid constraints that replicate half of your config; prefer checking the final
desired shape.
