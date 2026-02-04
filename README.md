# dform (prototype)

`dform` is a tiny Datalog-like language for describing infrastructure intent as:

- facts: inputs and discovered data
- rules: derive desired resources
- constraints: enforce production invariants

This repo currently uses a **fake backend** (no AWS/GCP/Azure) so we can iterate on language + semantics.

Authoring guidance: `docs/best_practices.md`.

## Quick start

The repo includes a demo program at `dform.df` (VPC + subnets + Postgres + k8s-like cluster).

```bash
cargo run -- plan
cargo run -- apply
cargo run -- plan --set env=prod
```

Remote state for the fake backend is written to `.dform/remote.json`.

## dform model (current)

- Declare resources with `want(Type, Name).`
- Contribute attributes with `arg(Type, Name, KeyPath, Value).`
  - `KeyPath` is dot-separated and becomes nested objects.
- Use `ref(Type, Name, Attr)` inside values to express dependencies.
- Use `collect(X)` in a rule head to build a list.
- Enforce invariants with `constraint("message") :- ... .`

### Components (MVP)

You can group rules into a scoped component instance:

```prolog
component network main {
  want("net.vpc", "vpc").
  arg("net.vpc", "vpc", "cidr", "10.0.0.0/16").
}.
```

Inside a component:

- `want/2` and `arg/4` resource names are automatically scoped with `scoped("comp.inst", Name)`.
- `ref/3` defaults to referring to component-local resources.
- `output(Key, Value)` is sugar for `output(Scope, Key, Value)`.
- The compiler injects `component_scope(Comp, Inst, Scope)` facts.

## Status

This is an MVP:

- naive forward-chaining evaluator
- basic built-ins: `format`, `concat`, `ref`, `cidrsubnet`, `collect`
- safe(ish) negation: `not` requires the atom be ground at evaluation time
