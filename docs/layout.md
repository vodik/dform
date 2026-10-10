# Project layout

A dform project is a directory tree whose root holds `dform.toml`. One
directory has a meaning, `stacks/`; every other `.df` file is a module,
named by its path from the root.

```
dform.toml                  the project root (`dform init` writes one)
project.df                  the project module: the deployments, each a
                            resource of its stack's type
stacks/<stack>.df           one stack per file, named after it
<name>.df, <dir>/<name>.df  modules: `config.df` is the module
                            `config`, `modules/net.df` is `modules.net`
config/<stack>/<key>.yaml   per-deployment settings: `set from yaml.decode(io.read(..))`
data/<table>.csv            tables (`input p from csv.decode(io.read(...))`)
providers/<name>/           a local provider: a plugin executable, or a
                            schema (and externs) the mock plays
dform.state/                state: per deployment, audit logs, masters,
                            cache/; gitignored (a key file is a secret)
```

- `dform.toml` marks the root: the nearest directory up from the working
  directory holding one. There is no project without one: outside, `plan`
  and the `dev` views run on a program file with no state, and what reads
  or writes state refuses. It is small; programs stay in `.df` files. It holds `[project]` (a name, the
  language edition its files are written in, `edition = "2026"`, which is
  required, and the dform versions the project takes), `[providers]` (each provider's source and
  version requirement, Cargo's semver syntax; a program's `use NAME` of a
  provider takes its source from here), `[stacks.NAME]` (the stack `NAME.df`'s
  operational settings, a closed list: `backend`, `role`,
  `approvals`, `audit_sink`, `isolated`; a term is a string,
  `{stack}` the stack's full name, `stacks.NAME`, and `{k}` its key `k`'s
  value), `[defaults]`
  (the same settings for every stack whose table does not say, and an s3
  backend's `lease_duration` and `lease_renewal`), `[io]` (how a
  location is read, the grants a program's `io.read` is satisfied by: `wait`, the wait on one not there yet, and `credentials`,
  a location pattern to a credential by name),
  `[discovery]` (`exclude` globs) and `[packages.NAME]` (another project mounted at
  `NAME`, `path = "../infra"`: its modules are `infra.config`, its stacks
  `infra.stacks.platform`, read through the backend its `dform.toml`
  names).
  Never inputs or key values: a deployment is named by its target. Policy
  reads it as `project_provider(Name, Constraint)`, `project_default(Key,
  Value)` and `project_stack(Name, Key, Value)`.
- A stack is a file, named after itself: discovery takes `stacks/*.df`
  and the root files a `[stacks.NAME]` names (a one-file project is
  `dform.toml` beside `shop.df` with `[stacks.shop]`); any other root
  file is a module. Any file runs by path, named after itself, as an
  entrypoint. A `[stacks.NAME]` no file is is an error. A directory
  holding its own `dform.toml` is another project, not walked.
- A stack's keys are its `key` statements: `key env: environment` makes
  each value of `env` a deployment, given by the target (`dform plan shop
  env=prod`), never `--set`.
- Every file is a module, named by its path with dots: `config.df` is
  `config`, `modules/net.df` is `modules.net`. A module is imported by
  `use` (`use config`, then `config.region`), once under its name, its
  inputs bound by a block on the `use` (`use traefik { acme_email }`) or by
  their defaults, its resources stamped once under its name
  (`traefik.x`). `component NAME { .. }`, an item of a module, is a type
  the program defines, made many times by `resource` (`resource
  modules.net.vpc blue { .. }`). A
  path is looked up, never searched: `modules.net` is `modules/net.df`,
  and `modules.net.vpc` its `component vpc`. A stack is a module the tool
  uses: `use stacks.platform` binds to its deployments, and no program
  makes a resource of it. A module named like the standard library's (`str.df`,
  `list.df`) is an error: `std` is in every scope already.
- `project.df` is the project module: which deployments the project
  has, `resource stacks.platform lab { env = "lab" }` per deployment,
  with clauses and ranges as anywhere. `dform plan` and `dform apply`
  with no target run on it, in dependency order; a deployment it does
  not list is a target of its own; one an apply of it made that it no
  longer lists, the next apply destroys (docs/grammar.md "Deployed
  modules"). It is the matrix of environments a Terraform project keeps
  as workspaces or a directory each.
- A keyed stack's settings document is one file per deployment under
  `config/<stack>/`, named by the key's value, `set from
  yaml.decode(io.read("config/dform/${env}.yaml"))` in the stack: `config/dform/prod.yaml`
  is `dform[env=prod]`'s. A key the target leaves out is its input's default,
  for `plan` and `apply` alike (both print `deployment: dform[env=staging]`
  first, `-v` adding `(env from its default)`); `controller run` names
  every key. A key
  defaulting to `"prod"` or `"production"` is a warning.
- Every path a program states resolves from the project root: module
  paths (`use modules.net`), table and config sources
  (`csv.decode(io.read("data/peerings.csv"))`, `set from yaml.decode(io.read("config/dform/${env}.yaml"))`),
  `file.*` externs, input relations from files, a provider's `source` and a
  trust root.
- `dform.state/` is gitignored: each deployment's master (the key file
  `state.key` without `[secrets]`; sealed in `state.master` with it) is
  its secrets' root (docs/reference.md "Secrets").

The lints: a `key` in a file a program uses as a module is an error at
its line, and so is a resource of a stack anywhere but in the
project module.

## This repository

Every example is a project under `examples/<name>/`, with its own
`dform.toml`:

| project                | what it shows                                             |
|------------------------|-----------------------------------------------------------|
| `examples/tour`        | start here: a tutorial, read top to bottom                |
| `examples/demo`        | the demo: modules, a component, per-env config, matrix   |
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
