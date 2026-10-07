# dform

Infrastructure is a database. An account is a table of VPCs, a table of
subnets, a table of policies, each row with attributes, each row pointing
at others by id. What you want to exist is also a set of tables. A plan
is the difference between the two, and it is a table as well. A policy
is a query that must return no rows, over what will exist or over the
change itself. "Why is this subnet here" is a question about which rows
produced it. Every infrastructure tool is, underneath, doing relational
work on data it stores as text.

Datalog is the language for exactly that: tables of facts, rules that
derive new tables from them, evaluated all at once to a fixpoint. It has
been the query language of choice for program analysis, access control
and network verification for the same reason it fits here. The rules can
join anything with anything, they can recurse, every derived row has a
derivation, and the whole program is small enough to reason about.

dform is that: infrastructure as facts and rules, with plan and apply,
state, modules, policy, secrets and approvals built in. Here is a
complete program, for an AWS provider.

```dform
use aws { region = "us-east-1" }

resource aws.vpc main {
  cidr_block = "10.0.0.0/16"
}

#| One private subnet in every available zone of the region.
resource aws.subnet "private-${availability_zone}" {
  vpc = main
  cidr_block = inet.subnet(main.cidr_block, 8, n)
  availability_zone
} where aws.availability_zone("available", availability_zone, n)
```

```
$ dform plan
plan: 7 changes (7 create) over 1 tick

tick 1  7 changes
  + aws.vpc main                   shop.df:3
      cidr_block = "10.0.0.0/16"
  + aws.subnet private-us-east-1a  shop.df:8
      availability_zone = "us-east-1a"
      cidr_block = "10.0.0.0/24"
      vpc = main
  + aws.subnet private-us-east-1b  shop.df:8
      availability_zone = "us-east-1b"
      cidr_block = "10.0.1.0/24"
      vpc = main
  ... four more
```

Every line of the plan says where it comes from, and only that: a
value written by another file carries that file and line, `-v` adds how
each value was computed and which writes lost, `-vv` each value's chain.
The subnet block ends in a `where` clause, which makes it a rule: the
clause is a query, and every answer is one subnet. `aws.availability_zone`
is a table the provider answers, what Terraform calls a data source:
`"available"` is the question, and each row binds a zone's name to
`availability_zone` and its stable position to `n`, so the n-th zone
gets the n-th /24. An entry that is only a name, `availability_zone`,
takes the variable of that name. `vpc = main` is a reference, printed
as the address it names: the VPC, which does not exist yet, so apply
makes it first. The `#|` line is a doc comment, which the editor
shows and policy can read. When the region gains a
zone, the next plan has one more subnet; nothing in the file changes.

If you have run Terraform in anger, three things are different here and
none of them is a feature flag. The plan tells you why each line is
there and how many rounds the apply will take before you say yes. A
policy is a query over the change set, written in the same file as the
resources, refused with the same `why`. A value the cloud only learns at
apply is a value now, so the cluster and what runs on it are one
program and one plan. There is no second tool for any of these.

## One model

What exists is a table, what you want is a table, the plan is the
difference and is a table, and policy is rules over any of them. These
lines go in the same file as the resources, under a header saying what
the program takes:

```dform
#| An input that selects the deployment: the keys together name one, with its own state.
key env: enum("dev", "staging", "prod") = "dev"

deny "every database has backups" where db in aws.db_instance, db.backup_retention_period < 7
deny "no deletes in prod" where env == "prod", deformation("delete", _, _)
requires_approval(sg, "security group ${action}") where deformation(action, sg, _), sg in aws.security_group
```

The first line is about what will exist. The other two are about the
change itself: once the plan is computed, every change in it is a row
the program can query, and the program's own rules run over those rows
before anything is applied. The plan says which matched and what
follows:

```
$ dform plan shop env=prod
plan: 3 changes (1 create, 1 update, 1 delete) over 1 tick, 1 denied, 1 approval
...
denied
  no deletes in prod                      aws.db_instance reports    shop.df:7

held for approval
  aws.security_group api                  security group update      shop.df:8

apply: refused  1 deny
plan digest: sha256:4f9c1e...
```

A deny refuses the plan, naming the row that matched. A
`requires_approval` row refuses a plain apply until someone signs the
plan's digest, which is the tool section's business. A message can name
the resource itself, `deny "no deleting ${r} in prod" where
deformation("delete", r, _)`, and `why 'deny(m)'` explains any of
these the way it explains a subnet.

```bash
dform -C examples/tour plan
```

The examples in this repository run on a fake cloud built into dform, so
they work from a clean clone with no credentials (`cargo install --path
.` puts `dform` on your path; in a checkout, `cargo run --` does the
same). `examples/tour` is a tutorial you read top to bottom.

## Why a rule, not a template

If you know Terraform, the program above is readable: a provider, a
resource block with attributes, a reference to another resource, a
plan with `+` lines, and an apply that makes it so. The workflow is the
same, and the pieces around it are too: state, modules with inputs and
outputs, a lock, a plan file you can review and sign. dform keeps the
shape because the shape is right.

What changes is what a block is. In HCL a resource block is a value in a
template, filled in from variables, and the tool evaluates the template
top down with special cases for the parts that cannot be a template:
`for_each` for repetition, `dynamic` blocks for repetition inside a
block, `depends_on` for an edge it cannot see, `-target` and a second
run for a value it cannot know yet, a provider feature for a tag that
should be everywhere. Each is a patch over something a template cannot
say. Pulumi removes the wall with a general-purpose language and loses
the ability to reason about the program: no plan you can trust before
running it, no policy you can prove.

In dform a resource block is a rule. Its clause is a query; its
attributes are contributions to cells that other rules may also write.
That is the whole difference, and it is why some of the words are
different too. The ones you need:

