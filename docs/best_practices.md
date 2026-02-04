# dform / Datalog Best Practices

This document captures the patterns that make Datalog-style configuration scale.
These are the same ideas that make Terraform modules practical: reuse, composition,
and predictable behavior.

## Model Your Branching as Data

If you see lots of repeated guards like:

```prolog
arg("db.postgres", Db, "backup_days", 14) :- env("prod"), want("db.postgres", Db).
arg("db.postgres", Db, "backup_days", 3) :- env("staging"), want("db.postgres", Db).
```

Prefer a lookup table:

```prolog
setting("prod", "db.backup_days", 14).
setting("staging", "db.backup_days", 3).

arg("db.postgres", Db, "backup_days", Days) :-
  want("db.postgres", Db),
  env(Env),
  setting(Env, "db.backup_days", Days).
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
arg("net.subnet", Sn, "visibility", "private").
```

Good:

```prolog
arg("net.subnet", Sn, "visibility", "private") :- want("net.subnet", Sn).
```

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
