# dform

Infrastructure should be a program its operators can read, planned over
the world as it is. Every line of the plan should say what produced it.
Policy should live beside what it governs, and every plan should check
it. We should be able to plan with a value the cloud has not produced
yet. The program should be as clear to the language model helping an
operator as to the operator.

It can, if we treat infrastructure like a database. An account is a
table of networks, a table of instances, a table of DNS records, each
row pointing at others. What we want is a set of tables too, and a plan
is the difference between the two. A policy is a query that must return
no rows, and we answer "why is this here" by tracking the rows that
produced it.

A complete program. It refuses to plan against any other account:

<!-- examples/shop/stacks/shop.df:5-18 -->
```dform
use aws { region = "us-east-1", expect_account = "123456789012" }

resource aws.vpc main { cidr_block = "10.0.0.0/16" }

let cidr(n) = inet.subnet(main.cidr_block, 8, n)

#| One private subnet in every available zone of the region.
resource aws.subnet "private-${availability_zone}" {
  vpc = main
  cidr_block = cidr(n)
  availability_zone
} where aws.availability_zone("available", availability_zone, n)

deny "no public subnets" { subnet } where subnet in aws.subnet, subnet.map_public_ip_on_launch
```

<!-- run in examples/shop -->
```
$ dform plan
deployment: stacks.shop[env=staging]
plan: 12 changes (12 create) over 2 ticks; policy: 4 hold

tick 1  11 changes
  + aws.vpc main                      stacks/shop.df:7
      cidr_block = "10.0.0.0/16"
      tags.team = "shop"              policy.df:5
...
  + aws.subnet private-us-east-1a     stacks/shop.df:12  with n = 0
      availability_zone = "us-east-1a"
      cidr_block = "10.0.0.0/24"
      tags.team = "shop"              policy.df:5
      vpc = main
  + aws.subnet private-us-east-1b     stacks/shop.df:12  with n = 1
      availability_zone = "us-east-1b"
      cidr_block = "10.0.1.0/24"
      tags.team = "shop"              policy.df:5
      vpc = main
  + aws.subnet private-us-east-1c     stacks/shop.df:12  with n = 2
      availability_zone = "us-east-1c"
      cidr_block = "10.0.2.0/24"
      tags.team = "shop"              policy.df:5
      vpc = main
...
policy  4 hold
```

When the region gains a zone, the next plan has one more subnet and the
file does not change, because `aws.availability_zone` is a table backed
by the provider. Each row says which answer produced it: `with n = 0`. A
value set from another line carries that line. `policy 1 hold` is the
deny, checked and holding. Type annotations are optional: dform infers
types from usage, dependencies and order from references, and the number
of subnets from the table.

## Describe what should exist

A dform file says what should exist and under which conditions, and
dform works out how many, in what order, and when. The file itself has
no order: a block may use a name declared further down or in another
file. A block holds for every answer to its clause, so repetition is a
condition, an edge is a reference, and dform plans a value known later,
later.

For example, we can use dform to describe a rough network layout and
derive and validate the topology from that description. Add a spoke and
its peering follows; peer two spokes by hand and the policy refuses the
plan:

<!-- examples/shared-network/stacks/net.df:26-39 -->
```dform
#| The core: the services every team reaches, the hub of the spokes.
resource aws.vpc core { cidr_block = inet.subnet(supernet, 4, 0), tags = { Name: "core" } }

hub(core)
spoke(vpc) where s in net.spokes, vpc in aws.vpc, vpc == network[s.team].vpc
link(from, to) where hub(from), spoke(to)

resource aws.vpc_peering_connection "${from.tags.Name}-${to.tags.Name}" {
  vpc = from
  peer_vpc = to
} where link(from, to)

deny "a spoke peers only the core" { peering }
  where peering in aws.vpc_peering_connection, spoke(peering.vpc), spoke(peering.peer_vpc)
```