| word | means | in Terraform |
|---|---|---|
| **resource** | a thing dform makes and manages, of any type | resource |
| **type** | what a resource is; a provider's (`aws.vpc`) or your own | resource type |
| **component** | a type you define: a resource made of resources, with inputs and outputs | a module instance |
| **module** | a `.df` file: a namespace of values, relations, types and components, named by its path | a module's source |
| **use** | import a module into scope, with its inputs bound; rules it carries run here | `module` block, `source` |
| **provider** | a module whose types a process implements; imported with `use`, its block its configuration | provider |
| **stack** | a module the tool deploys, one deployment per key, with its own state | root module, workspace |
| **key** | an input the target gives, selecting the deployment | workspace, `-var` |
| **change** | one line of the plan: create, update, replace, delete | planned action |
| **tick** | one round of an apply; a later tick waits on what an earlier one learned | a second run with `-target` |
| **set**, **deny**, **why** | write a cell from anywhere; refuse a plan; the derivation of anything | no equivalent |

There is no `instance`, no `data` block, no `locals`, no `variable`, no
`count`: a resource with a clause is the repetition, a provider's table
is the data source, a `let` is the local, an `input` is the variable. One more thing is worth saying up front:
the same properties make dform easy for a model to draft. There is one
way to spell each thing, every statement is right or wrong on its own
with nothing to simulate, and the mistakes a draft makes, a variable
bound once, two values for one cell, a literal of the wrong type, are
compile errors rather than surprises at apply.

## What follows from that

Seven things fall out of a block being a rule.

**Every rule sees every resource.** A rule can read any resource in the
program, in any module, declared before or after it. That is what
evaluating a Datalog program to a fixpoint means; nothing is ordered.

```dform
output private_subnets: list(aws.subnet) =
  [ s | s in aws.subnet, s.map_public_ip_on_launch == false ]
```

`s in aws.subnet` ranges over every subnet the program wants, wherever
it was declared; the brackets collect the matches into a list of
subnets, and a database that takes `subnets = network.main.private_subnets`
gets their ids at apply.
A subnet added in another module tomorrow is in this list without this
line changing.

**An attribute can have several authors.** A module sets a tag, a policy
sets another, the stack sets a third from its key. They merge per
leaf by rank, `@default` below normal below `@override`. Two authors who
disagree at the same rank are a conflict that names both. The order of
statements never matters.

```dform
set r.tags.team = "platform" @default where r in resource
```

`set` contributes to an attribute of something declared elsewhere; `r in
resource` is every resource of every type. `@default` means any explicit
`team` tag wins over this one. Ask for the merged result:

```
$ dform query 'main.vpc.tags'
{component: "network", env: "prod", team: "platform"}
```

Three authors: the module wrote `component`, the stack wrote `env` from
its key, this policy wrote `team`.

**Policy adds as well as forbids.** A policy pack is a file of these
statements, and both kinds go in it: the rule that writes a value and
the rule that refuses one.

```dform
# policies/baseline.df
set r.tags.team = "platform" @default where r in resource
deny "prod databases are multi-AZ" where env == "prod", db in aws.db_instance, not db.multi_az
```

That `set` is the one that wrote `team` in the merged tags just now, so
a pack is simply one of the authors a resource has. The `deny` reads
what they all produced: `db in aws.db_instance` binds `db` to each
database the program wants, in any module, whoever declared it, and
`db.multi_az` reads its attribute after every contribution has merged.
A deny with answers refuses the plan and prints them; `warn` reports
and goes on.

**The plan is a table too.** The denies that opened this page read a
relation dform declares for you, one row per change in the plan:

```dform
decl deformation(kind: enum("create", "update", "replace", "delete"), resource: ref, before: digest)
```

The resource is a reference like any other, so a rule over the change
set joins it to what the resource is: `db in aws.db_instance` narrows a
replace to databases, and a message interpolates the row it matched.

```dform
warn "replacing a database" { db } where deformation("replace", db, _), db in aws.db_instance
```

Because it is an ordinary relation, you can also just ask:

```
$ dform query 'deformation(k, r, _)'
k         r
"create"  aws.subnet["private-us-east-1c"]
"update"  aws.security_group["api"]
```

Terraform teams build this out of plan JSON, a policy engine and a CI
step; here it is a line in the same file as the resources, with the
same `why`. The lifecycle rules in the tool section are written this
way, and so is "needs approval".

**A value the cloud produces later is a value now.** A database's
endpoint does not exist until the database does. Every tool has to live
with that; Terraform's answer is the error everyone has met, "value
depends on resource attributes that cannot be determined until apply",
followed by a `-target` run by hand and a second plan. dform carries the
unknown as a value, `orders.endpoint`, plans with it,
and applies in ticks: everything that can be made is made, the unknowns
resolve, what depended on them is planned again against the real values
and made. A resource whose *name* depends on an unknown goes under
`later`: the plan names the rule and the value it waits on, never a
count, instead of refusing.

```dform
resource aws.iam_policy "connect-${host}" {
  policy = { Statement: [{ Action: "rds-db:connect", Resource: host }] }
} where db in aws.db_instance, host = db.endpoint
```

One policy per database, named after its endpoint. `host = db.endpoint`
needs the endpoint's text, which only exists once the database does, so
how many policies there will be is not known until the first tick runs:

```
later
  aws.iam_policy "connect-${host}"  shop.df:31  waits on orders.endpoint
```

Apply then runs tick 1, learns the endpoint, prints tick 2's plan with
the policy's real name, and asks again before making it:

```
plan: 1 change (1 create) over 1 tick

tick 2  1 change
  + aws.iam_policy "connect-orders.cx3k.us-east-1.rds.amazonaws.com"  shop.df:31
      policy = { Statement: [{
        Action: "rds-db:connect",
        Resource: "orders.cx3k.us-east-1.rds.amazonaws.com",
      }] }

Apply tick 2 to shop[env=prod]? [y/N]
```

