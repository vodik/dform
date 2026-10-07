# Controller mode (experimental)

Controller mode is experimental (DESIGN.org R-41): `dform controller run`
and `dform stack handover` run, each printing a warning first, but `dform
--help`, `dform stack --help` and the shell completions list them only
with `DFORM_EXPERIMENTAL=1`. The relations the controller hands a program
(`drift`, `auto_reconcile`, `approve`, `hold`) and the `role =
"bootstrap"` stack setting belong to it. The README describes none of
it; this page keeps what it said, and DESIGN.org keeps the design.

What is not decided, and why it is not handed to a user yet:

- Where it runs: a process beside the state, a Deployment in the cluster
  the bootstrap stack made, or a CI job on a schedule; who starts it and
  with which credentials.
- How it is supervised: what restarts it, how two controllers of one
  deployment are kept apart beyond the apply lock, what a crash between
  events leaves for the next start.
- What an operator sees: its log lines are the only interface; there is
  no status to query, no health endpoint, no way to pause it but to stop
  it.
- What it is told: `drift`, `auto_reconcile`, `approve` and `hold` still
  take a resource's type and address as text, not a reference (R-42
  converted every other lifecycle relation).

## Running a controller

`dform controller run` is the second executor over the same evaluator: the
plan is the diff a reconciler applies, so nothing in the language changes.
It waits for a source it read (a program file, a table, a document) or the
world file to change, then
does what `apply` does (refresh, evaluate, plan, the policy pass, ticks
until the plan is undeformed or `--max-ticks`), gated by policy, and logs
one line per event and per tick:

```
04:22:01 event start
04:22:01 tick 1: plan: 3 deformations (3 create)
04:22:01 stack workload is undeformed
04:22:07 input data.releases changed (file data/releases.df)
04:22:07 event input data.releases
04:22:07 tick 1: plan: 1 deformation (1 update)
04:22:07 stack workload is undeformed
04:22:12 event world dform.state/workload/remote.json changed
04:22:12 drift k8s.deployment.web spec.replicas: 3 -> 5 (auto_reconcile)
04:22:12 tick 1: plan: 1 deformation (1 update)
04:22:12 stack workload is undeformed
```

```bash
cargo run -- -C examples/bootstrap controller run workload                   # poll every 500ms
cargo run -- -C examples/bootstrap controller run workload --poll 100 --max-events 3   # stop after 3 events
cargo run -- -C examples/bootstrap controller run workload --once            # what changed since the last run
```

The target names the stack, and of a keyed stack one deployment
(`controller run 'app[env=prod]'`). `--poll MS` (default 500)
is how often the sources and the world file are looked at: polling, no file
notification. `--once` handles what changed since the last run (`event
resync` when nothing did) and exits; `--max-events N` exits after N events
(the start counts). A run that fails is logged (`error: ...`) and the
controller goes on watching; a failure of the first run ends it. Times are
UTC. Every run re-reads and re-evaluates the whole program.

The sources are the program's files, each by its module's name (a `.df`
file of facts, `use data.releases`, is one: edit it and the controller
deploys it), and every file and ref the last run's tables and documents
read (README "Documents and tables"): a change to one is an input event,
`input NAME changed (SOURCE)`. A `git` source is stamped by the commit its
ref names.

The controller keeps `controller.json` beside the stack's state (in its
store: a directory, or the bucket of an s3 stack, as are its `approvals/`
drop directory and `approval-pending.json`): the stamps
of the sources and the world file as its last run left them, and the world
as that run accepted it (the baseline). It says what changed (`event start`,
`event input NAMES`, `event world`, `event resync`) and hands the world's
difference from the baseline to the program as facts, `drift(T, A, Path,
Before, After)` (one per leaf, list elements by index; `Path` "" and `After`
`absent` for an object that is gone). Policy decides about it, and the
controller gates every tick on the policy pass:

- `hold(T, A, Reason)` holds `T["A"]`'s deformation: `tick N: proceed: held,
  Reason: T["A"]`, for as long as the policy derives it.
- `requires_approval(r, Reason)` holds a deformation until a signed
  approval of the plan's digest arrives (README "Approvals").
- Drift of `T["A"]` is corrected when every drifted path is
  `auto_reconcile(T, A, Path)` or `approve(T, A)` holds, or the event is an
  input change. Otherwise the deformation is held (`drift at PATH needs
  approval`) and its baseline kept, so it is held again at every event
  until an approval or an input change.
- Held drift is per resource, not per path: a deformation is one action,
  so an object with `auto_reconcile` drift (replicas) and other drift (the
  image) holds the whole update, replicas included. Per-path holding waits
  for `ignore_changes` to address list paths.

The controller parses the program once and again only for a file whose
text changed, and drops what an event registered for diagnostics with the
event, so a long-running controller does not grow per event.

The rest of the plan still applies. `examples/bootstrap/stacks/workload.df` is the
demo: replicas are `auto_reconcile`, any other drift waits for `approve`,
and in prod a rollout waits for an approval of its plan (`approval/1`).

## Bootstrap and handover

One program creates the cluster and installs dform in it; the workload's
state then moves where the in-cluster controller reaches it, a bucket, and
the controller runs it from there. `examples/bootstrap/` is the demo, on
the mocks and MinIO (`crates/dform-s3/minio.sh`; its test,
`tests/controller_bootstrap.rs`, takes the fake S3 server when MinIO is not
there):

```bash
eval "$(crates/dform-s3/minio.sh start)"
cargo run -- -C examples/bootstrap apply bootstrap   # 3 ticks
cargo run -- -C examples/bootstrap stack handover workload \
  --to 's3("dform-test", "renfry/workload", {endpoint: "http://127.0.0.1:9000"})'
cargo run -- -C examples/bootstrap controller run workload
crates/dform-s3/minio.sh stop
```

The bucket must exist (dform makes none; the test makes `dform-test`).

`stacks/bootstrap.df` (stack `bootstrap`, mock GCP from `providers/gcp/schema.df` and
mock Kubernetes) creates the network, the subnetwork and the cluster in
tick 1; the node pools (one per zone, and the zones are the cluster's)
and the `dform-system` namespace (its provider is configured from the
cluster's endpoint and CA) in tick 2; and the `dform-controller`
Deployment, whose args name the workload stack, in tick 3, once its node
pool is up. `stacks/workload.df` (stack `workload`) is a namespace, a
Deployment whose image is the module of facts `data/releases.df`'s
`release`, and a Service.

`role = "bootstrap"` in a stack's `[stacks.NAME]` marks the stack that creates what the
controller runs in: it stays batch. `dform controller run` refuses it (by its
program or by the registry), and it is never handed over.

`dform stack handover NAME --to BACKEND` (NAME a deployment, `app[env=prod]`, of a keyed stack) moves the deployment's
objects (state, plan key, audit log, published outputs, controller memo)
to the backend, under the deployment's lock, and records it in the
registry, `dform.state/stacks.json` (`{"state": ..., "backend": ...}`
beside the plain state paths, absolute; the controller's `event world` line
names it relative to the root). The mock's world goes with the state into
a directory; for a bucket it stays in `dform.state/NAME/` (it is the
provider's). Every later run of the stack uses it, whatever its
`backend` says; a batch `apply` of a handed-over stack is refused (the
controller runs it), `plan` is not. The stack's state is found in the
registry, else where its backend says; the target must hold none
of a deployment's objects, and the stack not be locked. Backends:

- `local("DIR")`: a directory, relative to the project root, as a stack's
  `backend` is.
- `s3("BUCKET", "PREFIX", {endpoint: "URL", region: "R"})`: the
  deployment's own prefix of a bucket (a handover between prefixes, or
  between buckets, works alike). A bucket never holds a key file
  (reference.md "Custody"): a deployment whose master is a local
  `state.key` has it sealed first, as dform.toml's `[secrets]` says (the
  same master; a `custody` audit entry), and `state.master` moves; with
  no `[secrets]` the handover is refused, naming the setting.
- `k8s("namespace/name")`: the in-cluster backend. For now it stands in as
  the directory `k8s/namespace/name` inside the state directory of the
  registered `role = bootstrap` stack (there must be exactly one; an apply
  of a bootstrap stack registers it).


## Approvals in controller mode

In controller mode a deformation that needs an approval is held (`tick N:
proceed: held, needs approval (Reason): T["A"]`) and the plan's digest is
published: a log line, `tick N: approval needed: plan digest sha256:...`,
and `approval-pending.json` beside the state. A token for that digest
releases it when it arrives through the relation `approval/1` (the
token's text; `approval(t) where approvals.approval(t)` over a module of
facts, `use data.approvals`) or
as a file in the drop directory `approvals/` beside the state (`event
approval`); `tick N: approved by WHO: plan digest ...`. A token for another
plan is ignored, one that fails otherwise is logged (`approval refused:
...`). `examples/bootstrap/stacks/workload.df` holds a prod rollout this way.

## Watching documents and tables

The controller watches what the last run's tables read: a changed file,
or a ref that names another commit, is an input event (`input pins changed
(git ops.git env/prod:pins.yaml)`).