<!-- run in examples/shared-network -->
```
$ dform plan net
deployment: stacks.net[env=staging]
plan: 11 changes (11 create) over 1 tick; policy: 2 hold
...
  + aws.vpc_peering_connection core-data  stacks/net.df:33
      peer_vpc = data.vpc
      vpc = core
  + aws.vpc_peering_connection core-shop  stacks/net.df:33
      peer_vpc = shop.vpc
      vpc = core

policy  2 hold
```

For example, a network and its subnets as one unit, stamped twice:

<!-- examples/shared-network/stacks/net.df:8-24 -->
```dform
resource network "${s.team}" {
  team = s.team
  cidr = inet.subnet(supernet, 4, s.slot)
} where s in net.spokes

component network {
  input team: string
  input cidr: inet
  output vpc: aws.vpc = vpc

  resource aws.vpc vpc { cidr_block = cidr, tags = { Name: team, team } }
  resource aws.subnet "private-${zone}" {
    vpc
    cidr_block = inet.subnet(cidr, 8, n)
    availability_zone = zone
  } where aws.availability_zone("available", zone, n)
}
```

For example, we may want to keep the networks in a toml list, or an
environment's inputs in yaml:

<!-- examples/shared-network/stacks/net.df:1-11 -->
```dform
key env: enum("staging", "prod") = "staging"
input supernet: inet
use aws { region = "us-east-1", expect_account = "123456789012" }

let net = toml.decode(io.read("data/networks.toml"))
set from yaml.decode(io.read("config/net/${env}.yaml"))

resource network "${s.team}" {
  team = s.team
  cidr = inet.subnet(supernet, 4, s.slot)
} where s in net.spokes
```

<!-- run in examples/shared-network -->
```
$ dform plan net env=prod
deployment: stacks.net[env=prod]
plan: 11 changes (11 create) over 1 tick; policy: 2 hold

tick 1  11 changes
  + aws.vpc core                          stacks/net.df:27
      cidr_block = "10.0.0.0/16"
      tags.Name = "core"
  + network data
    + aws.vpc data.vpc                    stacks/net.df:18
        cidr_block = "10.2.0.0/16"        stacks/net.df:10
        tags = { Name: "data", team: "data" }
...
```

We often need a value the cloud has not produced yet, sometimes in a
resource's name:

<!-- examples/shop/stacks/shop.df:30-49 -->
```dform
resource aws.db_instance orders {
  identifier = "orders-${env}"
  instance_class = database.class
  multi_az = database.multi_az
  backup_retention_period = database.backup_days
  publicly_accessible = public
  password = random.password("orders")
  vpc_security_groups = [postgres]
}

#| The app connects to the database as `app` with its role, not a
#| password: one policy per database, named for its resource id.
resource aws.iam_policy "connect-${id}" {
  policy = json.encode({
    Statement: [{
      Action: "rds-db:connect",
      Resource: "arn:aws:rds-db:us-east-1:*:dbuser:${id}/app",
    }],
  })
} where pg in aws.db_instance, id = pg.resource_id
```

<!-- run in examples/shop -->
```
$ dform plan
deployment: stacks.shop[env=staging]
plan: 12 changes (12 create) over 2 ticks; policy: 4 hold
...
  + aws.db_instance orders            stacks/shop.df:30
...
tick 2  1+ changes
  waits on  orders.resource_id
...
  aws.iam_policy "connect-${id}"      stacks/shop.df:42  waits on orders.resource_id
...
```

Terraform cannot plan a resource address from a value it learns at
apply; it asks for a `-target` and a second run.

For example, we want a host, the cluster installed on it and what runs
in that cluster to be one program and one plan, with the cluster's
provider configured from the host's address once it exists:

<!-- examples/k3s-platform/stacks/platform.df:22-37 -->
```dform
let k3s_init = "#!/bin/sh\ncurl -sfL https://get.k3s.io | sh -\n"

resource ovh.instance server {
  name = "k3s-${env}"
  region = "BHS5"
  flavor
  image = "Ubuntu 24.04"
  ssh_key = admin
  user_data = k3s_init
}

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")
let kubeconfig = str.replace(raw, "https://127.0.0.1:6443", "https://${server.public_ip}:6443")
use k8s { kubeconfig }

resource k8s.namespace apps { metadata.name = "apps" }
```

