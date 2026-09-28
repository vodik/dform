# Project layout

A dform project is a directory tree with a convention for where things go.
It is a convention, not a hard rule: discovery expects it, and the lints
below say when a file is somewhere else.

```
dform.toml                  the project root (`dform init` writes one)
stacks/<stack>.df           one stack per file: the only files with `stack`
modules/<module>.df         one module per file
policies/<pack>.df          policy packs
config/<stack>/<key>.yaml   per-deployment rows: a keyed stack's `config`
data/<table>.csv            tables (`input relation p(...) from csv(...)`)
scenarios/<name>.df         what-if programs
providers/<name>/           a local provider: a plugin executable, or a
                            schema (and externs) the mock plays
dform.state/                state: per deployment, audit logs, plan keys,
                            cache/; gitignored (a plan key is a secret)
```

- `dform.toml` marks the root: the nearest directory up from the working
  directory holding one. There is no project without one: outside, `plan`
  and the `dev` views run on a program file with no state, and what reads
  or writes state refuses. It is small; programs stay in `.df` files. It holds `[project]` (a name, and the dform
  versions the project takes), `[providers]` (each provider's source and
  version requirement, Cargo's semver syntax; a program's `provider NAME {}`
  takes its source from here), `[defaults]` (a `backend` template and
  `unknowns`, which a stack statement overrides, and an s3 backend's
  `lease_duration` and `lease_renewal`) and `[discovery]`
  (`exclude` globs). Never inputs, keys, settings or anything per
  deployment. Policy reads it as `project_provider(Name, Constraint)` and
  `project_default(Key, Value)`.
- Discovery walks the project for `.df` files: every file with a `stack`
  statement is a stack, and a stack's name is unique in its project. A
  directory holding its own `dform.toml` is another project, not walked.
- A stack file is named for its stack (a program without a `stack`
  statement is named for its file, so its name is the stack's).
- A module file is imported, never planned on its own; a stack file is
  planned, never imported.
- A keyed stack's config is one file per deployment under
  `config/<stack>/`, named by the key's value: `config/dform/prod.yaml` is
  `dform[env=prod]`'s.
- Every path a program states resolves from the project root: imports
  (`import "modules/network.df"`), table and config sources
  (`csv("data/peerings.csv")`, `config = yaml("config/dform/{env}.yaml")`),
  `file.*` externs, input relations from files, a provider's `source` and a
  trust root.
- `dform.state/` is gitignored: each deployment's plan key (`state.key`,
  the HMAC key of its plan files and audit log) is a secret.

The lints: a module or policy file with a
`stack` statement is an error; importing a stack file is an error
(anywhere); a `.df` outside the layout's directories is a warning.

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
| `examples/gke`         | two-phase GKE: `gke_two_phase` and `gke_one_zone`         |
| `examples/bootstrap`   | two stacks: bootstrap and the controller's workload       |
| `examples/approvals`   | approvals over a signed plan digest                       |
| `examples/crud-api`    | a blue/green rollout (proposal G); does not plan yet      |

Test-only programs and fixtures are under `tests/fixtures/` (the
adversarial stratification cases, world files, the `leaky` schema). The
mock's built-in schemas (`fake`, `gke`, `k8s`, `aws-mock`) are
`crates/dform-mock/schemas/<name>.df`. The root keeps the workspace, docs,
proposals, editors, `tree-sitter-dform`, `DESIGN.org` and `WORK.org`.
