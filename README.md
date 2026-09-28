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
cargo run -- test      # run the program's scenarios
cargo run -- strata    # evaluation order: the partition graph's strata
cargo run -- fmt modules/*.df          # format in place
cargo run -- fmt --check $(git ls-files '*.df')   # CI: list unformatted files, fail
```

State is scoped to a stack. One program owns one stack, named by its
`stack` statement:

```prolog
stack demo.main {
  backend = local(".dform/demo")   # where state, world and lock live; default .dform/<name>
  unknowns = strict                # or permissive (the default); see "Strict mode"
  role = bootstrap                 # optional: it stays batch; see "Bootstrap and handover"
}.
```

Without a `stack` statement the stack is the basename of the first `--file`
without its extension: `dform.df` is stack `dform`, `pngu.df` is stack
`pngu`. Two programs never see each other's resources. `apply` holds the
stack's lock, `<state dir>/state.lock` (the holder's pid): a second apply
of the same stack while one runs fails naming the holder; a lock whose
holder is gone (a killed apply) is taken over with a note.

Cross-stack values: `output k = t` at the top of a program is a stack
output. `apply` records the stack's outputs whose values are known in its
state and the stack's absolute state path in `.dform/stacks.json`; every
other program reads them as facts, `stack_output("net.shared", vpc_id, V)`.

`.dform/` (every path below, and the registry) is in the directory of the
first `--file`, not the working directory, so stacks whose programs sit
together share it wherever dform runs from; `--root DIR` puts it at
`DIR/.dform/` instead.

- Core state (Terraform-style address -> remote mapping, outputs): `.dform/<stack>/state.json`.
- The fake backend's world (what "exists"): `.dform/<stack>/remote.json`.
- Discovery inventory, shared by every stack: `.dform/inventory.json`.

`--inventory PATH` points at the discovery inventory file directly (see
"Discovery facts" below). Default: `<world dir>/inventory.json` when `--world`
is given and that file exists there, else `.dform/inventory.json`. This is how
`examples/adopt_demo.df` runs from a clean clone with no `.dform/` setup:

```bash
cargo run -- --file examples/adopt_demo.df --inventory examples/world/inventory.json plan --set env=prod
```

`--world PATH` (plan and apply) points the fake backend at a world file
instead: the JSON of what "exists", each resource's configured `attrs` and its
`computed` values. Plan refreshes from it, apply writes it back, and the stack's
state sits beside it as `<stem>.state.json`. Edit the world file and re-plan to
see drift:

```bash
cargo run -- plan --world examples/world/dform.json    # steady state: no changes
```

A world file with no state beside it is adopted whole: everything in it is
taken as this stack's. `examples/world/<stack>.json` is the fixture format for
tests.

A `.dform/` written before state was scoped (`.dform/state.json`,
`.dform/remote.json`) is moved into the `dform` stack on the next run.

## Providers are processes; the mock plays schema files

A provider is any executable that speaks the plugin protocol
(`proto/dform/v1/provider.proto`, gRPC; DESIGN.org "Providers and fact
plugins"). dform starts it, reads one handshake line from its stdout,
`dform-provider|1|tcp://127.0.0.1:PORT`, and calls Handshake, Configure,
Schema, Query, Read, Plan, Apply and Import on it; it exits when dform closes
its stdin. Schema returns the schema as facts, Plan and Apply are per resource,
and a sensitive computed value only ever crosses as its label. dform owns
ordering, parallelism, state and crash safety: a provider that dies during an
Apply is a failed action naming the resource, and the next `apply` resumes.

The mock provider, `dform-provider-fake` (built with dform, `src/fakecloud.rs`),
can pretend to be any provider: a provider it plays is a schema file of
plain facts, `providers/<name>/schema.df`, selected by the program's
`provider` statements (`provider gke {}.`, or `provider aws { source =
"providers/aws-mock" }.` for a directory or `.df` file relative to the
program's file) or, overriding them, with `--provider NAME` (repeatable;
default `fake`). `--provider path/to/schema.df` loads a file directly. A
`providers/<name>/schema.df` in the working directory wins over the schemas
built into the binary (`fake`, `gke`). A `source` (or `--provider` path) that
is an executable, or a directory holding one named `dform-provider*`, is a
plugin instead, started on its own; each type goes to the provider whose
schema declares it. The world file, the inventory and `--chaos` reach the mock
at Configure.

`dform provider check PATH` is the conformance suite: it runs every method
against the provider at PATH with a synthetic schema and prints one line per
check, failing if any deviates. A provider that serves its own schema (a real
API's) instead of the one it is given is checked with the `examples` its
Schema returns: documents of its own types to plan, create, update, replace
and delete. The mock passes it:

```bash
cargo run -- provider check providers/fake        # the mock
cargo run -- provider check ./my-provider         # any plugin executable
```

```prolog
type_provider(net.vpc, fakecloud).                    % who owns the type
type_attr(net.vpc, id, string, [computed, id]).       % Flags: required computed id
type_attr(db.postgres, endpoint, string, [computed]). %   sensitive nullable optional_computed
type_attr(net.vpc, cidr, string, [force_new]).         %   force_new
type_list_key(k8s.deployment, spec.template.spec.containers, [name]).  % list merge keys
type_mint(db.postgres, endpoint, "{name}.db.fake").   % optional: how the mock mints it
type_retry(db.postgres, 5).                           % optional: Read attempts (default 3)
type_replace(k8s.deployment, create_first).           % optional: create_first, destroy_first, either (default)
```

Built-in mock schemas: `fake` (the demo's), `gke` (pngu.df), `k8s` (fifteen
Kubernetes kinds; try `cargo run -- --file examples/k8s_demo.df plan`)
and `aws-mock` (twelve AWS types in the Terraform provider's shape, with its
Optional+Computed attributes and keyless sets; try
`cargo run -- --file examples/aws_demo.df plan`). Each example names its
provider with a `provider` statement.
A `required` attribute the program does not set is a plan error. Lists with
`type_list_key` are diffed by key (`spec.template.spec.containers[name=web].image`),
lists of type `set` as sets. A `type_mint` string may use `{type}`, `{name}`,
`{attr}`, `{hash}`, `{n}` and `{doc:PATH}` (the program's value at PATH, e.g.
Kubernetes' `generateName`); a template that is only `{doc:PATH}` takes the
value there whatever its type (the gke mock's cluster `zones` are its
`node_locations`).

`computed` + `id` is a fresh value (an identity), `computed` + `sensitive` is a
secret, `computed` alone is open (proposal E §2.2). `optional_computed` is
Terraform's Optional+Computed: the program may set it, else Apply picks it.
Setting a plain `computed` attribute is a compile error naming the resource
and the path.

`force_new` is the provider's "requires replace": an update that changes that
path (or one under it) is planned as a replace, `-/+` (see "Deletes and
replacement" below). The fake schema's vpc and subnet `cidr` are `force_new`.

The facts are injected into the program, so rules can read them and
`cargo run -- query type_attr` lists the schema. A run injects the rows of
the types its program, its inputs and state name (and the types they
alias), plus every `type_provider`: a derived Kubernetes schema is 16k
facts. A `query` or `why` of a schema predicate, or a rule reading one for
a type it does not spell out (`type_attr(T, ...)`), sees all of it.

## The Kubernetes provider

`dform-provider-k8s` (built with dform, `src/k8s/`) is a real provider: it
speaks the same protocol against the API server of the cluster the
environment names (`KUBECONFIG`, else `~/.kube/config`, else the pod's service
account). Select it by path; the program the mock plans applies to a cluster
unchanged:

```prolog
provider k8s { source = "./providers/k8s" }.   % a directory holding dform-provider-k8s
provider k8s { source = "../target/debug/dform-provider-k8s" }.
```

- Types are `k8s.<group>.<version>.<kind>`, the core group as `core` and the
  kind in snake_case (`k8s.apps.v1.deployment`, `k8s.core.v1.config_map`,
  `k8s.networking.k8s.io.v1.ingress`): a type name is a lowercase qualified
  name. The mock's short names (`k8s.deployment`) are aliases of them
  (`type_alias` in `providers/k8s/schema.df`), so `examples/k8s_demo.df`
  plans the same against either.
- The schema is derived at Configure from the cluster's `/openapi/v3`, and
  cached as `k8s-openapi.json` in the stack's state directory (fetched again
  when the server's index changes). `x-kubernetes-list-map-keys` are
  `type_list_key`, list-type `set` a `set`; the leaves of `status.*` and the
  server-written metadata are computed (`metadata.uid` the identity);
  `metadata.name` (a program may set `metadata.generateName` instead) and
  `metadata.namespace` (default: the kubeconfig's) are `optional_computed`
  and `force_new`; a property its object lists as `required` is required
  where the object is set; a Secret's `data` and `stringData` are sensitive;
  `type_retry` is 5; a Deployment, Service or ConfigMap is replaced
  `create_first`, a Namespace `destroy_first`.
- A field the API server defaults is `optional_computed`: one whose schema
  has a `default` (a cluster's document; the snapshot drops them), and
  `spec.clusterIP`, `spec.clusterIPs`, `spec.type`, `spec.sessionAffinity`,
  `spec.ipFamilies`, `spec.ipFamilyPolicy`, `spec.internalTrafficPolicy`,
  `spec.progressDeadlineSeconds`, `spec.revisionHistoryLimit`,
  `spec.strategy.type` and the pod template's `dnsPolicy`, `restartPolicy`,
  `schedulerName`, `terminationGracePeriodSeconds` and containers'
  `imagePullPolicy`, `terminationMessagePath`, `terminationMessagePolicy`.
  Read returns their live values as computed, so a ref to one
  (`ref(k8s.service, web, .spec.clusterIP)`) is a null until the object
  exists, then the cluster's value, as against the mock. Objects are never
  flagged (a default object would carry the server's `rollingUpdate` beside
  a program's `strategy.type = "Recreate"`), nor paths inside a list
  element (a ref cannot name one).
- The value model has no float. A float crosses the protocol as `Float`
  and the engine reads it as a string, the shortest decimal that parses
  back to the same number (`0.5`, `1e+300`). In the snapshot's kinds a
  number is only ever a quantity, which the server takes as a string too;
  a float field of a custom resource that a program sets reaches the server
  as a string.
- A Secret's `stringData` reads back from `data` (base64-decoded, the keys
  dform applied); this is tested against the fake API server only, not yet
  against a real one.
- Remote ids are `NAMESPACE/NAME` (`NAME` for a cluster-scoped kind); Read
  and Import GET the object. Writes are server-side apply as the field
  manager `dform`, never forced: a field another manager owns fails the
  action naming the manager and the field. What Read returns as
  configuration is only the fields `dform` owns (`metadata.managedFields`),
  so a server default or another manager's field is never a diff. Plan is a
  dry-run apply (`dryRun=All`) diffed against the world; a change to a field
  the server will not change in place plans a replacement. A generated name
  is picked by the provider (`generateName` plus five characters), since
  server-side apply needs a name. Delete propagates in the background and
  waits for the object to go. Watches are not used.
- With no cluster in reach, or `DFORM_K8S_OFFLINE` set, the provider is
  offline: the schema is the checked-in snapshot of Kubernetes v1.36.0's
  document (`providers/k8s/openapi-snapshot.json`, trimmed to the mock's
  kinds, Pod, ReplicaSet and RBAC by `providers/k8s/trim_openapi.py`), Plan
  diffs locally, and Read, Apply and Import fail naming why.

```bash
cargo build
cargo run -- provider check target/debug/dform-provider-k8s   # creates and deletes a ConfigMap
DFORM_K8S_OFFLINE=1 cargo run -- --file my.df plan             # no cluster: the snapshot's schema
```

`tests/k8s_offline.rs` runs without a cluster: offline, and against a fake API
server (conformance, a whole apply, a field-manager conflict).
`tests/k8s_cluster.rs` runs against a real one in a namespace
`dform-test-<random>` it deletes afterwards, only when
`DFORM_K8S_TEST_KUBECONFIG` names its kubeconfig.

## Computed values come from Apply

Plan never invents a computed value. The evaluator mints one labeled null
`?T/N#Attr` per wanted resource and computed attribute (proposal E §2.5), at
normal rank for `computed` and at `@default` for `optional_computed`, so a
program's own value wins. A `ref(T, N, Attr)` to such an attribute reads that
cell: the null, or the program's value. Once `N` exists, the world's value
replaces the null before anything is derived (round 0, through the state's
identity mapping), so a steady-state stack shows no nulls. Apply mints ids,
endpoints and secrets per the schema and fills the nulls in dependency order.

```bash
cargo run -- plan
# + net.subnet.network.main::private-us-test-1a
#   vpc_id = ?net.vpc/network.main::vpc#id
```

The plan is the Z-set `desired - world` (proposal E §2.8): per address a
create, a delete, an update, or nothing. The first line counts it in those
terms, `plan: 3 deformations (2 create, 1 update), 4 pending, 1 undetermined`,
and the sections follow in this order; what cannot be decided yet is said so:

- `moved T.Old -> T.New` lines first, one per `moved/3` rename of state.
- `definite:` deformations that run in this tick, grouped by resource: `+`
  create, `~` update, `-` delete, `>` adopt, `-/+` and `+/-` replace,
  `- T.A  (deposed)` for an object a replacement deposed. An update diffs a keyless set, or
  a list with merge keys (`containers[name=web]`), by element: an element that
  is new or gone is one `+`/`-` line with its leaves, not every later index
  shifting. Map leaves print one per line.
- `pending on ?nulls (resolves after tick N):` deformations held until those
  nulls resolve: downstream of an Apply that resolves a null something is
  stuck on, or an update whose new value is an open null against the world's
  value. Their diffs are shown now. The hint appears when the nulls' owners
  are scheduled by this plan.
- `pending groups:` resource rules stuck on a null (`gke_nodepool.? x
  unknown, on ?gke_cluster/pngu#zones`): how many there will be is not known.
  A resource rule that reads a predicate with a stuck instance is one too
  (`(reads node_pool_up("np-a"), which is stuck)`): it may derive after the
  boundary, so `apply` runs another tick for it.
- `undetermined:` policies that cannot be decided yet, `decided after tick
  N`; and denies that read a predicate with a stuck instance, which `may
  derive after tick N`. They are never reported as satisfied.
- `shadowed:` contributions at a losing rank that disagree (a warning), and
  `conflicts:` cells whose contributions disagree at the winning rank, each
  naming the resource, the path and every witness. A conflicted address is not
  a deformation; the plan still prints, then refuses with the deny.
- `denied:` denies over the plan itself (`lifecycle prevent_destroy: the plan
  would replace T.A`); the plan still prints, then refuses.
- `apply order: tick 1 [...] tick 2 [...]`: which tick each deformation runs
  in, from the dependency DAG and the nulls it waits on (a `+/-` replacement's
  deposed object is deleted the tick after).
- `(drift: ...)` marks an update where a fresh null meets a value the world
  already has: the identity mapping is stale.
- `stack NAME is undeformed`: nothing to do, nothing stuck (the only line).

Strict mode. `stack NAME { unknowns = strict }` refuses a plan that needs a
phase boundary, exactly Terraform's refusal. It is two generated denies,
so the refusal has provenance (`why`) and policy can relax it:

```
deny("strict: unresolved value at plan time", { rule: R, head: H, nulls: Ns }) :-
  stuck(R, H, _, Ns), not allow_stuck(H).
deny("strict: a pending group at plan time", { rule: R, head: H, nulls: Ns }) :-
  may_derive(R, H, Ns), not allow_stuck(H).
```

The first covers every stuck derivation: a stuck resource rule (a pending
group), an undetermined policy, and so every deformation held on a null;
the second a resource rule that may derive after the boundary
(`may_derive/3`, given to the plan's policy pass). The plan still prints,
then the violations name each instance's head pattern and nulls, and it
exits non-zero; `apply` refuses before its first Apply call.
`allow_stuck("want(\"gke_nodepool\", _)").` (a fact; no rule may derive
`allow_stuck`) allows one key's boundary. Fresh nulls still flow: a create
whose document carries `?T/A#id` of a resource created in the same tick is
definite, so single-phase plans pass. Strict is the expected default for
production stacks (`dform.df` is strict); `permissive`, the default, is for
controller mode and iterative development, where a two-phase plan applies
tick by tick.

`plan --json` prints the same report as one JSON document, the thing CI and
editors consume: `stack`, `undeformed`, a `summary` of counts, then the
sections as arrays in the order above (`definite`, `pending`,
`pending_groups`, `undetermined`, `shadowed`, `conflicts`, `apply_order`,
`unscheduled`, `moved`, `denied`). A deformation is `{action, type, name,
changes}`, a replace with `create_first`, a deposed delete with `deposed:
true`. A change is `{op, path, before, after}` (`op` is `set`, or
`add`/`remove` for a set element, with its `leaves`); a null is `{"null":
LABEL, "class": CLASS}` and a secret `{"sensitive": LABEL}`.

`cargo run -- query stuck` lists the stuck rule instances. A rule can read
them too, `stuck(RuleId, HeadPattern, Bindings, Nulls)`: a policy such as
`deny("strict", { rule: R, on: N }) :- stuck(R, _, _, N).` refuses any plan
with a stuck instance. `stuck/4` is derived above every rule that can stick,
so a reader must not itself be able to stick (read it into fresh variables
only) and nothing it derives may feed such a rule; otherwise the program is
rejected with the negative cycle.

`apply` runs in ticks. A tick applies every definite deformation in dependency
order and holds what is pending. At the boundary the results come back as
world facts, round 0 resolves the nulls they answer, the program is
re-evaluated and policy is checked again; a deny there stops the run with the
reason printed. `--max-ticks N` (default 8) bounds the loop:

```bash
cargo run -- --file examples/adversarial/gke_two_phase.df apply  # two ticks
cargo run -- --file examples/adversarial/gke_one_zone.df apply   # stops after tick 1
```

At a boundary apply also compares the refreshed world with what it last saw
(the tick's refresh and its Apply responses). A change under an address whose
deformation is pending for this boundary stops the run before the next tick,
with the change printed (`the world changed under a pending deformation after
tick N:`) and the deny that stops it (see "Policy over the plan" below); a
change anywhere else is reported as `drift after tick N:` and the
run goes on, the next tick deforming it back:

```bash
cargo run -- --file examples/adversarial/gke_two_phase.df apply \
  --chaos 'mutate=gke_cluster/pngu:deletion_protection=false'   # drift, tick 2 undoes it
```

Deletes and replacement. Deletes run after every create and update, in
reverse dependency order (a delete has no desired document left, so state
records each object's dependencies when it is applied). Which way a
replacement goes is the schema's `type_replace(T, Order)`: `destroy_first`
(`-/+`), `create_first` (`+/-`), or `either` (the default), where it is
`-/+` unless `lifecycle(T, A, create_before_destroy).` says `+/-`. That fact
on a `destroy_first` type is an error naming the type; on a `create_first`
type it is redundant. In the mocks a Kubernetes Deployment or Service and an
`aws_instance` are `create_first`, a Namespace and an `aws_s3_bucket`
`destroy_first`, the fake `net.vpc` `either`. A `-/+` replace deletes the old
object, then creates the new one under the same name. A `+/-` one: the new object is
created first under a free name (`main-2`), the old one is *deposed* (kept in
state's `deposed` section), and a boundary follows; the next tick moves what
depends on it to the replacement and then deletes the deposed object
(`- T.A  (deposed)`). A deposed object left by a failed apply is deleted by
the next one, once nothing that depends on it is still pending.

Either way the replacement is a new object, so every null that named the old
one (its id, its other computed values) is unresolved again: an existing
object that reads one is `pending on ?T/A#id (resolves after tick N)` and is
updated to the new value the tick after the replacement; a new object that
reads one is created after it in the same tick. `--chaos fresh-ids` makes
the mock mint a new id on every create, so the difference shows:

```bash
cargo run -- apply --chaos fresh-ids
cargo run -- apply --chaos fresh-ids --set env=prod   # tick 1 replaces vpcs and subnets; tick 2 updates their readers
```

Lifecycle is plain facts the planner reads (and policy can read too):

```prolog
lifecycle(net.vpc, main, prevent_destroy).        % a delete or replace of it is a deny
lifecycle(net.vpc, main, create_before_destroy).  % replace creates first (type_replace either)
moved(net.vpc, "network.main::vpc", "network.core::vpc").  % rename without destroy
ignore_changes(net.vpc, main, "tags.owner").      % set on create, then ignored
```

`moved(T, Old, New)` rewrites state's identity from `Old` to `New` before the
diff, so renaming a component instance with a `moved` fact per resource plans
undeformed (`moved T.Old -> T.New` is printed; `apply` persists it). It applies
only while state maps `Old` and not `New`, so the facts can stay.
`ignore_changes` leaves the path in a create; once the object exists it
drops the path from the desired document and from the world's, and an
update keeps the world's value there (or its absence). `prevent_destroy`
blocks `plan` and `apply` with `lifecycle prevent_destroy: the plan would
delete T.A`.

Policy over the plan. Once the plan is computed its deformations go back to
the evaluator as facts and the program is evaluated once more (the policy
pass): `deformation(Kind, T, A, Before)` per deformation (`Kind` is
`create`, `adopt`, `update`, `drift`, `pending`, `replace`, `delete` or
`delete_deposed`; `Before` a digest of the world document it was planned
against, `absent` for none) and `world_digest(T, A, Now)`. The lifecycle
denies are rules over them (`zset::POLICY_RULES`): `prevent_destroy` reads
`lifecycle/3` and a `delete` or `replace`, and at a phase boundary the held
deformations come back as `pending` with the digest they were planned
against, so the world moving under one is a deny too. `why` explains them,
and a policy can read the same facts:

```prolog
deny(M) :- deformation(delete, T, A, _), M = format("no deletes here: %s.%s", T, A).
```

```bash
cargo run -- why 'deny(M)'    # the deny, the lifecycle fact and the deformation it read
```

Only policy may read the deformation: a resource rule over it would make the
plan depend on itself, and is an error.

`apply --parallel N` (default 1) walks a tick's dependency DAG with at most N
Apply calls in flight: a create or update waits for what its document
references, deletes wait for everything else. Output order does not change
with N. On the mock the difference shows on the simulated clock:

```bash
cargo run -- apply --parallel 4 --chaos latency=net.vpc/network.main::vpc:100 \
  --chaos latency=net.vpc/network.peer::vpc:100   # the two vpcs overlap: 100ms, not 200ms
```

An apply that fails or is killed can be resumed: before a tick's first Apply
call its deformations are written to state as in flight, each with the world
document it was planned against, and each answered call takes its action out.
The next `apply` prints `resuming the apply interrupted at tick N; remaining:
...`, refreshes, and finishes the remaining actions; if the world changed
under one of them it prints the change and stops before any Apply call (run
`apply` again to plan against the world as it now is).

`plan --out PLAN.json` writes the plan file: the inputs (each `--file` with a
digest of its content, each `--input-file` with its digest, `--set`,
`--data`, `--provider`, `--world`, `--inventory`), a digest of the
refreshed world, and the deformation delta: each deformation's action,
before and after values (redacted as the plan prints them; a sensitive one
as `{"sensitive": label, "digest": HMAC}`), the nulls it
waits on and the tick it runs in; the pending groups; the nulls round 0
resolved and the ones the delta still carries; the tick schedule; and the
extern answers the plan read. `apply PLAN.json` takes its inputs from the
file (flags given on the command line must match them), refreshes and re-evaluates at
every tick, and refuses unless the delta it computes is the file's:
Terraform's stale-plan rule, stated for Z-sets. Every deformation must be in
the file with the same action, the same before-state and the same desired
values (a null the file carries matches what it has resolved to); every
deformation the file has not run yet must still be one; a new address is
allowed only where the file has a pending group of its type, and a deposed
object's delete the tick after its `+/-` replacement. It prints the difference
and stops before applying anything of that tick. So with a plan file, drift
anywhere stops the run at the boundary, where a plain `apply` reports it and
goes on. A sensitive value is compared by its digest: HMAC-SHA256 over its
bytes, keyed by 32 random bytes the stack keeps beside its state
(`state.key`, made on the first `plan --out`), so a secret that changed
between plan and apply is refused and the file never carries the bytes:

```bash
G=examples/adversarial/gke_two_phase.df
cargo run -- --file $G --world w.json plan --out plan.json
cargo run -- apply plan.json                          # the file's delta, two ticks
# the world moves after tick 1: tick 2 refuses
cargo run -- apply plan.json --chaos 'mutate=gke_cluster/pngu:name="other"'
```

A `sensitive` computed value never leaves the provider: what dform sees, stores
in consumers and prints is its label, `(sensitive T/N#Attr)`. A value at a
`sensitive` path the program sets prints as `(sensitive)` in a diff, and as its
label wherever else it appears. Everything dform prints goes through one
redactor: `plan` (text, `--json`, the plan file), `show`, `query`, `why`,
`graph`, and the policy messages on stderr, whose context quotes values and
rule text.

Secrets are also checked statically (E DR-19): one dataflow pass over
the predicate signatures labels every position a secret reaches, from a
`sensitive` schema attribute, an input declared `secret(T)` (`input pw:
secret(string)`) or an extern column declared `-value: secret(T)`, through
rules, `format`, arithmetic, lists and objects. A rule that would leak one
is a compile error with a span, before anything is evaluated:

| code  | the secret reaches |
|-------|--------------------|
| E0301 | a comparison, a builtin predicate or an inspecting function (`len`, `split`, `inet_*`) |
| E0302 | a negated literal |
| E0303 | an aggregate other than `collect_*` (`count` leaks cardinality) |
| E0304 | a public place: a resource attribute the schema does not mark `sensitive`, a setting, an output or input not declared `secret(T)`, a `deny`/`warn` |
| E0305 | a resource address (`want`, a resource name, `ref`, `scoped`) |

An input's own refinement (`input pw: secret(string) where len(pw) >=
12`) is where a secret may be checked; its deny does not print the value.
The value of an input or output declared `secret(T)` prints as its label,
`(sensitive input/#pw)`, in `query`, `why` (the `--set` leaf included) and
`show`. The plan file records a secret input's `--set` as its label and
digest, so `apply PLAN` asks for `--set pw=...` again and refuses another
value.

## Asking the fact store

`dform query` evaluates a pattern, or a conjunction of body literals, against
the final fact store and prints a table with one column per variable:

```bash
cargo run -- query 'attr(net.vpc, N, cidr, C)' --set env=prod
# N                    C
# "network.main::vpc"  10.20.0.0/16
# "network.peer::vpc"  10.21.0.0/16
# (2 rows)
cargo run -- query 'attr(T, A, cidr, C), want(T, A), T != net.subnet'
cargo run -- query 'want(net.vpc, "network.main::vpc")'    # yes / no
cargo run -- query want                                    # every want fact
```

`query --json` prints one document: `{query, count, facts}` for a predicate
name, `{query, columns, count, rows}` for a pattern, values spelled as in
`plan --json`.

Secrets print as their label, `(sensitive T/A#P)`: a value at a
`sensitive` path, and any value equal to it or string containing it, so a
rule that forwards a secret does not leak it either.

`dform why PATTERN` prints how a fact was derived, from the provenance
circuit every evaluation records (proposal E §3, DR-10): the rule (its id
and text), the rule's bindings, and the facts the firing read, recursively;
a fact given to the run says where it came from (`fact, statement N` until
the parser keeps spans, `input --set env=prod`, the provider schema, the
world). An attribute shows every contribution with its rank and owner.
Variables are allowed and every match is printed. A fact derived more than
one way shows its first derivation and `... N more alternatives`; `--all`
shows them all. An `attr`/`arg` pattern may name part of an object
attribute, by dotted path or by object value, and then shows only the
contributions that hold it:

```bash
cargo run -- why 'attr(net.vpc, "network.main::vpc", "tags.team", "platform")' --set env=prod
# attr("net.vpc", "network.main::vpc", "tags", {component: "network", env: "prod", team: "platform"})
#   by Σattr: attribute aggregate (lub_ranked, E §2.5) over 2 contributions
#   ├─ arg("net.vpc", "network.main::vpc", "tags", {team: "platform"}, "normal")   [rank normal, owner r69]
#   │    by r69: arg(Type, Name, "tags", {team: "platform"}, "normal") :- want(Type, Name)
#   │    with Name = "network.main::vpc", Type = "net.vpc"
#   ...
#   └─ ... 1 other contribution (--all)
```

`dform graph` prints Graphviz DOT, nodes and edges sorted:

```bash
cargo run -- graph | dot -Tsvg > resources.svg   # resource DAG: A -> B when A reads B (a ref, a null)
cargo run -- graph strata                        # partition graph, a cluster per stratum, negative edges dashed
cargo run -- graph vpc_peer/2                    # any binary relation of the fact store
```

## Chaos: failure and latency injection

`apply --chaos SPEC` (repeatable) makes the fake provider misbehave, the way a
real cloud does. Deterministic: nothing sleeps and nothing is random. The world
file keeps a `tick` counter; every `apply` is one tick.

| SPEC | Effect |
|------|--------|
| `fail=T/N` | Apply of `T/N` fails before it reaches the world |
| `timeout=T/N` | Apply of `T/N` takes effect, then times out: the world has it, state does not |
| `crash=T/N` | the provider process dies (exit 137) as it is called to Apply `T/N`: the action fails, nothing after it runs, and the next `apply` resumes |
| `read-lag=T/N:K` | the first `K` Reads of `T/N` after it is created return nothing (eventual consistency) |
| `mutate=T/N:PATH=JSON` | once per run, after the first tick `T/N` exists at, the world sets its `PATH` to `JSON` (drift) |
| `latency=T/N:MS` | Apply of `T/N` takes `MS` on a simulated clock, reported, never slept; the world's `timeline` records each call's start and end |
| `fresh-ids` | every Create mints new ids (the world keeps a `serial`), as a real cloud does; without it a destroy-first replacement under the same name gets its predecessor's id |

```bash
cargo run -- apply --chaos fail=net.subnet/network.main::private-us-test-1a
cargo run -- apply --chaos 'mutate=net.vpc/network.main::vpc:cidr="10.9.0.0/16"'
```

Refresh reads every object state maps; a Read that returns nothing is retried
up to the type's `type_retry(T, Attempts)` (a schema fact, default 3), each
retry logged on stderr as `retry T/N read (2/3)`. A lag within that budget is
not drift; an object still missing after the last attempt is taken as gone
(`read T/N: nothing after 3 attempts; taken as gone`).

Addresses are `TYPE/NAME` and must name a resource of the stack. The world is
saved after every action, and state (the identity mapping) is written after
every Apply call that returns, so a failed or killed apply leaves exactly what
a real cloud would: a failure or a crash at action N leaves the N-1 identities
before it in state.

## dform model (current)

The grammar is `docs/grammar.md` (edition 2026). Every `.df` file starts with
`edition 2026.`; uppercase names are variables, lowercase names symbols, `a.b`
a qualified symbol, `.a.b` a keypath, `X.a` a field of `X`; `-` and `/` are
always operators, so hyphenated or slashed names are strings (`"us-east-1"`).
A syntax error names `file:line:col` and what was expected, and parsing goes
on to the next statement, so every error in a file is reported at once.

`dform fmt [PATH...]` formats files in place (no PATH: the `--file` files):
two-space indentation per open bracket or rule body, one space around
operators and after commas, `{ a: 1 }` inside braces, at most one blank line,
and no comma where a newline already separates block entries. Line breaks
are the author's. A formatted file prints back byte for byte, and a file
with a syntax error is reported, not rewritten. `--check` rewrites nothing
and fails listing the files that would change.

- Core intent IR:
  - `want(Type, Name).` declares a resource instance.
  - `arg(Type, Name, KeyPath, Value).` contributes attributes (KeyPath supports dots).
  - `ref(Type, Name, .attr)` expresses dependencies.
  - `collect_set(X)` / `collect(X)` aggregates unique items (set-like).
  - `collect_list(X)` aggregates items with duplicates (multiset-like).
  - `constraint("message") :- ... .` enforces invariants.

- Ergonomic sugar (implemented as a lowering pass):
  - `resource Type Name { key = value, ... } :- ... .` lowers to `want/arg`.
  - resource merge fields: `tags += { team: platform }` is a plain contribution; how contributions merge is the path's lattice (see `docs/best_practices.md`, "One Merge Law").
  - record atoms: `setting{env: prod, key: .db.backup_days, value: 14}.` (optional)
  - settings blocks: `settings prod { db.backup_days = 14 }.` (newline or comma between entries) lowers to `setting(prod, .db.backup_days, 14).`
  - literals: lists `[a, b]` and objects `{k: v}`.
  - list comprehensions: `[X | pred(...), pred2(...)]` (lowers to a `collect_list(...)` rule).
  - expression terms: `IB = IA + 1` lowers to `IB = add(IA, 1)`.
  - `when <guard> { ... }.` applies a guard to each statement inside.
  - `import "path".` includes another file, once.

- Schemas and wildcards:
  - `decl pred(Field1: type, FieldTwo: type).` enables record-style matching: `pred{field1: X, field_two: Y}`.
  - `decl pred/N.` declares a predicate a provider feeds (it may have no rows).
  - `_` is an anonymous wildcard term (matches anything, never binds).

## Externs

An extern is a predicate a provider answers on demand, declared with a
binding pattern: `+` arguments are inputs, `-` arguments answers.

```prolog
extern file.json(+path, -value).
extern random.password(+name, -value) persist.

resource google_monitoring_dashboard pngu { dashboard_json = D } :-
  file.json("examples/files/dashboard-pngu.json", D).
```

A body literal of an extern is asked once the literals before it bind its
`+` arguments (an input that is a null waits), and the answers are facts of
the extern with those inputs (`why` shows them as extern calls). An extern
under `not`, in a recursive rule, stated by the program, or with an input
nothing before it binds is a compile error. Evaluation is by rounds: every
call the rules demand is asked once, then the program is evaluated again,
until no call is new.

`file.json` and `file.text` are the first real provider: they read a path
relative to the program's directory. Any other extern is asked of the mock,
which answers from `providers/<name>/externs.df` beside the provider's
schema: facts of the extern, the rows whose `+` columns are the inputs.

The plan file records the answers the plan read (not a call with a
`secret(...)` column), and `apply PLAN` asks none of them again. A
`persist` extern's answers are kept in state and never asked again, so a
generated password stays the same across runs.

## Escape hatches

### List membership

`member(List, Item)` is a built-in predicate that lets you "explode" list settings into rows:

```prolog
host_ip(Env, Ip) :-
  setting(Env, .vm.ips, Ips),
  member(Ips, Ip).
```

### Discovery facts

The fake backend can inject facts from `.dform/inventory.json`:

- `cloud_exists(Type, Name)`
- `cloud_attr(Type, Name, Path, Value)`
- `cloud_computed(Type, Name, Path, Value)`

These are intended to model provider data sources / inventory.

To reference discovered values in resource attributes without manually joining
`cloud_attr/cloud_computed`, you can use `cloud_ref(Type, Name, Attr)` as a value
term. In the fake backend it resolves against `.dform/inventory.json`.

`Attr` supports dotted and indexed paths like `.tags.owner` or `.subnets[0].id`.

### Adopt existing resources

`adopt(Type, LocalName, RemoteName)` marks a desired resource as existing already.
Planning will produce an `Adopt` action (`>` in plan output) instead of `Create`.

```prolog
adopt(net.vpc, network.main/vpc, "existing-prod-vpc") :-
  env(prod),
  cloud_exists(net.vpc, "existing-prod-vpc").

arg(net.vpc, network.main/vpc, .adopted_id, cloud_ref(net.vpc, "existing-prod-vpc", .id)).
```

`network.main/vpc` is an address: resource `vpc` of module instance
`network.main` (it lowers to `scoped("network.main", vpc)`).
```

### Stack inputs

A program declares its inputs, typed, with an optional default and an
optional refinement:

```prolog
input env: enum(dev, staging, prod) = staging.
input replicas: int = 2 where 1 <= replicas, replicas <= 10.
input allowed_cidrs: list(inet) = [].
input owner: string.                      # required: no default
```

Each is read as a relation, `env(E)`. An input is a cell of the attribute
aggregate: the default is an `@default` contribution, `--set env=prod` a
normal one that wins (and `why` shows both). `--input-file FILE.df`
(repeatable) gives inputs as facts, one per input, `env(prod).
allowed_cidrs([inet("10.0.0.0/8")]).`, each a normal contribution stated
where the file states it; the plan file records each input file's digest.

Types are `int`, `string`, `bool`, `inet`, `enum(a, b, ...)`, `list(T)`,
`set(T)` and objects `{ k: T }` (`addr`, `ref(...)` and `any` are
unchecked). A `--set` value is read as its input's type (an `inet` parses,
a `string` takes the text) and checked before evaluation: `--set env=qa`
is an error naming the input and its type, and so is `--set` of an input
the program does not declare. A value the program computes (a module
instance's input) is checked after evaluation and a wrong type blocks the
plan. A required input with no value is an error at its declaration. `where
R` is a deny unless `R` holds; `R` names the input by its name.

A program with no `input` declarations reads `--set k=v` as the fact
`input("k", v)`.

### Modules

A module groups rules behind an interface; an instance of it scopes them
(E DR-3, Terraform-module-like):

```prolog
module network {
  input vpc_net: inet.                       # set by each instance
  input zones: list(string) = ["a", "b"].    # a default: @default rank
  output vpc: addr.
  output private_subnet_ids: list(ref(net.subnet)).
  export subnet_of/2.                        # readable as network.main.subnet_of

  resource net.vpc vpc { cidr = Net } :- vpc_net(Net).
  zone_index(Z, I) :- zones(Zs), member(Zs, I, Z).   # private
  ...
  output vpc = vpc.
}.

instance network main { vpc_net = V } :- env(E), setting(E, .network.main.vpc_net, V).
instance database main { subnet_ids = Ids } :- output(network.main, private_subnet_ids, Ids).
```

Inside an instance:

- resource names are scoped, `network.main::vpc` (written `network.main/vpc`
  from outside), in `want`, `arg`, `attr`, `adopt` and `ref`;
- every predicate the module defines is private to the instance: another
  instance's `zone_index` is a different relation, and reading it from
  outside is an error naming the module. `export p/N` makes it readable as
  `m.INSTANCE.p`; `contributes p` makes the module a contributor to the
  global `p` (the demo's `iam_need`);
- `input k: T [= D] [where R]` is read as `k(V)`. The instance's `k = V :- B`
  is a normal-rank contribution to the cell `(input, m.i, k)` of the
  attribute aggregate and `D` an `@default` one, so `why` shows both. An
  instance that sets an undeclared input, or leaves out one with no
  default, is a compile error. `where R` is a deny unless `R` holds (`R`
  names the input by its name);
- `output k: T` declares an output and `output k = t` (or a rule for
  `output(k, V)`) gives it a value, read anywhere as `output(m.i, k, V)`;
  an `addr` output is the scoped address of the instance's resource.
  `output(network.I, vpc, A)` reads it with a variable instance.

The module reads every global relation; cross-instance values go through
outputs.

### Policies

Policies are packaged as policy packs and applied explicitly. A pack is a
module applied once: its own relations are private, and every `arg` it
writes must fall in one of its grants, the stratification partition spelled
by the author (E §2.6). A write outside them is a compile error at the head.

```prolog
policy baseline {
  contributes arg to _ at .tags.                 # any type, .tags and below
  contributes arg to settings at .audit.sinks.

  arg(T, A, .tags, { team: platform }) :- want(T, A).
  deny("db must be private", { resource: Db }) :- ...   # deny/warn need no grant
  warn("prod should enable audit logging", { env: prod }) :- ...
}.

apply baseline.
```

Every contribution to one attribute meets in one lattice cell; objects merge
per key, and a list path several sources contribute to is declared a set:

```prolog
type_lattice(iam.policy, .statements, set).
```

Settings are the same aggregate:

```prolog
type_lattice(settings, .audit.sinks, set).
setting_add(prod, .audit.sinks, ["s3"]).

settings prod {
  audit.sinks += ["cloudwatch"]
}.
```

## Scenarios

A scenario is a test: hypothetical facts plus ordinary deny rules, no
`expect` syntax (DESIGN L12).

```prolog
scenario prod {
  input("env", prod).
  deny("prod keeps 14 days of db backups") :-
    not attr(db.postgres, database.main/db, .backup_days, 14).
}.
```

A scenario is part of the program only when it is run. `dform test` runs
every scenario against an empty mock world (the provider's schema, no
world, no state, nothing written): the scenario's statements join the
program, and a scenario passes when nothing is denied, the program's own
denies included. It prints `scenario NAME: ok` or `denied` with each
deny, and exits non-zero if any scenario failed.
`dform plan --scenario NAME` is the same program as a what-if plan of the
stack, against its world.

```bash
cargo run -- test
cargo run -- plan --scenario dev
```

## Controller mode

`dform controller` is the second executor over the same evaluator: the
plan is the diff a reconciler applies, so nothing in the language changes.
It waits for an input relation's source or the world file to change, then
does what `apply` does (refresh, evaluate, plan, the policy pass, ticks
until the plan is undeformed or `--max-ticks`), gated by policy, and logs
one line per event and per tick:

```
04:22:01 event start
04:22:01 tick 1: plan: 3 deformations (3 create)
04:22:01 stack renfry.workload is undeformed
04:22:07 input release changed (file release.facts)
04:22:07 event input release
04:22:07 tick 1: plan: 1 deformation (1 update)
04:22:07 stack renfry.workload is undeformed
04:22:12 event world .dform/renfry.workload/remote.json changed
04:22:12 drift k8s.deployment.web spec.replicas: 3 -> 5 (auto_reconcile)
04:22:12 tick 1: plan: 1 deformation (1 update)
04:22:12 stack renfry.workload is undeformed
```

```bash
W=examples/bootstrap/workload.df
cargo run -- --file $W controller --stack renfry.workload             # poll every 500ms
cargo run -- --file $W controller --poll 100 --max-events 3           # stop after 3 events
cargo run -- --file $W controller --once                              # what changed since the last run
```

`--stack NAME` must be the program's own stack. `--poll MS` (default 500)
is how often the sources and the world file are looked at: polling, no file
notification. `--once` handles what changed since the last run (`event
resync` when nothing did) and exits; `--max-events N` exits after N events
(the start counts). A run that fails is logged (`error: ...`) and the
controller goes on watching; a failure of the first run ends it. Times are
UTC. Every run re-reads and re-evaluates the whole program.

Input relations feed facts from outside the program, re-read whenever
their source changes; `plan` and `apply` read them too:

```prolog
input relation release/1 from file("release.facts").        # release(Image).
input relation approve/2 from git("ops.git", "main", "approvals.df").
```

A source is a `.df` file of facts (`edition 2026.` first) of the
relations declared from it; a fact of any other predicate is an error
naming it. Paths are relative to the declaring file. A `git` source is read
at the ref with `git show REF:PATH` (a bare repository works) and changes
when the ref names another commit.

The controller keeps `controller.json` beside the stack's state: the stamps
of the sources and the world file as its last run left them, and the world
as that run accepted it (the baseline). It says what changed (`event start`,
`event input NAMES`, `event world`, `event resync`) and hands the world's
difference from the baseline to the program as facts, `drift(T, A, Path,
Before, After)` (one per leaf, list elements by index; `Path` "" and `After`
`absent` for an object that is gone). Policy decides about it, and the
controller gates every tick on the policy pass:

- `hold(T, A, Reason)` holds `T.A`'s deformation: `tick N: proceed: held,
  Reason: T.A`. A prod-style hold names what releases it, over an input
  relation: `hold(k8s.deployment, A, "needs approval") :- env(prod),
  deformation(_, k8s.deployment, A, _), release(I), not release_approved(I).`
- Drift of `T.A` is corrected when every drifted path is
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

The rest of the plan still applies. `examples/bootstrap/workload.df` is the
demo: replicas are `auto_reconcile`, any other drift waits for `approve`,
and in prod a release waits for `release_approved`.

## Bootstrap and handover

One program creates the cluster and installs dform in it; the workload's
state then moves into the cluster and the controller runs it from there.
`examples/bootstrap/` is the demo, on the mocks:

```bash
cargo run -- --file examples/bootstrap/bootstrap.df apply      # 3 ticks
cargo run -- --root examples/bootstrap stack handover renfry.workload --to 'k8s("dform-system/workload")'
cargo run -- --file examples/bootstrap/workload.df controller --stack renfry.workload
```

`bootstrap.df` (stack `renfry.bootstrap`, mock GCP from `gcp.df` and
mock Kubernetes) creates the network, the subnetwork and the cluster in
tick 1; the node pools (one per zone, and the zones are the cluster's)
and the `dform-system` namespace (its provider is configured from the
cluster's endpoint and CA) in tick 2; and the `dform-controller`
Deployment, whose args name the workload stack, in tick 3, once its node
pool is up. `workload.df` (stack `renfry.workload`) is a namespace, a
Deployment whose image is the `release` input relation, and a Service.

`stack NAME { role = bootstrap }` marks the stack that creates what the
controller runs in: it stays batch. `dform controller` refuses it (by its
program or by the registry), and it is never handed over.

`dform stack handover NAME --to BACKEND` moves the stack's state
directory (state, world, controller memo) to the backend and records it in
the registry, `.dform/stacks.json` (`{"state": ..., "backend": ...}`
beside the plain state paths, absolute; the controller's `event world` line
names it relative to the root). Every later run of the stack uses it,
whatever the program's `backend` says; a batch `apply` of a handed-over
stack is refused (the controller runs it), `plan` is not. The stack's
state is found in the registry, else at `.dform/NAME`; the target must be
empty and the stack not locked. Backends:

- `local("DIR")`: a directory.
- `k8s("namespace/name")`: the in-cluster backend. For now it stands in as
  the directory `k8s/namespace/name` inside the state directory of the
  registered `role = bootstrap` stack (there must be exactly one; an apply
  of a bootstrap stack registers it).

## Testing

`cargo test` runs the integration tests under `tests/` (one file per
concern: adoption, chaos, k8s, aws, ...) plus `cargo test --lib` for the
engine's own unit tests.

Golden (snapshot) tests pin `plan` and `strata` output for a table of
example and adversarial programs: `tests/golden.rs`, snapshots under
`tests/golden/<program>/<case>.<plan|strata>.txt`. Accept a change (after
reviewing the diff) with:

```bash
UPDATE_GOLDEN=1 cargo test --test golden -- --test-threads=1
```

See `tests/golden/README.md` for details.

The parser's suite is `tests/syntax.rs`: every `.df` file in the repository
and `tests/syntax/ok/` (E §7's programs among them) parses and prints back
byte for byte, and each `tests/syntax/err/*.df` fails with the diagnostics
pinned in its `.txt` (accept with `UPDATE_GOLDEN=1 cargo test --test syntax`).

## Performance

The evaluator is semi-naive over an operator IR (`src/ir/ops.rs`: Scan,
Join, Extern, AntiJoin, Filter with Stuck as its third output, Map,
Distinct, Agg, and a Fix per stratum), with a hash index per relation and
key the rules read through (`src/ir/store.rs`). Its output is the naive
loop's, byte for byte, circuit node ids included.

`benches/scale.rs` generates programs from a mock schema of 10^3 types (a
fresh `id` everywhere, an open `endpoint` on every tenth), N resources
(each with a `parent` ref to an earlier one, one in a hundred stuck on a
`format` over an endpoint), a recursive closure over the first thousand,
and a 500-rule policy pack, then plans them with the `dform` binary:

```bash
cargo bench --bench scale                  # 10^3 resources (CI-sized)
cargo bench --bench scale -- --full        # 10^4 and 10^5 resources
cargo bench --bench scale -- --resources 5000 --out /tmp/gen   # keep the programs
cargo bench --bench scale -- --bin path/to/other/dform         # A/B a build
```

It reports partition-graph nodes, provenance bytes per fact, stuck
instances per null, the evaluator's read count (index lookups plus tuples
read, deterministic), and `dform plan` wall time with /proc/loadavg and
instructions (when `perf` is installed).

## Status

This is an MVP:

- semi-naive evaluator with hash indexes (see Performance)
- basic built-ins: `format`, `concat`, `ref`, `scoped`, `cidrsubnet`, `collect_*`
- networking built-ins: `ip`, `inet`, `iprange`, `inet_host`, `inet_addr`, `inet_subnet`, `inet_contains`, `inet_overlaps`, `ip_unspecified`
- math built-ins: `add`, `sub`
- string/coercion built-ins: `to_int`, `to_string`, `len`, `lower`, `upper`, `split`, `join`
- list helper predicate: `member(List, Item)` and `member(List, Index, Item)` (Index starts at 0)
- safe(ish) negation: `not` requires the atom be ground at evaluation time

Provider model (in progress): the demo uses the mock provider, `dform-provider-fake`, a
separate process behind the plugin protocol that supplies schema facts
(`providers/<name>/schema.df`) and discovery facts (inventory), and supports plan/apply
against a world file, with chaos injection.