<!-- run in examples/k3s-platform -->
```
$ dform plan platform
deployment: stacks.platform[env=staging]
plan: 4 changes (4 create) over 2 ticks; policy: 3 hold

tick 1  2 changes
  + ovh.ssh_key admin          stacks/platform.df:17
      name = "k3s-staging-admin"
      public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NT…q3m7cJ4yQx1aB ops@example.com"
  + ovh.instance server        stacks/platform.df:24
      flavor = "b2-7"          stacks/platform.df:4
      image = "Ubuntu 24.04"
      name = "k3s-staging"
      region = "BHS5"
      ssh_key = admin
      user_data = (sensitive)  stacks/platform.df:22

tick 2  2 changes
  waits on  server.public_ip
            provider k8s  kubeconfig
  + ovh.domain_record api      stacks/platform.df:40
...
  + k8s.namespace apps         stacks/platform.df:37
      metadata.name = "apps"
...
```

For example, we want a blue/green rollout: the Service moves only after
the schema migration succeeds and every replica of the new colour is
ready:

<!-- examples/crud-api/stacks/api.df:1-25 -->
```dform
use k8s

let release = toml.decode(io.read("release.toml"))

let live = world.k8s.service["shop/api"].spec.selector.color
let live_image = world.k8s.deployment["shop/api-${live}"].spec.template.spec.containers[0].image
let next = "green" where live == "blue"
let next = "blue" where live == "green"
let next = "blue" where not has live

migrated(release.schema) where k8s.job["migrate-v${release.schema}"].status.succeeded == 1

run(live, live_image)
run(next, release.image) where migrated(release.schema)

ready(colour) where run(colour, _), app[colour].ready_replicas == app[colour].total_replicas

let serving = next where ready(next)
let serving = live where not ready(next)

resource k8s.service api {
  metadata = { name: "api", namespace: "shop" }
  spec.selector = { app: "api", color: serving }
  spec.ports = [{ port: 80, targetPort: 8080, protocol: "TCP" }]
}
```

dform works out the order.

A provider is facts as well: its types, its tables and its defaults are
rows the program reads. For example, we want a plan to refuse records
for a domain the account does not host:

<!-- examples/k3s-platform/stacks/platform.df:8-10 -->
```dform
use ovh { project = "shop" }

deny "the domain is not on this account" where not ovh.zone("example.com", _, _)
```

## Policy

Policy is part of the language: the shape every resource should have and
the changes that may not happen are both rules in the same file as the
resources, and every plan checks them. For example, we want every
resource to carry its team's tag, inside components too; remembering to
tag each one by hand is error-prone:

<!-- examples/shop/policy.df:5 -->
```dform
set r.tags = { team: "shop" } where r in resource
```

<!-- run in examples/shop -->
```
$ dform query 'aws.instance["web-us-east-1a"].tags'
{ team: "shop", Name: "shop-web-staging" }
```

After an apply, we want to ask what the next plan would change. The plan
is a table as well, `deformation`, one row per change:

<!-- run in examples/shop after dform apply -->
```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=14
Kind      Resource
"update"  aws.db_instance orders
```

"No deletes in prod" is `deny "no deletes in prod" where env == "prod",
deformation("delete", _, _)`, and it refuses the apply. A policy over a
value known only after apply stays undetermined, and the plan says when
it will know. With Terraform, policy is a second tool and a second
language over the plan's JSON, where such a value is only marked
unknown.

For a risky change, we want someone to sign off on exactly what will
apply:

<!-- examples/shop/policy.df:7-8 -->
```dform
requires_approval(r, "${kind} of ${r} in prod")
  where env == "prod", deformation(kind, r, _), kind in ["replace", "delete"]
```

We never want a prod database deleted, and we want a policy to check
that every one is protected. Lifecycle is a table, so a rule writes it
and a policy reads it:

