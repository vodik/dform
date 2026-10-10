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

A complete program, on the AWS-shaped mock:

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

deny "no public subnets" { subnet } where subnet in aws.subnet, subnet.map_public_ip_on_launch
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

When the region gains a zone, the next plan has one more subnet and
the file does not change: `aws.availability_zone` is a relation the
provider answers, and a block ending in `where` makes one resource per
answer. The examples run on a fake cloud built into dform with
no credentials. To try one, `cargo install --path .`, then `dform -C
examples/tour plan`.

## Describe what should exist

A dform file says what should exist and under which conditions, and
dform works out how many, in what order, and when. The file itself has
no order either: a block may use a name declared further down or in
another file, and a function is pure or reads the world, so the language
knows what to wait for. A block is a description that holds for every
answer to its clause, so repetition is a condition, an edge is a
reference, and a value known later is a later tick. Terraform makes a
block a template and adds `for_each`, `depends_on`, `-target` and
`default_tags` for what a template cannot say.

**Policy is part of the language.** Conformance has two halves, the
shape every resource should have and the changes that may not happen,
and both are rules in the same file as the resources, in the same
language, run by every plan and every apply. The shape is a `set` that
applies everywhere. `resource` is the table of every resource the
program declares, whatever its type, and a `set` over it writes into
each one its clause matches, leaf by leaf, beside what the resource's
own block wrote:

```dform
set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'net.vpc["blue.vpc"].tags'
{ team: "shop", component: "network" }
```

**The plan is a table too.** Every change the plan would make is a
`deformation` row, the kind of change and the resource it touches, so a
policy can ask about the change and the resource's configuration in one
question: no deleting a database in prod, no replacing a volume that
holds data. What the plan would do, as rows:

```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=7
Kind      Resource
"update"  db.postgres orders
```

**What may change is a rule too.** "No deletes in prod" is `deny "no
deletes in prod" where env == "prod", deformation("delete", _, _)`, and
it is checked by every plan and refuses the apply. A risky change waits
instead for a signature over exactly what will apply. A policy over a
value known only after apply says so: the logic has three values,
nothing is assumed false for being unknown, so such a policy is
undetermined and the plan says when it will be known. With Terraform,
policy is a second tool and a second language over the plan's JSON,
where such a value is only marked unknown.

**Lifecycle is a table too.** One row per resource and word:
`prevent_destroy`, `retain`, `create_first`, `bootstrap`. The program
writes rows, a provider seeds defaults for its own types, and the plan
reads them, so lifecycle is decided by a rule like anything else, and a
policy can read it back:

```dform
lifecycle(k3s.server, "prevent_destroy") where env == "prod"

deny "every prod database is kept" { db } where {
  env == "prod"
  db in db.postgres
  not lifecycle(db, "prevent_destroy")
}
```

**Secrets are part of the language too.** A secret cannot reach an
output, an address or a count by accident. The compiler follows every
value made from one, and a leak is an error at its line:

```dform
output password: string = pw                           # E0304: not declared secret(string)

deny "short password" where pw.len < 12                # E0301: inspecting it leaks it
n(c) where c = count(p), p = pw                        # E0303: a count leaks cardinality
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

State holds no secret, because a generated one derives from the
deployment's master, and `dform secrets rotate D KEY` changes one, one
plan line per place it lands. Terraform's `sensitive` keeps a value out
of its CLI output and still stores it in state.

**Descriptions build on each other.** Anything shaped like a graph is
derived, so reachability stays right as spokes come and go, and this is
where the descriptions are rules in the Datalog sense:

```dform
link(h, t) where hub(h), spoke(t)
link(t, h) where hub(h), spoke(t)

reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)
```

Resources are wired by reference, and a value the cloud has not
produced yet is tracked and planned around:

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

Terraform stops here and asks for a `-target` and a second run.

A provider can be configured from the plan's own values, so a host, the
cluster on it and what runs in the cluster are one program and one plan.

A provider is facts as well. Its types, its tables and its defaults are
rows the program reads, so "is this zone on the account" is one line:

```dform
deny "example.com is not on this account" where not ovh.zone("example.com", _, _)
```

Environments are one file, and every policy is checked over all of them
before anything ships:

```dform
key env: enum("lab", "prod") = "lab"
input agents: int = 0 check 0 <= agents <= 3
input sizes { synapse: bytes = 20Gi, forgejo: bytes = 20Gi }
output ingress_ip: ip = k3s.ingress_ip

set { agents = 1, sizes.synapse = 100Gi } where env == "prod"
```

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

A document is a value and a value is a document:

```dform
input peering from csv.decode(io.read("data/peerings.csv"))

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")

resource k8s.custom_resource_definition "${d.metadata.name}" = d where {
  d in yaml.decode(io.read("git+https://github.com/traefik/traefik/docs/crds.yml?ref=v3.7.14"))
}
```

**Everything explains itself, absence included.** A missing resource
has an answer as precise as a present one. `dform why` names
the rule that could have made it and the condition that failed:

```
$ dform why 'net.subnet private-us-test-1c'
net.subnet private-us-test-1c: no rule derives it
  stacks/tour.df:104  resource net.subnet "private-${z}" { .. } where zone(z, n)
    zone("us-test-1c", n): no row
    nearest: ("us-test-1a", 1), ("us-test-1b", 2)
```

## The work

Day to day it is `plan` and `apply`. Apply asks once, resumes where it
was interrupted, and never makes the same thing twice.

**Stacks and deployments.** An estate plans and applies as one, each
deployment with its own plan, question and state, in dependency order.
Which deployments exist is code. `project.df` lists them as resources,
`resource stacks.platform lab { env = "lab" }`, and a stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=lab` applies platform first. Terragrunt wires this with `dependency`
blocks, and Argo CD's ApplicationSet matrix generator does the matrix
for Applications.

For a pipeline, `render` prints what would be sent, under policy, with
no credentials and no state, and `status` says whether everything is
healthy and fails if not:

```
$ dform status app
compute.vm web     degraded   1 of 3 available
compute.vm worker  suspended  SHUTOFF
db.postgres db     healthy
net.vpc main       -
status: 1 healthy, 1 degraded, 1 suspended, 1 without health
```