You never approve a count of "unknown": each tick is asked for once its
plan has names, at tick 2, 3, 4 as needed, and `--yes` answers for you.
A plan file in CI applies only what it showed; at a tick that would add
something nobody printed, it stops with the state consistent and says to
run apply again, which plans the rest with the real values in front of a
reviewer.

**Rules recurse.** Which VPCs can reach which, through a transit hub, is
a path of any length. Routes for every pair are three lines, and they
stay correct as spokes come and go.

```dform
input supernet: inet = "10.0.0.0/12"

decl net(site: string, slot: int)
net("core", 0)
net("blue", 1)
net("green", 2)

hub("core")
spoke("blue")
spoke("green")

# One hop: every spoke is linked to the hub, in both directions.
link(h, s) where hub(h), spoke(s)
link(s, h) where hub(h), spoke(s)

# A path of any length: the rule that uses itself.
reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)

resource aws.ec2_transit_gateway tgw
resource aws.vpc "${v}" { cidr_block = inet.subnet(supernet, 4, slot) } where net(v, slot)
resource aws.route_table "${t}" { vpc = aws.vpc[t] } where net(t, _)

resource aws.route "${a}-to-${b}" {
  route_table = aws.route_table[a]
  destination_cidr_block = aws.vpc[b].cidr_block
  transit_gateway = tgw
} where reaches(a, b), a != b
```

`net` is the address plan and `hub`/`spoke` the topology, kept apart:
each site takes its slot's /16 of the /12, so no address is written
twice and moving the supernet moves every VPC. The route block makes
one route per reachable pair, and `aws.route_table[a]` looks a resource
up by the name the clause bound. Add `spoke("red")` and a slot for it,
and red's VPC, its route table and every route to and from it all
appear.

**The plan is a database, and it explains itself.** Every fact has a
derivation, and the plan prints the short form of it on every line:
the statement that produced the change with its bindings, the write
that won each changed attribute, and, since the last apply, what moved
to cause it: the row that appeared, the input that changed, the guard
that stopped holding. A delete says what used to derive the resource
and which of those facts is gone. For the thing that is not there,
`why` names the rule that could have produced it and the first
condition that failed, with the nearest rows that would have passed:

```
$ dform why 'aws.subnet["private-us-east-1c"]'
aws.subnet private-us-east-1c: no rule derives it
  shop.df:8  resource aws.subnet "private-${availability_zone}" { .. } where aws.availability_zone("available", availability_zone, n)
    aws.availability_zone("available", "us-east-1c", n): no row
    nearest: ("us-east-1a", 0), ("us-east-1b", 1)
```

The limit is stated rather than papered over: `why` explains what the
program derived, and what one rule failed to derive, and invents no
reason for something no rule mentions. Where a value
came from is one question away, each expression it passed through to
the literal at the end, and what it beat:

```
$ dform why k3s.admin.public_key
ovh.ssh_key k3s.admin.public_key = "ssh-ed25519 AAAA…"
  = ssh_public_key                         k3s.df:23
  = config.ssh_public_key                  stacks/platform.df:24
  = "ssh-ed25519 AAAA…"                    config.df:13
```

`plan -vv` prints the same chain under each attribute. The full
derivation, down to the line of source or the row of a table, is
`--tree`:

```
$ dform why --tree 'aws.route["blue-to-green"]'
aws.route["blue-to-green"]
  stacks/network.df:58  resource aws.route "${a}-to-${b}" { .. } where reaches(a, b), a != b
  with a = "blue", b = "green"
       "${a}-to-${b}" = "blue-to-green"
       aws.route_table[a] = aws.route_table["blue"]
       aws.vpc[b].cidr_block = 10.2.0.0/16
  ├─ reaches("blue", "green")
  │    stacks/network.df:55  reaches(a, c) where reaches(a, b), link(b, c)
  │    with a = "blue", c = "green", b = "core"
  │    ├─ reaches("blue", "core")
  │    │    stacks/network.df:54  reaches(a, b) where link(a, b)
  │    │    with a = "blue", b = "core"
  │    │    └─ link("blue", "core")
  │    │         stacks/network.df:52  link(s, h) where hub(h), spoke(s)
  │    │         with s = "blue", h = "core"
  │    │         ├─ hub("core")   stacks/network.df:47
  │    │         └─ spoke("blue")   stacks/network.df:48
  │    └─ link("core", "green")
  │         stacks/network.df:51  link(h, s) where hub(h), spoke(s)
  │         with h = "core", s = "green"
  │         ├─ hub("core")   stacks/network.df:47
  │         └─ spoke("green")   stacks/network.df:49
  └─ aws.vpc["green"].cidr_block = 10.2.0.0/16
       merged from 1 contribution
       └─ 10.2.0.0/16
            stacks/network.df:23  resource aws.vpc "${v}" { cidr_block = inet.subnet(supernet, 4, slot) } where net(v, slot)
            with v = "green", slot = 2
                 inet.subnet(supernet, 4, slot) = 10.2.0.0/16
            ├─ input supernet = 10.0.0.0/12
            │    merged from 1 contribution
            │    └─ 10.0.0.0/12 @default   stacks/network.df:14
            └─ net("green", 2)   stacks/network.df:19
```

The answer is the program's own text at the lines that fired (a
block's entries elided as `..`, but for the entry that fired), the
variables as they were bound, and under them every computed term of
the statement with what it became: the interpolated name, each lookup,
each read, a value still unknown as `?` and what it stands for. An attribute is
merged from its contributions, each with its value, its rank when it
is not the normal one, and the statement or line that made it: a
constant is its line. The same question works for an attribute
(`why 'aws.vpc["main"].tags.team'` shows every author and which rank
won) and for a refusal (`why 'deny(m)'`). `why --core` prints the
lowered rules instead.

## The language

This is the tour, condensed. `examples/tour` has every section with the
command to run and what it prints; `docs/grammar.md` is the reference.

