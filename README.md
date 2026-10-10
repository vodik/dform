# dform

Infrastructure should be a program its operators can read, planned over
the world as it is. Every line of the plan should say what produced it.
Policy should live beside what it governs and be checked by every plan.
A value the cloud knows only later should be a value the plan already
waits for. The program should be as clear to the language model helping
an operator as to the operator. It is. Most of this repository was
written by one, reading `why` the same way an operator does.

It works because infrastructure is a database. An account is a table of
networks, a table of instances, a table of DNS records, each row with
attributes and pointing at others. What the program wants is a set of
tables too, and a plan is the difference between the two. A policy is a
query that must return no rows, and "why is this here" asks which rows
produced it. A dform program is facts and rules over those tables,
evaluated all at once to a fixpoint, the way Datalog evaluates a query.
Every plan is the least model of the program over the world as it is,
and every line of it carries its proof, so `why` never disagrees with
the plan. Apply reconciles the world with the plan in ticks, and what
one tick creates, an endpoint or a kubeconfig, is a value the next tick
plans with. Each tick is the least model over the world the one before
it left.

Here is a complete program, on the AWS-shaped mock:

```dform
use aws { region = "us-east-1" }

resource aws.vpc main { cidr_block = "10.0.0.0/16" }

let cidr(n) = inet.subnet(main.cidr_block, 8, n)

#| One private subnet in every available zone of the region.
resource aws.subnet "private-${availability_zone}" {
  vpc_id = main
  cidr_block = cidr(n)
  availability_zone
} where aws.availability_zone("available", availability_zone, n)

deny "no public subnets" { subnet: s } where s in aws.subnet, s.map_public_ip_on_launch
```

```
$ dform plan
plan: 4 changes (4 create) over 1 tick; policy: 1 hold

tick 1  4 changes
  + aws.vpc main                   stacks/shop.df:3
      cidr_block = "10.0.0.0/16"
  + aws.subnet private-us-east-1a  stacks/shop.df:8  with n = 0
      availability_zone = "us-east-1a"
      cidr_block = "10.0.0.0/24"
      vpc_id = main
  + aws.subnet private-us-east-1b  stacks/shop.df:8  with n = 1
      availability_zone = "us-east-1b"
      cidr_block = "10.0.1.0/24"
      vpc_id = main
  + aws.subnet private-us-east-1c  stacks/shop.df:8  with n = 2
      availability_zone = "us-east-1c"
      cidr_block = "10.0.2.0/24"
      vpc_id = main

policy  1 hold
```

`aws.availability_zone` is a relation the provider answers, and the
subnet block, ending in `where`, makes one subnet per answer, so when
the region gains a zone the next plan has one more subnet and the file
does not change. The examples run on a fake cloud built into dform with
no credentials. To try one, `cargo install --path .`, then `dform -C
examples/tour plan`.

## Why a rule, not a template

A resource block should be a rule, a query whose every answer is a
resource, with attributes other rules can write too. Repetition is then
a clause, an edge a reference, and a value known later a later tick.
Terraform makes a block a template and adds `for_each`, `depends_on`,
`-target` and `default_tags` for what a template cannot say.

**Every rule sees every resource, and an attribute has many authors.**
A convention should be written once and hold in every module. A `set`
writes into any resource its clause matches, merged per leaf with what
the module wrote itself:

```dform
set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'net.vpc["blue.vpc"].tags'
{ team: "shop", component: "network" }
```

The AWS provider's `default_tags` does this for tags, across the
resources that one provider handles.

**Policy adds as well as forbids, in the same file.** A rule about a
change should sit beside the resources it governs. The plan is a table
of `deformation` rows the same rules read, so "no deletes in prod" is
`deny "no deletes in prod" where env == "prod", deformation("delete", _,
_)` over this:

```
$ dform query 'deformation(k, r, _)' --set database.backup_days=7
K         R
"update"  db.postgres orders
```

