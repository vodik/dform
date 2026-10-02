# Project layout

A dform project is a directory tree with a convention for where things go.
It is a convention, not a hard rule: discovery expects it, and the lints
below say when a file is somewhere else.

```
dform.toml                  the project root (`dform init` writes one)
stacks/<stack>.df           one stack per file, named after it
modules/<module>.df         one module per file
policies/<pack>.df          policy packs
config/<stack>/<key>.yaml   per-deployment rows: a keyed stack's `config`
data/<table>.csv            tables (`input p(...) from csv(...)`)
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
  takes its source from here), `[stacks.NAME]` (the stack `NAME.df`'s
  operational settings, a closed list: `backend`, `unknowns`, `role`,
  `approvals`, `audit_sink`, `isolated`, `config`; a term is a string,
  `{stack}` the stack's name and `{k}` its key `k`'s value), `[defaults]`
  (the same settings for every stack whose table does not say, and an s3
  backend's `lease_duration` and `lease_renewal`), `[discovery]`
  (`exclude` globs) and `[remotes]` (other projects whose stacks' outputs
  this one reads, each by its backend: `platform = { backend = "..." }`).
  Never inputs or key values: a deployment is named by its target. Policy
  reads it as `project_provider(Name, Constraint)`, `project_default(Key,
  Value)` and `project_stack(Name, Key, Value)`.
- A stack is a file, named after itself: discovery takes `stacks/*.df`,
  or, in a project with no `stacks/`, the root's `.df` files (a one-file
  project is `dform.toml` beside `shop.df`). Any file runs by path, named
  after itself. A `[stacks.NAME]` no file is is an error. A directory
  holding its own `dform.toml` is another project, not walked.
- A stack's keys are its `key` statements: `key env: environment` makes
  each value of `env` a deployment, given by the target (`dform plan shop
  env=prod`), never `--set`.
- A module file is imported, never planned on its own; a stack file is
  planned, never imported.
- A keyed stack's config is one file per deployment under
  `config/<stack>/`, named by the key's value, `[stacks.dform] config =
  'yaml("config/dform/{env}.yaml")'`: `config/dform/prod.yaml` is
  `dform[env=prod]`'s. A key the target leaves out is its input's default,
  for `plan` and `apply` alike (both print `deployment: dform[env=staging]
  (env from its default)` first); `controller run` names every key. A key
  defaulting to `"prod"` or `"production"` is a warning.
- Every path a program states resolves from the project root: imports
  (`import "modules/network.df"`), table and config sources
  (`csv("data/peerings.csv")`, `config = 'yaml("config/dform/{env}.yaml")'`),
  `file.*` externs, input relations from files, a provider's `source` and a
  trust root.
- `dform.state/` is gitignored: each deployment's plan key (`state.key`,
  the HMAC key of its plan files and audit log) is a secret.

The lints: a module or policy file with a `key` is an error; importing a
stack file is an error (anywhere); a `.df` outside the layout's
directories (with `stacks/`, a `.df` at the root too) is a warning.

## This repository

Every example is a project under `examples/<name>/`, with its own
`dform.toml`:

| project                | what it shows                                             |
|------------------------|-----------------------------------------------------------|
| `examples/tour`        | start here: a tutorial, read top to bottom                |
| `examples/demo`        | the demo: modules, a policy pack, per-env config          |
| `examples/pngu`        | a GKE stack, its peerings a CSV table                     |
| `examples/advanced`    | transitive closure: reachability, routes, group membership|
| `examples/adopt`       | adopting an existing resource from inventory              |
| `examples/decl`        | declared relations, record matching, wildcards            |
| `examples/k8s`         | the mock Kubernetes provider                              |
| `examples/aws`         | the Terraform-shaped mock AWS provider                    |
| `examples/refine`      | refinement types                                          |
| `examples/gke`         | two-phase GKE: `gke_two_phase`, in two zones or one       |
| `examples/bootstrap`   | two stacks: bootstrap and the controller's workload       |
| `examples/approvals`   | approvals over a signed plan digest                       |
| `examples/crud-api`    | a blue/green rollout (proposal G)                         |

Test-only programs and fixtures are under `tests/fixtures/` (the
adversarial stratification cases, world files, the `leaky` schema). The
mock's built-in schemas (`fake`, `gke`, `k8s`, `aws-mock`) are
`crates/dform-mock/schemas/<name>.df`. The root keeps the workspace, docs,
proposals, editors, `tree-sitter-dform`, `DESIGN.org` and `WORK.org`.