**Facts and rules.** A fact is a row: `az("us-east-1a", 1)`. A rule
derives rows: `link(h, t) where hub(h), spoke(t)`. Lower-case names are
variables, bound where they first appear; constants are quoted. A
variable used only once in a rule is an error, so a typo cannot become
a cross product of cloud resources.

**Resources.** `resource TYPE name { attr = value ... }` says a resource
should exist. With a `where` clause it is a rule, one resource per
answer, and the header may interpolate the clause's variables. An entry
that is only a name, `availability_zone`, takes the variable of that
name.

**References and reads.** `vpc = main` is a reference: the schema says
`vpc` identifies a VPC, so the attribute takes the VPC itself, an edge
in the apply order, and the provider gets its id once it exists.
`inet.subnet(main.cidr_block, 8, n)` reads the cidr now, because the
function needs its bytes. An attribute that is a value, `endpoint =
db.endpoint`, is passed along and resolves at apply; the same dot inside
a function or a clause reads it now, and the compiler says at the read
when that makes a block wait for a later tick.

**Types.** Values are typed, and strings stop at the edge. A provider's
schema types every attribute (`cidr_block: inet required`,
`multi_az: bool`, `endpoint: string computed`), inputs and document
columns declare theirs, and whatever arrives as text, a `--set`, a YAML
cell, a CSV field, is parsed into the declared type there or rejected
with the file and line. Inside the program a network is an `inet`, and
the arithmetic is on networks:

```dform
input vpc_net: inet = "10.0.0.0/16"
decl az(name: string, index: int)
input az from yaml("data/azs.yaml")

resource aws.subnet "private-${availability_zone}" {
  cidr_block = inet.subnet(vpc_net, 4, n)          # the n-th /20 of the /16
  availability_zone
} where az(availability_zone, n)

deny "subnets overlap" { a: x, b: y } where {
  x in aws.subnet
  y in aws.subnet
  x != y
  inet.overlaps(x.cidr_block, y.cidr_block)
}

deny "database reachable from the internet" where {
  rule in aws.security_group_rule
  rule.to_port == 5432
  "0.0.0.0" in rule.cidr_ipv4
}
```

A literal takes the type its position expects, as in Postgres:
`cidr_block = "10.0.0.0/16"` is an `inet` because the schema says so,
and a literal that does not parse is an error at that line. Types are
inferred where they are not declared, and there are no constructors: a
string becomes a network where an `inet` is wanted, an attribute, a
parameter, `let net: inet = cfg.net`. `inet.subnet`, `inet.host`,
`inet.contains`, `inet.overlaps` and a network's fields `n.addr` and
`n.bits` are the network arithmetic;
quantities are values too, a number and its unit in one token: `memory
= 512Mi` is `bytes`, `cpu = 500m` a `cpu` (millicores, because the
schema says cpu; `m` is minutes where it says duration), compared in
base units (`limits.memory > 2Gi`) and sent to each provider in its
schema's form (`20Gi` is `"20Gi"` to Kubernetes and `20` to RDS); a
`time` is a zoned instant and a `duration` a span, added in the time's
zone, a month a month and a day a day across DST;
`enum`, `list`, `set`, `ref(T)` and `secret(T)` are the other types,
and `type environment = enum("dev", "staging", "prod")` names one. A
`check` refines any of them, `input replicas: int = 2 check 1 <=
replicas <= 10`, `cidr_block: inet check cidr_block.bits <= 24` in a
schema, and a check is a policy: it is a `deny` over the
value, checked at compile time for a literal, at evaluation for a
computed value, and by the provider after apply for a secret, with
`why` explaining a failure like any deny.

**Externs and functions.** An extern is a relation a provider answers
on demand, with binding modes: `aws.availability_zone["available"]`,
`aws.ami[filter]`, `time.now()`. Functions are pure and qualified by the
type they are about, `inet.subnet`, `str.split`, `regex.match`,
`oci.with_tag`, declared in signature files you can jump to from the
editor. A
generated secret is a function too: `random.password("db")` derives the
same value every run from the deployment's own secret, so nothing is
stored; what must be kept is kept on purpose, `memo.first("db-created",
time.now(), created)` returns the first value it was ever given.

**Secrets.** A sensitive value never leaves the provider as bytes. The
schema says which attributes are sensitive, and the compiler follows
them: a secret that reaches a position where it would be seen is a
compile error, named and located, before anything runs.

```dform
# None of these compile. Each is a secret reaching somewhere it would be seen.
deny "weak password" where db.master_password == "hunter2"  # E0301: comparing leaks a bit
output password: string = db.master_password                # E0304: not declared secret(string)
resource aws.iam_user "u-${db.master_password}"             # E0305: addresses are printed
```

Two more are refused the same way: `not` over a secret, because absence
leaks a bit, and `count` over one, because cardinality does.
`declassify(v, reason)` is the one way past, and it has to say why.

The useful consequence is what state does not hold. A plan and an apply
read attribute values from the provider every run, so state maps each
address to its remote id and records what the last apply saw, and
nothing more. No secret is written to it. Two things that would
otherwise have to be kept are arranged so they need no storage either:

- A generated secret is derived, not drawn. `random.password("db")` is
  HKDF of the deployment's key with the call and the deployment as its
  info, so every run computes the same password, a changed knob is a
  new one, and no run writes it down.
- A value that genuinely must survive, a creation time or a token
  someone else issued, is kept on purpose by `memo.first`. A secret one
  is sealed (XChaCha20-Poly1305, its label as associated data) with a
  key derived from the same deployment key: state holds the seal, and
  the run that needs the value opens it in memory.

A secret another stack publishes is not copied at all: the reader's
state records which provider holds it, and the provider substitutes the
bytes inside the call that needs them.

Be exact about what this buys, because it is one step short of where it
should be. Both mechanisms rest on one 32-byte key per deployment,
`state.key`, made on first use and written `0600`, and today it sits
beside the state it protects. So reading `state.json` reveals nothing,
which is worth having: a copy, a backup, a file pasted into a ticket
leaks no password, where a Terraform state file leaks every one it ever
held. But someone who takes the whole directory has the key, and with
it can recompute every derived secret and open every sealed memo. That
is a smaller and tidier target than a plaintext state file, not a
stronger kind of protection.

