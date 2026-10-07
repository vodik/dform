# dform

Infrastructure is a database. An account is a table of networks, a
table of instances, a table of DNS records, each row with attributes and
pointing at others. What you want is a set of tables too, and a plan is
the difference between the two. A policy is a query that must return no
rows. "Why is this here" is a question about which rows produced it.

A dform program is facts and rules over those tables, evaluated all at
once to a fixpoint, the way Datalog evaluates a query. Every derived row
has a derivation, so every line of the plan says where it came from. The
plan is itself a table the program's own policy reads. Apply reconciles
the world with it, in ticks: what a tick creates (an endpoint, a
kubeconfig) is a value the next tick plans with.

State, modules, policy, secrets, approvals and an audit log are built
in. A complete program, here on the AWS-shaped mock:

```dform
use aws { region = "us-east-1" }

resource aws.vpc main { cidr_block = "10.0.0.0/16" }

#| One private subnet in every available zone of the region.
resource aws.subnet "private-${availability_zone}" {
  vpc_id = main
  cidr_block = inet.subnet(main.cidr_block, 8, n)
  availability_zone
} where aws.availability_zone("available", availability_zone, n)
```

```
$ dform plan
plan: 4 changes (4 create) over 1 tick

tick 1  4 changes
  + aws.vpc main                   shop.df:3
      cidr_block = "10.0.0.0/16"
  + aws.subnet private-us-east-1a  shop.df:6  with n = 0
      availability_zone = "us-east-1a"
      cidr_block = "10.0.0.0/24"
      vpc_id = main
  + aws.subnet private-us-east-1b  shop.df:6
      availability_zone = "us-east-1b"
      cidr_block = "10.0.1.0/24"
      vpc_id = main
  + aws.subnet private-us-east-1c  shop.df:6  with n = 2
      availability_zone = "us-east-1c"
      cidr_block = "10.0.2.0/24"
      vpc_id = main
```

The subnet block ends in `where`, which makes it a rule: one subnet per
answer. `aws.availability_zone` is a table the provider answers (a data
source), binding each zone's name and its stable index `n`, so the n-th
zone gets the n-th /24. `vpc_id = main` is a reference, printed as the
address it names; apply makes the VPC first. When the region gains a
zone, the next plan has one more subnet and the file does not change.

The examples run on a fake cloud built into dform, with no credentials:
`cargo install --path .`, then `dform -C examples/tour plan`.
`examples/tour` is a tutorial read top to bottom; each sample below
that cites `stacks/tour.df` is its output.

## Why a rule, not a template

In HCL a resource block is a template filled in from variables,
evaluated top down, with a patch for each thing a template cannot say:
`for_each` and `dynamic` for repetition, `depends_on` for an edge it
cannot see, `-target` and a second run for a value not known yet,
`default_tags` for a tag that should be everywhere. In dform a block is
a rule. Its clause is a query; its attributes are contributions to cells
that other rules may also write. Everything else follows.

**Every rule sees every resource, and an attribute has many authors.**
A policy reads any resource in any module, declared anywhere, and writes
into it:

```dform
set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'net.vpc["blue.vpc"].tags'
{ component: "network", team: "shop" }
```

The component wrote `component`, the policy wrote `team`. Writes merge
per leaf by rank, `@default` below normal below `@override`; two writes
at one rank that disagree are a conflict naming both. Order never
matters. `why` names every layer:

```
$ dform why orders.backup_days env=prod
db.postgres orders.backup_days = 14
  = database.backup_days  stacks/tour.df:145
  = 14                    stacks/tour.df:139
  over 1 @default         stacks/tour.df:27
```

**Policy adds as well as forbids, in the same file.** The `set` above
is policy. So is a deny, checked by every plan and every apply:

```dform
deny "a database must not be public" { database: pg } where pg in db.postgres, pg.public
```

```
$ dform plan --set public_db=true
...
constraint violations:
- a database must not be public  database = "orders"
Error: blocked by constraints
```