A policy over a value known only after apply should say so. The logic
has three values, and nothing is assumed false for being unknown, so
such a policy is undetermined and the plan names the tick that decides
it. OPA reads Terraform's plan JSON, where such a value is only marked
unknown.

**Rules recurse.** Anything shaped like a graph should be derived, so
routes over which networks reach which stay right as spokes come and
go:

```dform
link(h, t) where hub(h), spoke(t)
link(t, h) where hub(h), spoke(t)

reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)
```

**A value the cloud produces later is a value now.** What depends on a
value the cloud has not assigned yet should be in the plan anyway, in
the tick after that value exists, naming what it waits on:

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

Terraform stops on a `for_each` over such a value and says a `-target`
may be needed. A provider's settings are values too, so `use k8s {
kubeconfig = k3s.kubeconfig }` puts a cluster and what runs on it in
one plan, where the Terraform Kubernetes provider's documentation asks
for the two in separate `apply` operations.

**Everything explains itself, absence included.** A missing resource
should have an answer as precise as a present one. `dform why` names
the rule that could have made it and the condition that failed:

```
$ dform why 'net.subnet private-us-test-1c'
net.subnet private-us-test-1c: no rule derives it
  stacks/tour.df:104  resource net.subnet "private-${z}" { .. } where zone(z, n)
    zone("us-test-1c", n): no row
    nearest: ("us-test-1a", 1), ("us-test-1b", 2)
```

## The language

Evaluation order is never part of a program. `has x` guards that a
value exists and, like every construct, waits for one not reported yet,
so `not has c.resources.limits` finds a container without limits. A
function is pure or reads the world, and the language knows which, so
`random.password("db")` is the same every run and a read of a host
still booting waits for its tick.

A document should be a value wherever it lives. `io.read` takes a
project path or a uri, and a format's package decodes it into rows that
keep their file and line:

```dform
input peering from csv.decode(io.read("data/peerings.csv"))

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")

resource k8s.custom_resource_definition "${d.metadata.name}" = d where {
  d in yaml.decode(io.read("git+https://github.com/traefik/traefik/docs/crds.yml?ref=v3.7.14"))
}
```

Terraform's `file()` reads only files on disk at the start of a run.

Environments should differ in one file. Each value of a `key` is its own
deployment with its own state, and a `set` under a condition overrides
an input's default:

```dform
key env: enum("lab", "prod") = "lab"
input agents: int = 0 check 0 <= agents <= 3
input sizes { synapse: bytes = 20Gi, forgejo: bytes = 20Gi }
output ingress_ip: ip = k3s.ingress_ip

set { agents = 1, sizes.synapse = 100Gi } where env == "prod"
```

Terraform keeps a state per workspace and varies a configuration by
`terraform.workspace`.

A secret should not reach an output, an address or a count by accident.
The compiler follows every value made from one, and a leak is an error
at its line:

```dform
output password: string = pw                           # E0304: not declared secret(string)

deny "short password" where pw.len < 12               # E0301: inspecting it leaks it
n(c) where c = count(p), p = pw                        # E0303: a count leaks cardinality
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

State holds no secret, because a generated one derives from the
deployment's master, and `dform secrets rotate D KEY` changes one, one
plan line per place it lands. Terraform's `sensitive` keeps a value out
of its CLI output and still stores it in state.

Every deny should hold in every environment before anything ships.
`dform test` runs the program once per combination of its enums, bools
and keys, against an empty world:

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

A `terraform test` file runs the `run` blocks its author writes.

## The tool

