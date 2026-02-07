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
unique setting(2).

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
- use `+=` for mergeable attributes (tags, IAM statements, labels)
- record atoms (`setting{...}`) over positional arguments
- object/list literals (`{k: v}`, `[a, b]`) over lots of `tags.foo` keypaths
- list comprehensions (`[X | ...]`) over hand-written `collect(...)` rules
- `when <guard> { ... }` to avoid repeating the same guard on many statements
- declare merge behavior for shared attributes (`merge_rule(tags, map_merge)`, `merge_rule(iam.policy, statements, set)`) when multiple sources contribute

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

## Declare Merge Semantics for Shared Attributes

If multiple rule sets contribute to the same attribute, prefer `arg_add/4` (or `+=`)
and declare how that attribute should merge:

```prolog
merge_rule(tags, map_merge).
merge_rule(iam.policy, statements, set).
```

This makes composition predictable and avoids accidental scalar conflicts.

Settings can be layered the same way:

```prolog
merge_rule(setting, audit.sinks, set).
setting_add(prod, audit.sinks, ["s3"]).

settings prod {
  audit.sinks += ["cloudwatch"]
}.
```

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