It exits 4. The plan is a table the same rules read, one
`deformation(kind, resource, before)` row per change, so "no deletes in
prod" is `deny "no deletes in prod" where env == "prod",
deformation("delete", _, _)`, and "a replace needs a signature" is a
`requires_approval` row:

```
$ dform query 'deformation(k, r, _)' --set database.backup_days=7
K         R
"update"  db.postgres orders
```

**Rules recurse.** Which networks reach which, through a hub, is a path
of any length; the routes are a rule over it and stay right as spokes
come and go:

```dform
link(h, t) where hub(h), spoke(t)
link(t, h) where hub(h), spoke(t)

reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)
```

```
$ dform why --tree 'reaches("blue", "green")'
decl reaches(a: string, b: string)
reaches("blue", "green")
  stacks/tour.df:281  reaches(a, c) where reaches(a, b), link(b, c)
  with a = "blue", c = "green", b = "main"
  ├─ reaches("blue", "main")
  │    stacks/tour.df:280  reaches(a, b) where link(a, b)
  │    with a = "blue", b = "main"
  │    └─ link("blue", "main")
  │         stacks/tour.df:278  link(t, h) where hub(h), spoke(t)
  │         with t = "blue", h = "main"
  │         ├─ hub("main")   stacks/tour.df:250
  │         └─ spoke("blue")   stacks/tour.df:247
  └─ link("main", "green")
       stacks/tour.df:277  link(h, t) where hub(h), spoke(t)
       with h = "main", t = "green"
       ├─ hub("main")   stacks/tour.df:250
       └─ spoke("green")   stacks/tour.df:248
```

**A value the cloud produces later is a value now.** A policy named
after a database's endpoint cannot be counted before the database
exists. dform plans it in the tick after, and says what it waits on:

```dform
resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
```

```
$ dform plan tour env=prod
...
tick 2  ? changes
  waits on  orders.endpoint
  iam.policy "connect-${host}"             stacks/tour.df:323  waits on orders.endpoint
```

Apply makes tick 1, learns the endpoint, prints tick 2's plan with real
names, and asks again:

```
plan: 1 change (1 create) over 1 tick

tick 2  1 change
  + iam.policy "connect-orders.db.fake"  stacks/tour.df:323  with pg = db.postgres orders
Apply tick 2 to tour[env=prod]? [y/N]
```

A provider's settings are values like any other, so a cluster and what
runs on it are one program and one plan: `use k8s { kubeconfig =
k3s.kubeconfig }` puts every Kubernetes object in the tick after the
server it is read from.

**Everything explains itself, absence included.** Every plan line has
its file and line; `why` takes any address, value or row, and for one
that is not there names the rule that could have made it and the first
condition that failed:

```
$ dform why 'net.subnet private-us-test-1c'
net.subnet private-us-test-1c: no rule derives it
  stacks/tour.df:104  resource net.subnet "private-${z}" { .. } where zone(z, n)
    zone("us-test-1c", n): no row
    nearest: ("us-test-1a", 1), ("us-test-1b", 2)