**Stacks and deployments.** An estate should plan and apply as one, each
deployment with its own plan, question and state, in dependency order.
Which deployments exist is code. `project.df` lists them as resources,
`resource stacks.platform lab { env = "lab" }`, and a stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=lab` applies platform first. Terraform links root modules through
remote state, Terragrunt through `dependency` blocks that export a
module's outputs, and Pulumi through a `StackReference`. Argo CD's
ApplicationSet matrix generator crosses two generators' parameters into
Applications.

**apply** asks once, and again only at a tick whose plan it could not
show in full. An interrupted apply resumes where it stopped, and a
create whose answer was lost is found again, never made twice.

**destroy** is the plan against an empty program, so a deny over
deletes refuses it. Lifecycle is facts a rule can condition,
`lifecycle(k3s.server, "prevent_destroy") where env == "prod"`, and a
provider can seed a type's default lifecycle. The Tailscale provider
seeds its device's, so a device dropped from the program is let go
unless the program says otherwise. Terraform's `lifecycle` settings take
only literal values.

**status** shows the health a provider reports for each object dform
manages, `-` for a type its provider does not judge, and fails unless
all is well:

```
$ dform status app
compute.vm web     degraded   1 of 3 available
compute.vm worker  suspended  SHUTOFF
db.postgres db     healthy
net.vpc main       -
status: 1 healthy, 1 degraded, 1 suspended, 1 without health
```

Argo CD's health view uses the same words for Kubernetes resources.

**render** prints a deployment's planned documents as their provider
would send them, under the program's policy, with no provider
configured and no state. For the Kubernetes provider, `dform render
apps env=lab` is a YAML stream for Argo CD's config management plugin or
`kubectl apply -f -`.

**Approvals.** A risky change should wait for a person, and the
approval should cover exactly what applies. `requires_approval(r,
reason)` holds the change until someone signs the plan's digest, and
`apply --approval FILE` brings the signature. Atlantis's `approved`
requirement holds an apply until someone other than its author approves
the pull request.

**Providers** are written against the SDK in `docs/providers.md`, and
`dform provider check` runs the conformance suite on one;
`docs/reference.md` has every command and flag.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, then a second run | it plans in the next tick |
| how many rounds an apply takes | find out during it | the plan is grouped by tick |
| a cluster and what runs on it, in one plan | two root modules | `use k8s { kubeconfig = k3s.kubeconfig }` |
| a tag on everything, overridable | a variable threaded through every module | `set r.tags.team = "shop" @default where r in resource` |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| rules about the change set | plan JSON through a policy engine | denies over `deformation` rows; the plan says what holds, fails and cannot be known yet |
| "why is this here?" | read the source | every plan line cites its file and line; `dform why ADDR` |
| "why is this not here?" | read the source harder | `dform why` names the condition that failed |
| routes from reachability | write them out by hand | a recursive rule |
| policies tested over every environment | one test per case | `dform test` runs every combination of enums, bools and keys |
| one generated password rotated | taint and hope nothing else moves | `dform secrets rotate D KEY` |
| a state file that leaks nothing | encrypt the bucket | state holds no secret |
| a key the cloud mints, written into another resource | it lands in state in the clear | the provider holds it; dform reveals it into the one call that writes it |
| health after a deploy | apply waits on it, or a second tool | `dform status`, across every provider that reports it |
| manifests for GitOps, under the program's policy | a template engine beside the infrastructure tool | `dform render`: no provider, no state |

## What it is not

- Not a general-purpose language. There are no loops, no mutation and
  no effects.
- Not a configuration manager. It runs nothing on a host; cloud-init in
  `user_data` and the cluster's own controllers do that.
- Not a Helm. A manifest of one kind is a resource per document; a
  chart's render of mixed kinds is not read by kind.
- Not a proof. `dform test` enumerates enums, bools and keys and leaves
  every other input at its default.
- Not finished. It is pre-release. The language changes without
  compatibility, and every `.df` here is rewritten when it does. Few
  providers are real yet. Controller mode, `dform controller run`, is
  experimental, behind `DFORM_EXPERIMENTAL=1`.

## Where next

- `examples/tour`: the tutorial. `examples/crud-api`: a blue/green
  rollout gated on a migration Job. Each example's README says what it
  shows and the commands to run.
- `docs/grammar.md`: the language. `docs/reference.md`: every command
  and how to run dform. `docs/best_practices.md`: how to write a
  program.
- `proposals/` and `DESIGN.org`: the model and the decisions.

Apache-2.0. Contributions under the Developer Certificate of Origin
(`git commit -s`); there is no CLA.
