# Project layout

A dform project is a directory tree with a convention for where things go.
It is a convention, not a hard rule: discovery expects it, and the lints
below say when a file is somewhere else.

```
dform.toml                  the project root (optional; else the git root)
stacks/<stack>.df           one stack per file: the only files with `stack`
modules/<module>.df         one module per file
policies/<pack>.df          policy packs
config/<stack>/<key>.yaml   per-deployment rows: a keyed stack's `config`
data/<table>.csv            tables (`input relation p(...) from csv(...)`)
scenarios/<name>.df         what-if programs
providers/<name>/           a local provider: a plugin executable, or a
                            schema (and externs) the mock plays
.dform/                     state, gitignored
```

- `dform.toml` marks the root. It is small and optional; programs stay in
  `.df` files.
- A stack file is named for its stack (a program without a `stack`
  statement is named for its file, so its name is the stack's).
- A module file is imported, never planned on its own; a stack file is
  planned, never imported.
- A keyed stack's config is one file per deployment under
  `config/<stack>/`, named by the key's value: `config/dform/prod.yaml` is
  `dform[env=prod]`'s.
- Paths a program states (imports, `config`, tables, `file(...)`, a
  provider's `source`, a trust root) are relative to the file that states
  them, so a stack under `stacks/` reaches the rest with `../`.

The lints, once the CLI discovers projects: a module file with a `stack`
statement is an error; importing a stack file is an error; a `.df` outside
the convention's directories is a warning.

## This repository

Every example is a project under `examples/<name>/`, with its own
`dform.toml`:

| project                | what it shows                                             |
|------------------------|-----------------------------------------------------------|
| `examples/demo`        | the demo: modules, a policy pack, per-env config          |
| `examples/pngu`        | a GKE stack, its peerings a CSV table                     |
| `examples/advanced`    | transitive closure: reachability, routes, group membership|
| `examples/adopt`       | adopting an existing resource from inventory              |
| `examples/decl`        | declared relations, record matching, wildcards            |
| `examples/k8s`         | the mock Kubernetes provider                              |
| `examples/aws`         | the Terraform-shaped mock AWS provider                    |
| `examples/refine`      | refinement types                                          |
| `examples/bootstrap`   | two stacks: bootstrap and the controller's workload       |
| `examples/approvals`   | approvals over a signed plan digest                       |

Test-only programs and fixtures are under `tests/fixtures/` (the
adversarial stratification cases, world files, the `leaky` schema). The
mock's built-in schemas (`fake`, `gke`, `k8s`, `aws-mock`) are
`crates/dform-mock/schemas/<name>.df`. The root keeps the workspace, docs,
proposals, editors, `tree-sitter-dform`, `DESIGN.org` and `WORK.org`.