```

The words, with Terraform's nearest:

| word | means | in Terraform |
|---|---|---|
| **resource** | a thing dform makes and manages; with a `where` clause, one per answer | resource, `for_each` |
| **type** | what a resource is: a provider's (`aws.vpc`) or a component | resource type |
| **component** | a type you define: resources with inputs and outputs | a module instance |
| **module** | a `.df` file, named by its path; `use` imports it, its rules run here | a module's source |
| **provider** | a namespace of types and externs a process serves; `use ovh { .. }` configures it, `use ovh as ca` is a second one | provider, `alias` |
| **extern** | a table a provider answers on demand | data source |
| **stack** | a file under `stacks/`, deployed with its own state | root module |
| **key** | an input that selects the deployment: one state per value | `terraform.workspace` |
| **deployment** | a stack at one key, `shop[env=prod]` | workspace |
| **input**, **let**, **output** | a value given from outside; computed; published | variable, local, output |
| **set** | a write into any cell from anywhere, at a rank | none |
| **set(T)** | an attribute every writer adds elements to (a role's policies) | an attachment resource |
| **deny**, **warn** | a query whose rows refuse the plan, or report | Sentinel, OPA |
| **change** | one plan line: create, update, replace, delete, forget | planned action |
| **tick** | one round of apply; a later tick plans with what earlier ones made | a second run with `-target` |
| **later** | what waits on something no tick of this plan makes | none |
| **has** | whether a value is there, or a resource exists | `can()`, `depends_on` |
| **master** | a deployment's root secret, sealed under a passphrase; every generated secret derives from it | none |
| **destroy** | the plan against an empty program: every object deleted, dependents first | `terraform destroy` |
| **uri** | a location's type; its scheme picks the transport (`ssh://`, `git+https://`) | none |
| **coeffect** | a function that reads the context (`io.read`, `time.now()`); every other is pure | none |
| **why** | the derivation of any value, row or absence | none |

There is no `data` block, no `locals`, no `variable`, no `count`, no
`depends_on`: a provider's table is the data source, a `let` the local,
an `input` the variable, a clause the repetition, a reference or `where
has r` the edge.

## The language

`examples/tour` has each idea with the command to run;
`docs/grammar.md` is the reference.

**Facts and rules.** A fact is a row, `zone("us-test-1a", 1)`. A rule
derives rows, `link(h, t) where hub(h), spoke(t)`. Lower-case names are
variables; a variable used once is an error, so a typo is not a cross
product of cloud resources.

**Resources and references.** `resource TYPE NAME { attr = value }`;
with `where`, one per answer, the name interpolating the clause. An
entry that is only a name takes the value of that name. `vpc = main`
gives the resource; the provider sends its id once it exists, and a
program never reads an id. A dot in a function or a clause reads the
value now, and the compiler says when that makes the block wait a tick.
`resource T NAME = VALUE` takes a whole document as the body.

**Paths.** One dotted grammar names everything: `config.region`, a
copy's output `blue.vpc`, a deployment's `platform[env].ingress_ip`, a
module's resource `k3s.admin`. A segment holding a dot is quoted,
`k3s."k8s-lab.vodik.xyz"`. `[k]` takes one element by key; `[_]` ranges
over every one, so a baseline is one line per workload type:

```dform
set k8s.deployment[_].spec.template.spec.containers[_].resources.limits = {
  cpu: 500m,
  memory: 256Mi,
} @default
```

**Types.** Strings stop at the edge: a provider's schema types every
attribute, and a `--set`, a YAML cell or a CSV field is parsed to the
declared type there, or rejected with its file and line. A literal takes
the type its position wants, as in Postgres. `inet` and `ip` with their
arithmetic (`inet.subnet`, `n.bits`, `"10.0.0.5" in n`); quantities
(`512Mi` is `bytes`, `500m` a `cpu`, `30d` a `duration`), compared in
base units and sent in each provider's form; `time`, `semver`, `uri`
(`u.host`, `u.port`), `oci` (`c.image.tag`); `enum`, `list`, `set`,
`ref(T)`, `secret(T)`, objects. A `check` refines any of them and is a
deny over the value:

```dform
input agents: int = 0 check 0 <= agents <= 3
input disk: bytes = 50Gi check 10Gi <= disk <= 4Ti

deny "subnets overlap" { a: x, b: y } where {
  x in net.subnet
  y in net.subnet
  x != y
  inet.overlaps(x.cidr, y.cidr)
}
```

**Definedness.** `has x` holds when `x` has a value: `not has
c.resources.limits` finds a container without limits. A computed
attribute has none until the provider reports it, so what `has` gates
waits for it. `has r` of a resource holds once it exists: `resource
k8s.deployment web { .. } where has warm_cache` applies the tick after
the cache, an order with no value passed.

