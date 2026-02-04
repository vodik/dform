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

Remote state for the fake backend is written to `.dform/remote.json`.

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
  - record atoms: `setting{env: prod, key: db.backup_days, value: 14}.` (optional)
  - settings blocks: `settings prod { db.backup_days = 14 }.` lowers to `setting(prod, db.backup_days, 14).`
  - literals: lists `[a, b]` and objects `{k: v}`.
  - list comprehensions: `[X | pred(...), pred2(...)]` (lowers to a `collect_list(...)` rule).
  - `let X = expr` in rule bodies (equivalent to `X = expr`).
  - `when <guard> { ... }.` applies a guard to each statement inside.
  - `import "path" as ns.` includes another file (simple namespacing).

### Components (MVP)

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

## Status

This is an MVP:

- naive forward-chaining evaluator
- basic built-ins: `format`, `concat`, `ref`, `cidrsubnet`, `collect`
- safe(ish) negation: `not` requires the atom be ground at evaluation time