Two things are needed to close that and neither is built: the key
wrapped by something outside the state (a KMS key, a passphrase, an
external reference), and rotation. Rotating `state.key` today changes
every derived value at once, which is a credential rotation across the
whole deployment rather than a key rotation. Secret handling is the
newest part of the design and the least exercised; treat it as a
promising mechanism rather than a settled one.

**The header.** A file begins with what it takes:

```dform
key env: enum("dev", "staging", "prod") = "dev"
key region: enum("us-east-1", "eu-west-1") = "us-east-1"
input owner: string
input db {
  multi_az: bool = false
  backup_days: int = 3
}
```

A stack is the unit of state and apply, and a file under `stacks/` is
one, named after itself: this is `stacks/shop.df`, so `dform plan shop`.
An `input` is what the outside supplies, with a type and maybe a
default. A `key` is an input with one more property: it selects the
deployment. The keys together name one, so `env` and `region` give a
deployment per combination, each with its own state, and they are given
with the target, `dform plan shop env=prod region=eu-west-1`.
A provider is imported like a module, `use aws { region }`, its block
being its configuration, and the line goes wherever reads best, usually
next to what it depends on. Where
a stack's state lives and who may approve a plan are operational, so
they live in `dform.toml`, where `[stacks.shop]` is `stacks/shop.df`:

```toml
[project]
edition = "2026"

[stacks.shop]
backend = 's3("acme-state", "shop/{env}")'
approvals = 'jwks("https://sso.acme.example/keys")'
```

A small project needs none of this: `dform.toml` beside one `.df` file is
a project with one stack.

**Configuration.** Every bare name you read is a cell: an `input` is one
the outside supplies, a `key` one the target supplies, a `let` one the
program computes, and `why` shows the layers of any of them.
Configuration is the inputs, and `set` writes them like any other cell:
the declaration gives the default, a `set` contributes a value under a
condition, and `--set` on the command line wins over both:

```dform
set { db.multi_az = true, db.backup_days = 14 } where env == "prod"
set db.backup_days = 30 @override where env == "prod", region == "eu-west-1"
set from yaml("config/${env}.yaml")
```

The condition can be any subset of a composite key, or anything else the
program knows. The deployment's value is just the input's name,
`db.backup_days`, and `why` shows which layer won; two sets that both
apply and disagree are a conflict naming both, so a broad one says
`@default`. `set from` takes a whole document, one leaf per input path,
which is how a `config/prod.yaml` written by another tool feeds the
program.

**Documents.** Data that is not code is loaded as a document and
destructured into relations, and the rows are facts like any other,
each with a line `why` can point at:

```dform
let network = toml("data/network.toml")
decl az(name: string, index: int)
decl peering(name: string, peer: string)
decl pin(app: string, image: string)
input az from network.az                         # the [[az]] tables
input peering from network.peerings
input pin from toml("git+https://github.com/acme/ops/pins.toml?ref=env/${env}")
```

```toml
# data/network.toml
[[az]]
name = "us-east-1a"
index = 1

[[peerings]]
name = "shared"
peer = "vpc-0a1b2c"
```

`yaml`, `toml`, `json` and `csv` load; `input name from DOC` reads one
row per object by the declared column names, parsing each cell to its
type or failing with the file and line, and several `from` lines union;
a row written by hand mixes in. A `git(..)` source is read at a commit the plan
records, so apply reads what plan read even if the branch moved. A file
of plain facts is a module like any other: `use data.releases`, then
`releases.release(app, key, value)`.

The other direction is a function. Some APIs want a document as a
string in a field, a ConfigMap entry or a policy body, and
`yaml.encode`, `json.encode` and `toml.encode` build one from a value
the program computed, so the nesting is dform's and only the last step
is text:

```dform
resource k8s.config_map app {
  metadata.name = "app"
  data."app.yaml" = yaml.encode({
    listen: ":8080",
    upstreams: [ s.endpoint | s in aws.db_instance ],
  })
}
```

A rule can read the list and a deny can check it; nothing has to parse
a string to find out what the program said.

**Modules.** Every `.df` file is a module, named by its path from the
project root: `config`, `modules.net`, `stacks.platform`. `use config`
imports it: its rules and denies run over what you can see, its items
read as `config.x`, its inputs are bound by a block on the `use` or by
their defaults, and its resources, if it has any, are stamped once under
its name, so `use synapse` is the homeserver and `use traefik {
acme_email }` is the ingress with its one input given. A module used
from two stacks runs in both. `std` is used everywhere already;
`str.split` needs no `use`.

A **component** is a type you define: `component NAME { .. }`, an item
of a module, with inputs for its attributes and outputs for what it
computes. `resource` makes one, exactly as it makes one of a provider's
type, gated by a clause if you like. Inside it, relations are private;
its resources are visible to policy, as cloud resources are.

```dform
component network {
  input cidr: inet
  output vpc: aws.vpc = vpc

  resource aws.vpc vpc { cidr_block = cidr }
  resource aws.subnet "private-${availability_zone}" {
    vpc = vpc
    cidr_block = inet.subnet(cidr, 8, n)
    availability_zone
  } where az(availability_zone, n)
}

resource network blue { cidr = "10.1.0.0/16" }
resource network green { cidr = "10.2.0.0/16" } where env == "prod"
```

Inside, `vpc` is the copy's own resource and `az(..)` is the stack's
table, read like any fact. `green` exists only in prod. The copy's VPC is
`blue.vpc` everywhere else, `network[t].vpc` ranges over every copy,
and the plan shows `blue` as one change with its VPC and subnets under
it. An input with no
value is the same error wherever it is: in a stack, a module, or a
component.

**Either cloud.** A name may be declared more than once when each
declaration has a clause, and a signature says what the copies have in
common, so one stack runs on either cloud:

