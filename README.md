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

When the region gains a zone, the next plan has one more subnet and
the file does not change, because `aws.availability_zone` is a table
backed by the provider. `policy  1 hold` is the deny, checked and
holding.

## Describe what should exist

A dform file says what should exist and under which conditions, and
dform works out how many, in what order, and when. The file itself has
no order: a block may use a name declared further down or in another
file. A block holds for every answer to its clause, so repetition is a
condition, an edge is a reference, and a value known later is simply
later. Terraform makes a block a template and adds `for_each`,
`depends_on`, `-target` and `default_tags` for what a template cannot
say.

For example, we can use dform to describe a rough network layout and
derive and validate the topology from that description:

```dform
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

reaches(a, b) where link(a, b)
reaches(b, a) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)

deny "every spoke reaches core" { vpc } where spoke(vpc), not reaches(vpc, core)
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

Terraform has no recursion; a hub and its spokes are a list kept by
hand.

**Policy is part of the language.** The shape every resource should
have and the changes that may not happen are both rules in the same file
as the resources, checked by every plan. `resource` is the table of every
resource the program declares, whatever its type, and a `set` over it
writes into each one its clause matches, leaf by leaf, beside what the
resource's own block wrote:

```dform
resource aws.vpc main { cidr_block = "10.0.0.0/16", tags = { component: "network" } }

set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'main.tags'
{ component: "network", team: "shop" }
```

Data files are facts. A toml names the networks, a yaml holds an
environment's settings, and both are read like any other table:

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
deployment: stacks.shop[env=staging]
plan: 3 changes (3 create) over 1 tick

tick 1  3 changes
  + aws.db_instance orders         stacks/shop.df:10
      backup_retention_period = 7  stacks/shop.df:6
      instance_class = "db.t3.micro"
  + aws.vpc data                   stacks/shop.df:8  with v = {cidr: "10.2.0.0/16", name: "data"}
      cidr_block = "10.2.0.0/16"
  + aws.vpc shop                   stacks/shop.df:8  with v = {cidr: "10.1.0.0/16", name: "shop"}
      cidr_block = "10.1.0.0/16"
```

**The plan is a table too.** Every change it would make is a
`deformation` row, the kind of change and the resource it touches, so a
policy asks about the change and the resource's configuration in one
question:

```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=14
Kind      Resource
"update"  aws.db_instance orders
```

"No deletes in prod" is `deny "no deletes in prod" where env ==
"prod", deformation("delete", _, _)`, checked by every plan, and it
refuses the apply. A policy over a value known only after apply is
undetermined, since nothing is assumed false for being unknown, and the
plan says when it will be known. A risky change waits instead for a
signature over exactly what will apply. With Terraform, policy is a
second tool and a second language over the plan's JSON, where such a
value is only marked unknown.

**Lifecycle is a table too.** A rule writes it, a provider defaults it
for its own types, and a policy reads it back. Terraform's
`prevent_destroy` must be a literal:

```dform
lifecycle(orders, "prevent_destroy") where env == "prod"

deny "every prod database is kept" { db } where {
  env == "prod"
  db in aws.db_instance
  not lifecycle(db, "prevent_destroy")
}
```

**Secrets are part of the language too.** A secret cannot reach an
output, an address or a count by accident. The compiler follows every
value made from one, and a leak is an error at its line:

```dform
let pw = random.password("db")

output password: string = pw                           # E0304: not declared secret(string)
deny "short password" where pw.len < 12                # E0301: inspecting it leaks it
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

State holds no secret, because a generated one derives from the
deployment's master. Secrets are imported, managed and rotated with
`dform secrets`. Terraform's `sensitive` keeps a value out of its CLI
output and still stores it in state.

Resources are wired by reference, and a value the cloud has not
produced yet is tracked and planned around:

```dform
resource aws.db_instance orders { instance_class = "db.t3.micro" }

resource aws.iam_policy "connect-${host}" {
  policy = json.encode({ Statement: [{ Action: "rds-db:connect", Resource: host }] })
} where pg in aws.db_instance, host = pg.address
```

```
$ dform plan
plan: 1 change (1 create) over 2 ticks

tick 1  1 change
  + aws.db_instance orders          stacks/db.df:3
      instance_class = "db.t3.micro"

tick 2  ? changes
  waits on  orders.address
  aws.iam_policy "connect-${host}"  stacks/db.df:5  waits on orders.address
```

Terraform stops here and asks for a `-target` and a second run.

A provider can be configured from the plan's own values, so a host, the
cluster on it and what runs in the cluster are one program and one
plan:

```dform
use aws { region = "us-east-1" }

let k3s_init = "#!/bin/sh\ncurl -sfL https://get.k3s.io | sh -\n"

resource aws.instance server { instance_type = "t3.small", user_data = k3s_init }

let kc: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")
use k8s { kubeconfig = kc }

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
  waits on  provider k8s  kubeconfig = kc
  provisional: planned against the offline schema; planned again once kubeconfig is known
  + k8s.namespace apps   stacks/cluster.df:10
      metadata.name = "apps"
```

A provider is facts as well. Its types, its tables and its defaults are
rows the program reads, so "is this zone on the account" is one line:

```dform
deny "example.com is not on this account" where not ovh.zone("example.com", _, _)
```

Environments are one file: each value of a key is its own deployment,
and `dform test` runs every policy over every combination of the
program's enums and bools:

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

**Everything explains itself, absence included.** A missing resource
has an answer as precise as a present one. `dform why` names the rule
that could have made it and the condition that failed:

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
deployment with its own plan, question and state, in dependency order.
Which deployments exist is code. `project.df` lists them as resources,
`resource stacks.platform prod { env = "prod" }`, and a stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=prod` applies platform first. Terragrunt needs a `dependency` block
per edge and a directory per environment.

For a pipeline, `status` says whether everything is healthy and fails
if not, and `render` prints what would be sent, under policy, with no
credentials and no state, a stream Argo CD or kustomize reads as plain
manifests:

```
$ dform status apps env=staging
k8s.deployment web  degraded  CrashLoopBackOff: container web
k8s.namespace apps  -
status: 1 degraded, 1 without health
```

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
