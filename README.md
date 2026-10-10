# dform

Infrastructure should be a program its operators can read, planned over
the world as it is. Every line of the plan should say what produced it.
Policy should live beside what it governs and be checked by every plan.
A value the cloud knows only later should be a value the plan already
waits for. The program should be as clear to the language model helping
an operator as to the operator.

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
plans with.

A complete program:

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

When the region gains a zone, the next plan has one more subnet and the
file does not change, because `aws.availability_zone` is a table backed
by the provider. `policy 1 hold` is the deny, checked and holding.

## Describe what should exist

A dform file says what should exist and under which conditions, and
dform works out how many, in what order, and when. The file itself has
no order either: a block may use a name declared further down or in
another file. A block is a description that holds for every answer to
its clause, so repetition is a condition, an edge is a reference, and a
value known later is a later tick. Terraform makes a block a template
and adds `for_each`, `depends_on`, `-target` and `default_tags` for what
a template cannot say.

**Policy is part of the language.** The shape every resource should have
and the changes that may not happen are both rules in the same file as
the resources, checked by every plan. `resource` is the table of every
resource the program declares, whatever its type, and a `set` over it
writes into each one its clause matches, leaf by leaf, beside what the
resource's own block wrote:

```dform
set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'net.vpc["blue.vpc"].tags'
{ team: "shop", component: "network" }
```

**The plan is a table too.** Every change it would make is a
`deformation` row, the kind of change and the resource it touches, so a
policy asks about the change and the resource's configuration in one
question:

```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=7
Kind      Resource
"update"  db.postgres orders
```

"No deletes in prod" is `deny "no deletes in prod" where env == "prod",
deformation("delete", _, _)`, checked by every plan, and it refuses the
apply. A policy over a value known only after apply is undetermined,
since nothing is assumed false for being unknown, and the plan says when
it will be known. A risky change waits instead for a signature over
exactly what will apply. With Terraform, policy is a second tool and a
second language over the plan's JSON, where such a value is only marked
unknown.

**Lifecycle is a table too.** A rule writes it, a provider defaults it
for its own types, and a policy reads it back. Terraform's
`prevent_destroy` must be a literal:

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
deployment's master. Secrets are imported, managed and rotated with
`dform secrets`. Terraform's `sensitive` keeps a value out of its CLI
output and still stores it in state.

**Descriptions build on each other.** Anything shaped like a graph is
derived, so reachability stays right as spokes come and go, and this is
where the descriptions are rules in the Datalog sense:

```dform
use fake

resource net.vpc core { name = "core", cidr = "10.0.0.0/16" }
resource net.vpc shop { name = "shop", cidr = "10.1.0.0/16" }
resource net.vpc data { name = "data", cidr = "10.2.0.0/16" }

hub(core)
spoke(shop)
spoke(data)

link(from, to) where hub(from), spoke(to)

resource net.peering "${from.name}-${to.name}" { from, to } where link(from, to)

reaches(from, to) where link(from, to)
reaches(to, from) where link(from, to)
reaches(from, to) where reaches(from, via), link(via, to)

deny "every spoke reaches core" { spoke } where spoke(spoke), not reaches(spoke, core)
```

```
$ dform plan
plan: 5 changes (5 create) over 1 tick; policy: 1 hold

tick 1  5 changes
  + net.vpc core           stacks/net.df:3
      cidr = "10.0.0.0/16"
      name = "core"
  + net.vpc data           stacks/net.df:5
      cidr = "10.2.0.0/16"
      name = "data"
  + net.vpc shop           stacks/net.df:4
      cidr = "10.1.0.0/16"
      name = "shop"
  + net.peering core-data  stacks/net.df:13  with from = net.vpc core, to = net.vpc data
      from = core
      to = data
  + net.peering core-shop  stacks/net.df:13  with from = net.vpc core, to = net.vpc shop
      from = core
      to = shop

policy  1 hold
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
key env: enum("lab", "prod") = "lab"
input database { backup_days: int = 1 }
use fake

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

resource net.vpc "${v.name}" { cidr = v.cidr } where v in net.vpcs
resource db.postgres orders { backup_days = database.backup_days }
```

```
$ dform plan
deployment: stacks.shop[env=lab]
plan: 3 changes (3 create) over 1 tick

tick 1  3 changes
  + db.postgres orders  stacks/shop.df:9
      backup_days = 7   stacks/shop.df:6
  + net.vpc data        stacks/shop.df:8  with v = {cidr: "10.2.0.0/16", name: "data"}
      cidr = "10.2.0.0/16"
  + net.vpc shop        stacks/shop.df:8  with v = {cidr: "10.1.0.0/16", name: "shop"}
      cidr = "10.1.0.0/16"
```

An object spreads into another, so a rendered config is a shared base
and what differs:

```dform
input region: string = "eu-west-1"
use k8s

let common = { log_level: "info", region }

resource k8s.namespace apps { metadata.name = "apps" }

resource k8s.config_map app {
  metadata = { name: "app", namespace: apps.metadata.name }
  data = { "config.json": json.encode({ ..common, replicas: 3 }) }
}
```

```
$ dform plan
...
  + k8s.config_map app  stacks/app.df:8
      data."config.json" = "{\"log_level\":\"info\",\"region\":\"eu-west-1\",\"replicas\":3}"
      metadata = { name: "app", namespace: "apps" }
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

Day to day it is `plan` and `apply`. Apply asks before anything changes
and again only when a later plan differs from the one shown;
interrupted, it resumes where it stopped.

**Stacks and deployments.** An estate plans and applies as one, each
deployment with its own plan, question and state, in dependency order.
Which deployments exist is code. `project.df` lists them as resources,
`resource stacks.platform lab { env = "lab" }`, and a stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=lab` applies platform first. Terragrunt needs a `dependency` block
per edge and a directory per environment.

For a pipeline, `status` says whether everything is healthy and fails if
not, and `render` prints what would be sent, under policy, with no
credentials and no state, a stream Argo CD or kustomize reads as plain
manifests:

```
$ dform status app
compute.vm web     degraded   1 of 3 available
compute.vm worker  suspended  SHUTOFF
db.postgres db     healthy
net.vpc main       -
status: 1 healthy, 1 degraded, 1 suspended, 1 without health
```

```
$ dform render apps env=lab
---
apiVersion: v1
kind: Namespace
metadata:
  name: apps
---
apiVersion: apps/v1
kind: Deployment
...
$ dform render apps env=lab > manifests/apps.yaml
$ kustomize build manifests | kubectl apply -f -
```
