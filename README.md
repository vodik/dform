# dform

Infrastructure should be a program its operators can read, planned over
the world as it is. Every line of the plan should say what produced it.
Policy should live beside what it governs and be checked by every plan.
A value the cloud knows only later should be a value the plan already
waits for. The program should be as clear to the language model helping
an operator as to the operator.

It can, because infrastructure is a database. An account is a table of
networks, a table of instances, a table of DNS records, each row with
attributes and pointing at others. What the program wants is a set of
tables too, and a plan is the difference between the two. A policy is a
query that must return no rows, and "why is this here" asks which rows
produced it. A dform program is facts and rules over those tables,
evaluated the way Datalog evaluates a query, so every plan is the least
model of the program over the world as it is and every line of it
carries its proof. Apply reconciles the world with the plan in ticks,
and what one tick creates, an endpoint or a kubeconfig, is a value the
next tick plans with.

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

When the region gains a zone, the next plan has one more subnet and
the file does not change, because `aws.availability_zone` is a table
backed by the provider. `policy  1 hold` is the deny, checked and
holding.

## Describe what should exist

A dform file says what should exist and under which conditions, and
dform works out how many, in what order, and when. The file itself has
no order: a block may use a name declared further down or in another
file. A block holds for every answer to its clause, so repetition is a
condition, an edge is a reference, and a value known later is planned
later. Terraform makes a block a template and adds `for_each`,
`depends_on`, `-target` and `default_tags` for what a template cannot
say.

For example, we can use dform to describe a rough network layout and
derive and validate the topology from that description. Add a spoke and
its peering follows; peer two spokes by hand and the policy refuses the
plan:

```dform
use aws { region = "us-east-1" }

resource aws.vpc core { cidr_block = "10.0.0.0/16", tags = { Name: "core" } }
resource aws.vpc shop { cidr_block = "10.1.0.0/16", tags = { Name: "shop" } }
resource aws.vpc data { cidr_block = "10.2.0.0/16", tags = { Name: "data" } }

hub(core)
spoke(shop)
spoke(data)
link(from, to) where hub(from), spoke(to)

resource aws.vpc_peering_connection "${from.tags.Name}-${to.tags.Name}" {
  vpc_id = from
  peer_vpc_id = to
} where link(from, to)

deny "a spoke peers only the core" { peering } where {
  peering in aws.vpc_peering_connection
  spoke(peering.vpc_id)
  spoke(peering.peer_vpc_id)
}
```

```
$ dform plan
plan: 5 changes (5 create) over 1 tick; policy: 1 hold
...
  + aws.vpc_peering_connection core-data  stacks/net.df:12
      peer_vpc_id = data
      vpc_id = core
  + aws.vpc_peering_connection core-shop  stacks/net.df:12
      peer_vpc_id = shop
      vpc_id = core

policy  1 hold
```

In Terraform the peerings are a `for_each` over a `setproduct`, and
nothing refuses one added by hand.

For example, we may want to keep the networks in a toml list, or an
environment's inputs in yaml:

```dform
key env: enum("staging", "prod") = "staging"
input database { backup_days: int = 1 }
use aws { region = "us-east-1" }

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

resource aws.vpc "${v.name}" { cidr_block = v.cidr } where v in net.vpcs

resource aws.db_instance orders {
  instance_class = "db.t3.micro"
  backup_retention_period = database.backup_days
}
```

```
$ dform plan
deployment: stacks.app[env=staging]
plan: 3 changes (3 create) over 1 tick

tick 1  3 changes
  + aws.db_instance orders         stacks/app.df:10
      backup_retention_period = 7  stacks/app.df:6
      instance_class = "db.t3.micro"
  + aws.vpc data                   stacks/app.df:8  with v = {cidr: "10.2.0.0/16", name: "data"}
      cidr_block = "10.2.0.0/16"
  + aws.vpc shop                   stacks/app.df:8  with v = {cidr: "10.1.0.0/16", name: "shop"}
      cidr_block = "10.1.0.0/16"
```

**Policy is part of the language.** The shape every resource should have
and the changes that may not happen are both rules in the same file as
the resources, and every plan checks them. For example, every resource
should carry the team's tag, whatever its type, and nobody should have
to remember to add it:

```dform
resource aws.vpc main { cidr_block = "10.0.0.0/16", tags = { component: "network" } }

set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'main.tags'
{ component: "network", team: "shop" }
```

The plan is a table too. For example, after an apply we can ask what the
next plan would change, as rows of `deformation`:

```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=14
Kind      Resource
"update"  aws.db_instance orders
```

"No deletes in prod" is `deny "no deletes in prod" where env == "prod",
deformation("delete", _, _)`, and it refuses the apply. A policy over a
value known only after apply stays undetermined, and the plan says when
it will know. A risky change can instead wait for a signature over
exactly what will apply. With Terraform, policy is a second tool and a
second language over the plan's JSON, where such a value is only marked
unknown.

**Lifecycle is a table too.** For example, a prod database must never be
deleted, and a policy can check that every one of them is protected:

```dform
lifecycle(orders, "prevent_destroy") where env == "prod"

deny "every prod database is kept" { db } where {
  env == "prod"
  db in aws.db_instance
  not lifecycle(db, "prevent_destroy")
}
```

