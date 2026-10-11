# shared-network

A company's network on AWS, run by its network team: a core network for
the services every team reaches, a spoke network per team, each in every
zone, and a peering from the core to each spoke, in staging and in prod.
The spokes are a list in `data/networks.toml`; each environment's address
range is its own file under `config/net/`. `policy.df` is the team's
policy: two networks that route to each other never share an address.
The provider is the AWS mock (dform.toml).

From this directory, `dform` being `cargo run --` in a checkout:

```
dform plan                    # staging and prod, each its own deployment
dform apply                   # each, asked for on its own
dform plan net env=prod       # prod alone
```

## A spoke per team

A team's network is a component, a VPC and its subnets as one unit,
stamped once per row of the toml list, at the team's slot of the
environment's range:

<!-- run in examples/shared-network -->
```
$ dform plan net
deployment: stacks.net[env=staging]
plan: 11 changes (11 create) over 1 tick; policy: 2 hold

tick 1  11 changes
  + aws.vpc core                          stacks/net.df:27
      cidr_block = "10.16.0.0/16"
      tags.Name = "core"
  + network data
    + aws.vpc data.vpc                    stacks/net.df:18
        cidr_block = "10.18.0.0/16"       stacks/net.df:10
        tags = { Name: "data", team: "data" }
...
  + aws.vpc_peering_connection core-data  stacks/net.df:33
      peer_vpc = data.vpc
      vpc = core
  + aws.vpc_peering_connection core-shop  stacks/net.df:33
      peer_vpc = shop.vpc
      vpc = core

policy  2 hold
```

The peerings are derived: `hub` and `spoke` are facts about the
networks, `link` is a rule over them, and a peering is made for every
link. A new team is three lines of the toml list; its network, its
subnets and its peering follow. A peering between two spokes written by
hand is refused by the deny under the rule.

The same program in prod is prod's range:

<!-- run in examples/shared-network -->
```
$ dform plan net env=prod
deployment: stacks.net[env=prod]
plan: 11 changes (11 create) over 1 tick; policy: 2 hold

tick 1  11 changes
  + aws.vpc core                          stacks/net.df:27
      cidr_block = "10.0.0.0/16"
...
```