```dform
input cloud: enum("aws", "gcp")
input gcp_project: string where cloud == "gcp"

type database = component {
  input name: string
  output conn: string
}

component rds: database { .. }
component cloudsql: database { .. }

use aws { region = "eu-west-1" } where cloud == "aws"
use google { project = gcp_project } where cloud == "gcp"

resource rds db { name = "shop" } where cloud == "aws"
resource cloudsql db { name = "shop" } where cloud == "gcp"
```

`db.conn` is whichever copy holds; a deployment that picks gcp never
starts the aws provider, and `gcp_project` is asked for only there.
Two declarations that both hold are a deny naming both, and over enum
inputs the compiler warns of a value no declaration covers. Switching
`cloud` on a deployment plans a delete of one copy and a create of the
other: the data does not move.

**Policies.** A policy is a module of `set`, `deny` and `warn`
statements, and nothing marks it as one but where you chose to put it:
`policies/baseline.df` is applied with `use policies.baseline`. A pack
can write into your resources, and nothing about that is hidden:
it touches a stack only when the stack says `use`, `dform dev effects`
lists what it touches, ranks decide who wins, and `why` names the author
of every value. A pack with inputs takes them on the `use`. A pack reads
the plan's own `deformation` rows as readily as it reads resources, so
"no deletes in prod" and "a replace needs a signature" are policy like
any other.

**Testing.** The denies are the tests. `dform test` evaluates the program
once for every combination of inputs, each key and enum enumerated,
each `check` supplying its boundaries and samples, against an empty
world, and every deny must hold in each:

```dform
deny "prod peers the two VPCs" where env == "prod", not "blue-green" in aws.vpc_peering_connection
deny "dev has no database" where env == "dev", _ in aws.db_instance
```

A failure prints the inputs that produced it as `--set` flags, so it
reproduces in one command; `dform test shop env=prod` pins the key, so
only prod worlds run.

## The tool

**A project** is a directory tree whose root holds a `dform.toml`;
`dform init` writes one. One directory name means anything to the tool,
`stacks/`, which is where it looks for stacks, and a project without
one takes the root's `.df` files instead, so `dform.toml` beside
`shop.df` is a project with one stack. Every other file is a module
named by its path, and you arrange them however you like:
`policies/baseline.df` is `policies.baseline` because of where you put
it, not because `policies/` is a thing. State lives in a gitignored
`dform.state/`. A command runs on a target: a stack by name, or one
deployment of a keyed stack, `dform plan shop env=prod`.

**plan** prints what will change, grouped by tick. Tick 1 applies now;
each later tick names the values it waits on; `later` lists the rules
that may add changes once a value is known, as the rule, never as a
count. A last line, `apply: refused  1 deny`, appears only when there
is something to decide. A plan line is one of two
shapes: a change, `+ aws.subnet private-us-east-1a  shop.df:8`, its
address as the source names it with the file and line that made it, or
an attribute, `cidr_block = "10.0.0.0/24"`, followed by a file and line
only when the value was written outside its own block (a policy, a
`set`, a config module). A reference is the address it names, a secret
`(sensitive)`, a long string elided in the middle, and a `because` line
says what moved since the last apply. `-v` adds how: the bindings, the
expression behind each value, and the writes that lost with their ranks
(`@normal over k3s.df:9 @default`); `-vv` adds under each attribute the
chain of expressions its value passed through, as `why` prints it. `-q`
is the bare diff for scripts, laid out as it was before
any of this. Colour is a hint and never the only carrier. `--json`
carries the ticks, what each waits on, each change's kind, full address,
site and `because` as fields.

**apply** prints the plan and asks. It applies in ticks, planning each
when the one before reports: how many there are is not known up front.
At any tick that adds what the first plan could not show, it prints
that tick's plan and asks again before changing anything, at tick 2, 3,
4 as needed; `--yes` answers every question, and a plan file applies
what it showed and stops before the rest. State is written after every provider call,
so an interrupted apply resumes where it stopped. `--parallel N`
overlaps independent calls. Every provider call has a timeout, and one
that failed in a way worth trying again (a 429, a 5xx, the connection)
is sent again with backoff up to a budget; a create that timed out is
looked up by its idempotency key first, so it is adopted, never made
twice. A tick held on a value the world has not reached yet, a Job's
`status.succeeded`, waits for it, saying `waiting on
k8s.job["migrate-v42"].status.succeeded since 02:14 (3m)` every ten
seconds. The wait is bounded by that provider's `wait` (10m unless
dform.toml says), not the `timeout` of its calls; past it the apply
stops with the state consistent and says what it waited on.

**Lifecycle.** The things that happen between two applies are facts,
so policy can read them and `why` can explain them:

```dform
moved(aws.vpc, "main.vpc", core_vpc)       # renamed: state follows, nothing is replaced
adopt(legacy, "vpc-0a1b2c")                          # exists already: take it over, no create
lifecycle(orders, "prevent_destroy")                 # a delete or replace is a deny
lifecycle(nodes, "create_before_destroy")            # a replace builds the new one first
ignore_changes(bastion, "tags.last_scan")            # set on create, then the world's value stands
lifecycle(db, "prevent_destroy") where env == "prod", db in aws.db_instance   # every prod database
```

Which way a replace goes is the schema's to say, with
`create_before_destroy` where it allows either; the old object is
deposed and deleted the tick after what depended on it has moved. Drift
is detected on every plan, because a plan starts by refreshing what
exists: a change made in the console shows as an update back, or as a
deny if a policy says so. An apply that dies halfway resumes: each
create carries an idempotency key, and the next `apply` says "resuming
the apply interrupted at tick 2" and finishes it, after checking that
the world did not move under the remaining actions. `dform destroy
TARGET` removes a deployment: the same plan against an empty wanted
set, every object deleted dependents first, `prevent_destroy` a
refusal; the audit log stays.

