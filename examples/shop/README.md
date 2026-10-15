# shop

A small web shop on AWS, run by its own team: a VPC with a private
subnet in every zone, the orders database, web servers behind a load
balancer, and the role they connect to the database with, in staging and
in prod, each a deployment with its own state. `policy.df` is the team's
policy: every resource carries the team's tag, and a replace or a delete
in prod is signed off. The provider is the AWS mock (dform.toml), so
nothing here needs an account.

From this directory, `dform` being `cargo run --` in a checkout:

```
dform plan                    # staging: every change, with the line that made it
dform apply                   # asks, then makes it in two ticks
dform plan shop env=prod      # prod: the same program, prod's settings
dform test                    # every deny, staging and prod, public or not
```

## The plan

We write what should exist; the plan is the difference between that and
the world. The mock's world is empty, so everything is a create (`+`),
each with the file and line that made it:

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
...
tick 2  1+ changes
  waits on  orders.resource_id
  + aws.iam_role app                  stacks/shop.df:61
...
  aws.iam_policy "connect-${id}"      stacks/shop.df:42  waits on orders.resource_id

policy  4 hold
```

A subnet is a row of a rule: one for every zone the provider's
`aws.availability_zone` table answers, `with n = 0` the answer that made
it. `vpc = main` is a reference: the subnet waits for the VPC, and apply
gives it the VPC's id. `tags.team` is written by the policy, at its own
line. The connect policy is named for the database's resource id, which
only apply learns, so it is planned in the tick after the database's and
says what it waits on.

`dform apply` prints the plan and asks once; it makes tick 1, plans tick
2 with what it learned, and makes it. After it, there is nothing to do:

<!-- run in examples/shop after dform apply -->
```
$ dform plan
deployment: stacks.shop[env=staging]
stack shop is up to date
```

## Staging and prod

The stack's key, `env`, makes each value a deployment of its own, with
its own state. Prod's settings are a `set` that holds where `env ==
"prod"`; the plan says which line gave each value:

<!-- run in examples/shop -->
```
$ dform plan shop env=prod
deployment: stacks.shop[env=prod]
plan: 12 changes (12 create) over 2 ticks; policy: 4 hold
...
  + aws.db_instance orders             stacks/shop.df:30
      backup_retention_period = 14     stacks/shop.df:25
      identifier = "orders-prod"
      instance_class = "db.m6g.large"  stacks/shop.df:24
      multi_az = true                  stacks/shop.df:23
...
```

`why` follows a value to the literal, and what it overrode:

<!-- run in examples/shop -->
```
$ dform why orders.backup_retention_period shop env=prod
aws.db_instance orders.backup_retention_period = 14
  = database.backup_days  stacks/shop.df:34
  = 14                    stacks/shop.df:25
  over 1 @default         stacks/shop.df:3
```

## Policy

A deny is a rule whose matches stop the plan, and `dform test` runs every
one over every combination of the key and the bool inputs:

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

The tag rule reads every resource and adds to it, so a resource's tags
have two authors:

<!-- run in examples/shop -->
```
$ dform query 'aws.instance["web-us-east-1a"].tags'
{ team = "shop", Name = "shop-web-staging" }
```

After an apply, the next plan is a table too, `deformation`:

<!-- run in examples/shop after dform apply -->
```
$ dform query 'deformation(kind, resource, _)' --set database.backup_days=14
Kind      Resource
"update"  aws.db_instance orders
```

And `why` says why something does not exist:

<!-- run in examples/shop -->
```
$ dform why 'aws.subnet private-us-east-1d'
aws.subnet private-us-east-1d: no rule derives it
  stacks/shop.df:12  resource aws.subnet "private-${availability_zone}" { .. } where aws.availability_zone("available", availability_zone, n)
    aws.availability_zone("available", "us-east-1d", n): no row
    nearest: ("us-east-1a", 0), ("us-east-1b", 1), ("us-east-1c", 2)
```