**Functions.** A function is pure or a coeffect, a read the context
satisfies: `io.read`, `time.now()`, `memo.first`, a provider's extern.
Each is named by its package, subject first (`str.split(s, ",")`,
`inet.subnet(n, 4, i)`, `oci.with_tag(base, release)`); there is no
prelude. `random.password("db")` is pure: derived from the deployment's
master, the same every run, stored nowhere. `memo.first(key, v)` keeps
the first value it was given, for what cannot be derived again.

**Documents.** `io.read(LOCATION)` is a location's text; a location is a
project path or a uri whose scheme picks the transport (`file:`,
`https://`, `git+https://..?ref=`, `ssh://` over SFTP, `s3://`, `data:`).
Each format's package decodes it, and rows keep their file and line:

```dform
input peering from csv.decode(io.read("data/peerings.csv"))

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")

resource k8s.custom_resource_definition "${d.metadata.name}" = d where {
  d in yaml.decode(io.read("git+https://github.com/traefik/traefik/docs/crds.yml?ref=v3.7.14"))
}
```

A git read is pinned to the commit the plan recorded, so `apply PLAN`
reads what plan read. A host still booting is "not yet": the read waits
for the tick that makes it. `io` has no write; a write is a provider's
apply. `yaml.encode`, `json.encode`, `toml.encode` build a document
from a value for an API that wants one as a string.

**The header.** A file begins with what it takes:

```dform
key env: enum("lab", "prod") = "lab"
input agents: int = 0 check 0 <= agents <= 3
input sizes { synapse: bytes = 20Gi, forgejo: bytes = 20Gi }
output ingress_ip: ip = k3s.ingress_ip

set { agents = 1, sizes.synapse = 100Gi } where env == "prod"
```

A `key` selects the deployment: `env` gives one state per value, named
by the target, `dform plan platform env=prod`. A `set` writes an input
under a condition, over its default; `--set` wins over both.

**Modules and components.** Every `.df` file is a module named by its
path; `use config` imports it, its rules run over what you can see,
its items read as `config.x`. A module with resources is stamped once by
its `use`, its inputs in the block: `use k3s { name = "k8s-${env}",
agents }`. A component is a type with inputs and outputs, made by
`resource network blue { cidr = "10.1.0.0/16" }`; its resources stay
visible to policy. A declaration with a clause exists only where it
holds (`use backups { .. } where backup`).

**Providers.** `use` imports a provider's types and configures it; its
settings are terms like any other, so one can be made from a resource.
The shape of a k3s cluster on OVH and what runs on it:

```dform
use ovh { endpoint = "ovh-ca", project = config.ovh_project }

use k3s { name = "k8s-${env}", region = config.region, agents }

use k8s { kubeconfig = k3s.kubeconfig }

resource k8s.namespace apps { metadata.name = "apps" }
```

`k3s.kubeconfig` is read off the server over `ssh://` once it answers,
so the namespace and everything else of `k8s` plan in tick 2. `use ovh
as ca { endpoint = "ovh-ca" }` beside `use ovh as eu { .. }` is two
configurations of one provider, `ca.instance` and `eu.instance`.

**Secrets.** The schema says which attributes are sensitive, and the
compiler follows every value made from one. A secret reaching a place
where it would be seen is a compile error, located:

```dform
output password: string = pw                           # E0304: not declared secret(string)

deny "short password" where pw.len < 12               # E0301: inspecting it leaks it
n(c) where c = count(p), p = pw                        # E0303: a count leaks cardinality
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

`secret.declassify(v, reason)` is the one way out, and says why.

**Policy and tests.** A policy pack is a module of `set`, `deny`,
`warn` and `requires_approval`, applied by `use policies.baseline`;
`dform dev effects` lists what a module reads and writes. `dform test`
evaluates the program once per combination of its enums, bools and keys,
against an empty world, and every deny must hold:

```
$ dform test
test tour: 4 combinations of env, public_db
env   public_db  result
dev   false      ok
dev   true       denied
prod  false      ok
prod  true       denied
denied  dform plan tour env=dev --set public_db=true
  - a database must not be public ctx={"database":"orders"}