**Plan files and approvals.** `plan --out plan.json` records everything
the plan depended on. `apply plan.json` refuses if the world or the
inputs moved. `apply --approval` verifies a signed digest offline
against the stack's trust root. `dform verify plan.json` recomputes the
plan from the file alone, with no cloud access.

**why, query, diff.** `dform why ADDR` explains a resource, or its
absence; `dform why 'deny "MESSAGE"'` says whether a deny holds and
why. `plan` warns when a change would delete everything a
rule derived at the last apply, or empty a relation that had rows then,
naming the rule and the row that went, so a broken join and a deliberate
delete do not look alike; `apply` asks for that on its own, also under
`--yes`, unless `--allow-empty` names it. `dform query 'attr(aws.subnet, s,
"availability_zone", z)'` asks the fact store anything. `dform diff
--since 2026-09-20` explains what changed between applies, and why.

**test and check.** `dform test` runs the denies over the input space;
`dform check --sarif` runs them for CI.

**Stacks and deployments.** A project grows from one file to many
stacks without changing shape. Each `stacks/*.df` is a stack with its
own state; its keys make it one deployment per combination, so one program is
`shop[env=dev]`, `shop[env=staging]` and `shop[env=prod]`, each applied,
locked and audited on its own:

```
$ dform stack list
shop[env]       stacks/shop.df
  env=dev       applied 2026-09-30 14:02 by simon at 1c83fe0
  env=prod      applied 2026-09-28 09:40 by ci at 0cebc08, plan pending
platform[env]   stacks/platform.df
```

A stack is a module the tool uses, one deployment per key combination. `use
stacks.platform` binds to those deployments, and reading one is the same
keyed read as reading a copy; another project's stacks mount under a
name in `dform.toml`:

```dform
use stacks.platform
use acme.stacks.platform as acme_platform

let cluster_endpoint = platform[env].endpoint
let registry = acme_platform[env="prod"].registry_url
```

The stack is the unit of partial work: `dform apply shop` applies the
stacks shop reads from first, then shop, each with its own confirmation
and state, and nothing that depends on shop; `dform apply` with no
target is the whole project in dependency order. A plan of shop before
platform is applied shows platform's outputs as unknowns, pending on
that apply, the same way it shows a value the cloud has not produced. State is small: it maps each address to the object's
remote id and records what the last apply saw; attribute values come
from the provider on every plan, and no secret is ever written to it.
It lives in a directory or an S3 bucket with conditional writes and a
lease per deployment, so a second apply of the same deployment is
refused naming the holder. `dform state show`, `state mv` and `stack
rekey` are the state operations.

**Providers.** A provider is a module whose items are types and externs
and whose inputs are its configuration, so `use aws { region }` imports
and configures it, `use aws as eu { region = "eu-west-1" }` is a second
account or region, and a guarded `use` is a provider that exists only
in some environments. It runs as a process dform starts, native over
gRPC, or as a wasm component behind `--features wasm`, and either way
it never touches a socket: HTTP, SSH and git are the host's, with
credentials applied by name, so a provider holds nothing it was not
granted. It carries its own schema, so the editor can jump to a type's
definition with nothing running. A registry is a bucket, the same kind
you keep state in: `[registries] acme = { backend = 's3(..)', keys =
'jwks_file(..)' }` in `dform.toml`, versions immutable, packages signed,
resolved into `dform.lock`. `dform provider publish` runs the
conformance suite and uploads. Functions ship the same way.

**The editor.** `dform lsp` gives diagnostics, hover with the value of any
term for the selected deployment and who contributed it, jump to a type's
or a function's definition, and the plan's action beside each resource.
`dform fmt` has one normal form per construct.

## From the VPC to the running service, in one project

A Deployment and a VPC are the same kind of row to dform, so they get
the same management model: one plan, one confirmation per tick, one
signed approval, one `why`, the same denies and the same drift
handling, in one project. Infrastructure tooling and Kubernetes tooling
grew up apart, and most teams run two change processes because of it;
here a policy that says "every resource carries a team tag" covers the
VPC and the Deployment with the same line, and "no deletes in prod"
means the namespace too.

Infrastructure tools stop at the cluster and hand over to a second tool
chain for what runs on it. The reason is specific: the cluster's
endpoint does not exist until the cluster does, and a provider block is
the one place those tools cannot wait for a value. Terraform's provider
configuration is evaluated before the plan, so a provider fed by a
resource output is a documented limitation and a second root module.

In dform importing a provider is a rule like every other statement, and
the evaluation engine that carries unknowns through a resource carries
them through a provider too. `use k8s { endpoint = cluster.endpoint }`
is simply a rule that cannot fire until tick 1 has made the cluster;
the engine knows that, plans the cluster first, learns the endpoint,
configures the provider, and plans what runs on it in tick 2. There is
no second-class corner of the language where values have to be known
in advance: not providers, not components, not names. And because an attribute can have several authors, a Kubernetes
object is assembled the way kustomize assembles one, from a base and
any number of overlays, except that the overlays are rules.

```dform
use aws { region }

resource aws.vpc main { cidr_block = vpc_net }
resource aws.eks_cluster cluster {
  vpc_config.subnets = [ s | s in aws.subnet ]
}

# The Kubernetes provider is bound to the cluster above: its endpoint and
# CA are unknown until tick 1 has created it, so this provider, and
# everything that uses it, waits for that tick.
use k8s {
  endpoint = cluster.endpoint
  ca = cluster.certificate_authority
}

resource k8s.namespace shop { metadata.name = "shop" }

resource k8s.deployment api {
  metadata.namespace = shop.metadata.name
  spec.template.spec.containers = [{ name: "api", image: released_image }]
}
```

The plan says it in its own terms: the namespace and the deployment sit
under `tick 2  waits on cluster.endpoint`. Tick 1 makes the VPC and the
cluster; the provider is configured from the endpoint; tick 2 makes the
namespace and the deployment. One
plan, one apply, one state, one `why`.