Terraform's `prevent_destroy` must be a literal.

**Secrets are part of the language too.** A secret cannot reach an
output, a condition or an address by accident; a leak is an error at its
line:

```dform
let pw = random.password("db")

output password: string = pw                           # E0304: not declared secret(string)
deny "short password" where pw.len < 12                # E0301: inspecting it leaks it
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

State holds no secret, because a generated one derives from the
deployment's master. `dform secrets` imports, lists and rotates them.
Terraform's `sensitive` keeps a value out of its CLI output and still
stores it in state.

A value the cloud has not produced yet is planned around, even in a
resource's name:

```dform
use aws { region = "us-east-1" }

resource aws.db_instance orders { instance_class = "db.t3.micro" }

resource aws.iam_policy "connect-${id}" {
  policy = json.encode(
    { Statement: [{
      Action: "rds-db:connect",
      Resource: "arn:aws:rds-db:us-east-1:*:dbuser:${id}/app",
    }] },
  )
} where pg in aws.db_instance, id = pg.resource_id
```

```
$ dform plan
plan: 1 change (1 create) over 2 ticks

tick 1  1 change
  + aws.db_instance orders        stacks/db.df:3
      instance_class = "db.t3.micro"

tick 2  ? changes
  waits on  orders.resource_id
  aws.iam_policy "connect-${id}"  stacks/db.df:5  waits on orders.resource_id
```

Terraform cannot plan a resource address from a value it learns at
apply; it asks for a `-target` and a second run.

For example, a host, the cluster installed on it and what runs in that
cluster can be one program and one plan, with the cluster's provider
configured from the host's address once it exists:

```dform
use aws { region = "us-east-1" }

let k3s_init = "#!/bin/sh\ncurl -sfL https://get.k3s.io | sh -\n"

resource aws.instance server { instance_type = "t3.small", user_data = k3s_init }

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")
let kubeconfig = str.replace(raw, "https://127.0.0.1:6443", "https://${server.public_ip}:6443")
use k8s { kubeconfig }

resource k8s.namespace apps { metadata.name = "apps" }
```

```
$ dform plan
plan: 2 changes (2 create) over 2 ticks

tick 1  1 change
  + aws.instance server  stacks/cluster.df:5
      instance_type = "t3.small"
      user_data = "#!/bin/sh\ncurl -sfL https://get.k3s.io | sh -\n"  stacks/cluster.df:3

tick 2  1 change
  waits on  provider k8s  kubeconfig
  + k8s.namespace apps   stacks/cluster.df:11
      metadata.name = "apps"
```

The Kubernetes provider's own documentation asks for the cluster and
what runs on it in separate applies.

A provider is facts as well: its types, its tables and its defaults are
rows the program reads. For example, a plan can refuse to write records
into a domain the account does not host:

```dform
deny "the domain is hosted here" where not ovh.zone("example.com", _, _)
```

For example, staging and prod can differ by one `set`, and `dform test`
runs every policy over every combination of enums and bools:

```dform
key env: enum("staging", "prod") = "staging"
input agents: int = 0 check 0 <= agents <= 3
input public: bool = false
use aws { region = "us-east-1" }

set agents = 2 where env == "prod"

resource aws.db_instance orders { instance_class = "db.t3.micro", publicly_accessible = public }
resource aws.instance "agent-${n}" { instance_type = "t3.small" } where n in 0..agents

deny "no public database in prod" where env == "prod", public
```

```
$ dform test
test app: 4 combinations of env, public
env      public  result
staging  false   ok
staging  true    ok
prod     false   ok
prod     true    denied
denied  dform plan app env=prod --set public=true
  - no public database in prod
test app: 4 combinations, 1 failed
```

`dform why` answers for a resource that does not exist, naming the rule
that could have made it and the row it lacked:

```
$ dform why 'aws.subnet private-us-east-1d'
aws.subnet private-us-east-1d: no rule derives it
  stacks/shop.df:8  resource aws.subnet "private-${availability_zone}" { .. } where aws.availability_zone("available", availability_zone, n)
    aws.availability_zone("available", "us-east-1d", n): no row
    nearest: ("us-east-1a", 0), ("us-east-1b", 1), ("us-east-1c", 2)
```

## The work

Day to day it is `plan` and `apply`. Apply asks before anything
changes and again only when a later plan differs from the one shown;
interrupted, it resumes where it stopped.

**Stacks and deployments.** An estate plans and applies as one, each
deployment with its own plan, approval and state, in dependency order.
Which deployments exist is code: `project.df` lists them as resources,
`resource stacks.platform prod { env = "prod" }`. A stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=prod` applies platform first. Terragrunt needs a `dependency` block
per edge and a directory per environment.

In a pipeline, `status` fails unless everything is healthy:

```
$ dform status apps env=staging
k8s.deployment web  degraded  CrashLoopBackOff: container web
k8s.namespace apps  -
status: 1 degraded, 1 without health
```

`render` prints what apply would send, policy checked, with no
credentials and no state, as plain manifests for Argo CD or kustomize:

```
$ dform render apps env=staging
---
apiVersion: v1
kind: Namespace
metadata:
  name: apps
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: apps
...
$ dform render apps env=staging > manifests/apps.yaml
$ kustomize build manifests | kubectl apply -f -
```