...
```

## The tool

A project is a directory holding `dform.toml` (`dform init` writes one).
A file under `stacks/` is a stack; every other `.df` file is a module.
State lives in the gitignored `dform.state/`, or an S3 bucket. A
command runs on a target: `dform plan platform env=prod`.

**plan** prints what will change, by tick. A line is one of two shapes:
a change, its mark (`+ ~ - ±`), its address as the source names it and
the file and line that derive it; or an attribute, `path = value`,
with a site only when the value was written outside its own block. A
`because` line says what moved since the last apply. `-v` adds the
bindings and the writes that lost; `-vv` each value's chain:

```
$ dform plan --set database.backup_days=7
deployment: tour[env=dev]
plan: 1 change (1 update) over 1 tick

tick 1  1 change
  ~ db.postgres orders     stacks/tour.df:143
      backup_days = 1 → 7  --set database.backup_days=7
      because input database is now {backup_days: 7, multi_az: false} (was {backup_days: 1, multi_az: false})
$ dform plan -v --set database.backup_days=7
...
      backup_days = 1 → 7  --set database.backup_days=7  @override over stacks/tour.df:27 @default
```

A change is in the first tick after everything it waits on is made,
and the tick's header says what that is; `?` or `3+` counts a rule
whose answers are not known yet. `later` holds what no tick of this plan
makes: another deployment's output not applied yet, a provider
configured from outside, a deny `until tick 2`. A secret prints
`(sensitive)`. `--json` carries all of it as fields; `-q` is the bare
diff; `--out FILE` writes a plan file.

**apply** prints the plan and asks once; at a tick whose plan holds what
the first could not name, it prints that plan and asks again. `--yes`
answers every question. A plan file applies what it showed and stops
before the rest. State is written after every provider call, so an
interrupted apply resumes where it stopped; a create carries an
idempotency key, so a lost answer is looked up, never made twice. Every
call has a timeout, a retryable failure is retried with backoff, and a
tick waiting on the world (a Job's `status.succeeded`) waits within the
provider's `wait`, saying so every ten seconds. `--parallel N` overlaps
independent calls.

| exit | meaning |
|---|---|
| 0 | done, with or without changes |
| 1 | an error |
| 2 | usage: the command line is wrong |
| 3 | declined: a question answered no |
| 4 | refused by the program: a deny or a conflict |
| 5 | stopped: a plan file, a destroy or a run without the master did what it could; state is consistent and the next run resumes |
| 6 | locked: another run holds the deployment |

**destroy** removes a deployment: the plan against an empty program,
dependents first, asked for as apply asks (`plan --destroy` prints it).
A deny over deletes refuses it. Lifecycle is facts, so policy and `why`
read it:

```dform
lifecycle(k3s.server, "prevent_destroy") where env == "prod"   # a delete or replace is a deny
lifecycle(nodes, "create_before_destroy")                       # a replace builds the new one first
lifecycle(libvirt.volume["data"], "retain")                     # a delete forgets it; the world keeps it
moved(net.vpc, "main.vpc", net.vpc["core.vpc"])                 # renamed: state follows
ignore_changes(bastion, "tags.last_scan")                       # set on create, then the world's
adopt(legacy, "vpc-0a1b2c")                                     # exists already: take it over
```

**why, query, diff.** `dform why ADDR` explains a resource, a value, a
row or an absence, `why 'deny "MESSAGE"'` a deny. `dform query` asks
the fact store anything. `dform diff --since 2026-09-20` lists what the
applies since did and why. A plan that empties a rule which derived
resources at the last apply warns, naming the rule, and apply asks for
that on its own.

**Stacks and deployments.** A stack reads another's outputs by `use
stacks.platform` and `platform[env].kubeconfig`; `dform apply apps
env=lab` applies platform first, each with its own plan, question and
state, and a plan of apps before platform is applied shows its outputs
as values not known yet. `dform stack list` is every deployment, its
last apply and by whom; `dform output` its outputs; `state show` and
`state mv` its objects. A deployment holds a lock while it applies;
its audit log is hash-chained, and `dform log` prints it.

**Secrets.** State holds no secret: ids, names and keyed digests only.
Every generated secret derives from the deployment's master, kept
sealed (`state.master`) under the passphrase `[secrets] passphrase`
names, so a copy of the bucket opens nothing. A run without the
passphrase still plans in full, and proves a secret unchanged without
knowing it. `dform secrets rotate D KEY` changes one generated secret,
one plan line per place it lands; `dform secrets list` shows each
secret's age and who reads it; `dform secrets cycle` makes a new master
and changes no value. A secret output another stack reads is sealed to
that stack's key.

**Approvals.** `requires_approval(r, reason)` holds a change until
someone signs the plan's digest (a JWT or a DSSE envelope), checked
against the stack's trust root: `apply --approval FILE`.

**Providers.** A provider is a process dform starts and speaks gRPC to,
or a wasm component behind `--features wasm`; HTTP, SSH and git are the
host's, with credentials granted by name in `dform.toml`. Three are
real: Kubernetes (its schema from the cluster's OpenAPI document), OVH,
and Postgres (`postgres.role`, `postgres.database`, configured from a
connection; a password rotation is one `ALTER ROLE` with a SCRAM
verifier). The fake cloud and the AWS- and Google-shaped mocks run the
examples. `dform provider check` runs the conformance suite;
`docs/providers.md` is the protocol and the SDK, `docs/providers/` each
provider.

**The editor.** `dform lsp` gives diagnostics, hover with each term's
value for a deployment and who wrote it, completion, references, rename,
and go-to-definition, std functions and provider types included. `dform
fmt` has one normal form. `tree-sitter-dform/` is the grammar,
`editors/emacs` a mode.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, then a second run | it plans in the next tick |
| a cluster and what runs on it, in one plan | two root modules | `use k8s { kubeconfig = k3s.kubeconfig }` |
| a tag on everything, overridable | a variable threaded through every module | `set r.tags.team = "shop" @default where r in resource` |
| "why is this here?" | read the source | every plan line says; `dform why ADDR` |
| "why is this not here?" | read the source harder | `dform why` names the condition that failed |
| how many rounds an apply takes | find out during it | the plan is grouped by tick |
| rules about the change set | plan JSON through a policy engine | `deformation` rows the program's denies read |
| routes from reachability | write them out by hand | a recursive rule |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| policies tested over every environment | one test per case | `dform test` |
| one generated password rotated | taint and hope nothing else moves | `dform secrets rotate D KEY` |
| a state file that leaks nothing | encrypt the bucket | state holds no secret |

## What it is not

- Not a general-purpose language. There are no loops, no mutation and
  no effects: a function is pure or reads, and only a provider's apply
  changes the world.
- Not a configuration manager. It runs nothing on a host; cloud-init in
  `user_data` and the cluster's own controllers do that.
- Not a Helm. A manifest of one kind is a resource per document; a
  chart's render of mixed kinds is not read by kind.
- Not a proof. `dform test` enumerates enums, bools and keys and leaves
  every other input at its default.
- Not finished. It is pre-release: the language changes without
  compatibility (every `.df` here is rewritten when it does), and the
  real providers are the three above. Controller mode (`dform
  controller run`) is experimental, behind `DFORM_EXPERIMENTAL=1`.

## Where next

- `examples/tour`: the tutorial. `examples/crud-api`: a blue/green
  rollout gated on a migration Job. Each example's README says what it
  shows and the commands to run.
- `docs/grammar.md`: the language. `docs/reference.md`: every command,
  state backends, keyed stacks, secrets, approvals, the audit log.
  `docs/best_practices.md`: how to write a program.
- `proposals/` and `DESIGN.org`: the model and the decisions.

Apache-2.0. Contributions under the Developer Certificate of Origin
(`git commit -s`); there is no CLA.