In a real project this is two stacks, `stacks/platform.df` owning the
cluster and `stacks/shop.df` owning what runs on it, because they change
at different speeds and are applied by different people. The second
says `use stacks.platform` and reads `platform[env].endpoint`, and
nothing else changes: the engine treats a value another stack published
exactly as it treats one the cloud will produce. The same policy pack that tags every VPC can set
resource limits on every container, in every module: `c` is bound to
an element of a keyed list, so the write lands on that container by
name, not by position, and a module's own limits win over the default:

```dform
set c.resources.limits = { cpu: 1, memory: 512Mi } @default
  where w in k8s.deployment, c in w.spec.template.spec.containers
```

Deployments are rules too. A blue/green rollout is the release (a row
read from git), the colour the live Service points at (read from the
world), and three rules: run the migration Job for the release's
schema, bring up the other colour once the Job has succeeded, switch
the Service once every replica of the new colour is ready. Each step
waits on a status field the cluster fills in, which is an unknown until
it does, so each is its own tick and the plan says which:

```dform
# The release is a row from git; the serving colour is a world fact.
let active = world.k8s.service["shop/crud-api"].spec.selector.color
other("blue", "green")
other("green", "blue")
let next = other[active]

let rollout = next where world.k8s.deployment["shop/crud-api-${active}"]
  .spec.template.spec.containers[0].image != released_image

migrated(v) where k8s.job["migrate-v${v}"].status.succeeded == 1
run(rollout, released_image, schema) where migrated(schema)
ready(c) where run(c, _, _), app[c].ready_replicas == app[c].total_replicas
let serving = rollout where ready(rollout)
```

`world.T` reads what exists rather than what the program wants, so
`active` is whichever colour the live Service selects. `app` is the
component each colour is a resource of, and its outputs carry the
replica counts the cluster fills in.

`examples/crud-api` is the whole thing: a database, its generated
password as a secret the program never sees, the namespace with a
default-deny network policy, the migration, the two colours, the
cutover, and the invariants (no container without limits, no image
without a digest, no public database) as denies.

## What you cannot do elsewhere

|You want                                     |The usual workaround                       |In dform                                                   |
|---------------------------------------------|-------------------------------------------|-----------------------------------------------------------|
|a resource per value only apply knows        |`-target`, then a second run by hand       |listed under `later`; apply runs a second tick             |
|a tag on everything, overridable per resource|a variable threaded through every module   |`set r.tags.team = "platform" @default where r in resource`|
|"why does this exist?"                       |read the source, guess                     |every plan line says; `dform why ADDR` for each value      |
|"why does this not exist?"                   |read the source, guess harder              |`dform why ADDR` names the condition that failed           |
|"how many rounds will this apply take?"      |find out during the apply                  |the plan is grouped by tick, with what each tick waits on  |
|rules about the change set itself            |plan JSON through an external policy engine|the plan is a table the program's own denies read          |
|routes from reachability                     |write them out, keep them in sync          |a recursive rule                                           |
|a policy that sees inside modules            |export every value as an output            |policy reads any resource                                  |
|a /20 per team that never moves              |a spreadsheet                              |`allocate`, pinned in state †                              |
|policies tested over every environment       |one test per case                          |`dform test`, over the input space                         |
|"can A reach B?" before apply                |a separate tool, after the incident        |`std.net` reachability as a query †                        |
|adopt four hundred existing subnets          |one import block each                      |`dform import --match` with one rule †                     |
|prove an approved plan is what runs          |trust                                      |`dform verify plan.json` †                                 |
|a secret the tool will not print             |`sensitive = true`, and hope               |a label the compiler tracks                                |

† designed, not built yet; see below.

## What runs today

dform is pre-release, and this page describes the language as it is
being designed. Most of it runs now; some of it does not, and the gap is
not uniform, so here it is.

Everything in the tour and in the Kubernetes section above runs from a
clean clone with no credentials: the recursive routes, the ranked merge,
unknowns carried through ticks, `deformation` rows the program's own
denies read, provenance, approvals over a signed digest, secret labels
the compiler tracks, `dform test` over the input space, keyed stacks and
their state. `examples/` is the proof; each one is a project you can run.

The rows marked † in the table above are designed and not built, and
so are `check --sarif` and the provider registry.

Secret handling is the newest part and the one to treat as an
experiment. The compiler's refusals and the redaction are tested, but
the deployment key is unwrapped and sits beside the state, and there is
no rotation. The Secrets section above says exactly what that does and
does not buy.

Providers are the honest caveat. The contract is WIT and the intended
host is wasmtime, but today a provider is a native process reached over
gRPC, and the wasm host is bindings only. The Kubernetes provider is
real and derives its schema from a cluster's own OpenAPI document. Two
things gate the wasm host, on different clocks: Rust's `wasm32-wasip3`
target was promoted to tier 2 in September 2026 and is riding the
release train, while client-certificate TLS, which is how most
kubeconfigs authenticate, is not in any adopted WASI proposal. So
Kubernetes stays a native provider for the foreseeable future, and
`dform-grpc` is a first-class host rather than a bridge.

## Where next

- `examples/tour`: the tutorial. `examples/crud-api`: the Kubernetes
  rollout above. Each other example under `examples/` shows one thing;
  its README says which.
- `docs/grammar.md`: the language, for reference.
- `docs/reference.md`: every command and flag, the provider model, state
  backends, keyed stacks, approvals, the audit log.
- `proposals/`: the model and how it was decided: cells and ranks,
  unknowns and ticks, the plan as a Z-set, provenance. Read
  `E-synthesis.org` as revised by `F-revision.org`.
- `DESIGN.org`: decisions and the roadmap.

The language changes without compatibility until it ships; every `.df`
in the repository is rewritten when it does.

Apache-2.0. Contributions are accepted under the Developer Certificate
of Origin (sign your commits with `-s`); there is no CLA.