<!-- examples/shop/stacks/shop.df:55-58 -->
```dform
lifecycle(orders, "prevent_destroy") where env == "prod"

deny "every prod database is kept" { db }
  where env == "prod", db in aws.db_instance, not lifecycle(db, "prevent_destroy")
```

`moved(aws.vpc, "main", network.vpc)` moves a resource to a new address
without replacing it.

For example, we want `dform test` to run every policy over every
combination of enums and bools, staging and prod included:

<!-- examples/shop/stacks/shop.df:1-2,5-6,30-39,51 -->
```dform
key env: enum("staging", "prod") = "staging"
input public: bool = false
use aws { region = "us-east-1", expect_account = "123456789012" }

resource aws.db_instance orders {
  identifier = "orders-${env}"
  instance_class = database.class
  multi_az = database.multi_az
  backup_retention_period = database.backup_days
  publicly_accessible = public
  password = random.password("orders")
  vpc_security_groups = [postgres]
}

deny "no public database in prod" where env == "prod", public
```

<!-- run in examples/shop -->
```
$ dform test
test shop: 4 combinations of env, public
env      public  result
staging  false   ok
staging  true    ok
prod     false   ok
prod     true    denied
denied  dform plan shop env=prod --set public=true
  - no public database in prod
test shop: 4 combinations, 1 failed
```

We want the compiler to catch a secret leaking into an output, a
condition or an address, at its line:

<!-- tests/fixtures/readme/leaks.df:1-2,5-8 -->
```dform
output password: string = pw # E0304: not declared secret(string)

let pw = random.password("db")

deny "short password" where pw.len < 12 # E0301: inspecting it leaks it
resource aws.iam_user "u-${pw}" { name = "x" } # E0305: addresses are printed
```

State holds no secret, because a generated one derives from the
deployment's master. `dform secrets` imports, lists and rotates them.
Terraform's `sensitive` keeps a value out of its CLI output and still
stores it in state.

We want to know why a resource does not exist as much as why one does.
`dform why` names the rule that could have made it and the row it
lacked:

<!-- run in examples/shop -->
```
$ dform why 'aws.subnet private-us-east-1d'
aws.subnet private-us-east-1d: no rule derives it
  stacks/shop.df:12  resource aws.subnet "private-${availability_zone}" { .. } where aws.availability_zone("available", availability_zone, n)
    aws.availability_zone("available", "us-east-1d", n): no row
    nearest: ("us-east-1a", 0), ("us-east-1b", 1), ("us-east-1c", 2)
```

## The work

Day to day it is `plan` and `apply`. Apply asks before anything changes
and again only when a later plan differs from the one shown;
interrupted, it resumes where it stopped. dform fences state: a stalled
apply cannot write once its lease has expired.

**Stacks and deployments.** An estate plans and applies as one, each
deployment with its own plan, approval and state, in dependency order.
Which deployments exist is code: `project.df` lists them as resources,
`resource stacks.platform prod { env = "prod" }`. A stack reads
another's outputs as `platform[env].kubeconfig`, so `dform apply apps
env=prod` applies platform first. Terragrunt needs a `dependency` block
per edge and a directory per environment.

In a pipeline, `status` fails unless everything is healthy:

<!-- run in examples/crud-api after dform apply; k8s.deployment blue.deploy is degraded: CrashLoopBackOff: container api -->
```
$ dform status api
k8s.deployment blue.deploy  degraded  CrashLoopBackOff: container api
k8s.job migrate-v42         -
k8s.namespace shop          -
k8s.service api             -
status: 1 degraded, 3 without health
```

`render` prints what apply would send, policy checked, with no
credentials and no state, as plain manifests for Argo CD or kustomize;
a tag resolves to its digest at plan, so a moved tag shows up as an
update:

<!-- run in examples/k3s-platform -->
```
$ dform render apps env=staging
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: apps
spec:
  replicas: 2
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - image: nginx:1.27@sha256:c4ab9b7d9cdbba02d12e91035d6350386d1c05daa28b1f5c816f3d3f34c75147
        name: web
...
$ dform render apps env=staging > manifests/apps.yaml
$ kustomize build manifests | kubectl apply -f -
```
