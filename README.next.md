# dform

Infrastructure is a database. An account is a table of VPCs, a table of
subnets, a table of policies, each row with attributes, each row pointing
at others by id. What you want to exist is also a set of tables. A plan
is the difference between the two. A policy is a query that must return
no rows. "Why is this subnet here" is a question about which rows
produced it. Every infrastructure tool is, underneath, doing relational
work on data it stores as text.

Datalog is the language for exactly that: tables of facts, rules that
derive new tables from them, evaluated all at once to a fixpoint. It has
been the query language of choice for program analysis, access control
and network verification for the same reason it fits here. The rules can
join anything with anything, they can recurse, every derived row has a
derivation, and the whole program is small enough to reason about.

dform is that: infrastructure as facts and rules, with plan and apply,
state, modules, policy, secrets, approvals and a controller mode built
in. Here is a complete program, for an AWS provider.

```dform
edition 2026

provider aws { region = "us-east-1" }

resource aws.vpc main {
  cidr_block = "10.0.0.0/16"
}

#| The availability zones, and the subnet index each gets.
decl az(name: string, index: int)        # optional: inferred from the rows otherwise
az("us-east-1a", 1)
az("us-east-1b", 2)

resource aws.subnet "private-${availability_zone}" {
  vpc_id = main.id
  cidr_block = inet.subnet(main.cidr_block, 8, n)
  availability_zone
} where az(availability_zone, n)
```

```
$ dform plan
plan: 3 deformations (3 create)
+ aws.vpc["main"]
  cidr_block = "10.0.0.0/16"
+ aws.subnet["private-us-east-1a"]
  availability_zone = "us-east-1a"
  cidr_block = "10.0.1.0/24"
  vpc_id = ?aws.vpc["main"].id
+ aws.subnet["private-us-east-1b"]
  availability_zone = "us-east-1b"
  cidr_block = "10.0.2.0/24"
  vpc_id = ?aws.vpc["main"].id
```

The `az(..)` lines are facts: two rows in a table. The `decl` above them
names and types the columns; without it the types are inferred from the
rows. The `#|` line is a doc comment, which the editor shows and policy
can read. The subnet block ends in a `where` clause, which makes it a
rule: the clause is a query over the tables, and every answer is one
subnet; an entry that is only a name, `availability_zone`, takes the
variable of that name. `?` marks a value apply will learn, here the
VPC's id. Add a third availability zone and a third subnet follows;
nothing else changes.

```bash
dform -C examples/tour plan
```

The examples in this repository run on a fake cloud built into dform, so
they work from a clean clone with no credentials (`cargo install --path
.` puts `dform` on your path; in a checkout, `cargo run --` does the
same). `examples/tour` is a tutorial you read top to bottom.

## It looks like Terraform, on purpose

If you know Terraform, the program above is readable: a provider, a
resource block with attributes, a reference to another resource's id, a
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
That is the whole difference, and six things fall out of it.

**Every rule sees every resource.** A rule can read any resource in the
program, in any module, declared before or after it. That is what
evaluating a Datalog program to a fixpoint means; nothing is ordered.

```dform
output private_subnet_ids: list(ref(aws.subnet)) =
  [ s.id | s in aws.subnet, s.map_public_ip_on_launch == false ]
```

`s in aws.subnet` ranges over every subnet the program wants, wherever
it was declared; the brackets collect one `s.id` per match into a list.
A subnet added in another module tomorrow is in this list without this
line changing.

**An attribute can have several authors.** A module sets a tag, a policy
sets another, the environment's settings set a third. They merge per
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
$ dform query 'aws.vpc["network.main::vpc"].tags'
{component: "network", env: "prod", team: "platform"}
```

Three authors: the module wrote `component`, the environment's settings
wrote `env`, this policy wrote `team`.

**Policy adds as well as forbids.** The line above is a policy. So is
this one, and both live in the same pack.

```dform
deny "prod databases are multi-AZ" where env == "prod", db in aws.db_instance, not db.multi_az
```

`db in aws.db_instance` binds `db` to each database the program wants,
in any module, whoever declared it; `db.multi_az` reads its attribute
after every author's contribution has merged. A deny with answers
refuses the plan and prints them; `warn` reports and goes on.

**A value the cloud produces later is a value now.** A database's
endpoint does not exist until the database does. Every tool has to live
with that; Terraform's answer is the error everyone has met, "value
depends on resource attributes that cannot be determined until apply",
followed by a `-target` run by hand and a second plan. dform carries the
unknown as a value, `?aws.db_instance["orders"].endpoint`, plans with it,
and applies in ticks: everything that can be made is made, the unknowns
resolve, what depended on them is planned again against the real values
and made. A resource whose *name* depends on an unknown is a pending
group; the plan says so, and says which tick resolves it, instead of
refusing.

```dform
resource aws.iam_policy "connect-${host}" {
  policy = { Statement: [{ Action: "rds-db:connect", Resource: host }] }
} where db in aws.db_instance, host = db.endpoint
```

One policy per database, named after its endpoint. `host = db.endpoint`
needs the endpoint's text, which only exists once the database does, so
how many policies there will be is not known until the first tick runs:

```
pending groups:
? aws.iam_policy[?] x unknown, on ?aws.db_instance["orders"].endpoint, resolves after tick 1
```

Apply then runs tick 1, learns the endpoint, prints tick 2's plan with
the policy's real name, and asks again before making it:

```
tick 2:
plan: 1 deformation (1 create)
+ aws.iam_policy["connect-orders.cx3k.us-east-1.rds.amazonaws.com"]
  policy.Statement[0].Resource = "orders.cx3k.us-east-1.rds.amazonaws.com"
Apply 1 new deformation to shop[env=prod]? [y/N]
```

You never approve a count of "unknown". An unattended apply (`--yes`,
or a plan file in CI) goes further only through ticks the plan
enumerated; at a tick that would add something nobody printed, it stops
with the state consistent and says to run apply again, which plans the
rest with the real values in front of a reviewer.

**Rules recurse.** Which VPCs can reach which, through a transit hub, is
a path of any length. Routes for every pair are three lines, and they
stay correct as spokes come and go.

```dform
hub("core")
spoke("blue")
spoke("green")

link(h, s) where hub(h), spoke(s)
link(s, h) where hub(h), spoke(s)

reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)

resource aws.route "${a}-to-${b}" {
  route_table_id = aws.route_table[a].id
  destination_cidr_block = aws.vpc[b].cidr_block
  transit_gateway_id = tgw.id
} where reaches(a, b), a != b
```

`link` is one hop, in both directions. `reaches` is the rule that uses
itself: a path of any length. The route block then makes one route per
reachable pair; `aws.route_table[a]` looks a resource up by name at run
time, and `tgw` is the transit gateway declared elsewhere in the file.
Add `spoke("red")` and every route to and from red appears.

**The plan is a database.** Every fact has a derivation and you can ask
for it, down to the line of source or the row of a table.

```
$ dform why 'aws.route["blue-to-green"]'
aws.route["blue-to-green"]
  stacks/network.df:58  resource aws.route "${a}-to-${b}" { .. } where reaches(a, b), a != b
  with a = "blue", b = "green"
       "${a}-to-${b}" = "blue-to-green"
       aws.route_table[a].id = ?aws.route_table["blue"].id
       aws.vpc[b].cidr_block = 10.2.0.0/16
       tgw.id = ?aws.ec2_transit_gateway["tgw"].id
  ├─ reaches("blue", "green")
  │    stacks/network.df:55  reaches(a, c) where reaches(a, b), link(b, c)
  │    with a = "blue", b = "core", c = "green"
  │    ├─ reaches("blue", "core")
  │    │    stacks/network.df:54  reaches(a, b) where link(a, b)
  │    │    └─ link("blue", "core")
  │    │         stacks/network.df:52  link(s, h) where hub(h), spoke(s)
  │    │         ├─ hub("core")      stacks/network.df:47
  │    │         └─ spoke("blue")    stacks/network.df:48
  │    └─ link("core", "green")
  │         stacks/network.df:51  link(h, s) where hub(h), spoke(s)
  │         ├─ hub("core")      stacks/network.df:47
  │         └─ spoke("green")   stacks/network.df:49
  └─ aws.vpc["network.green::vpc"].cidr_block = 10.2.0.0/16
       merged from 1 contribution
       └─ stacks/network.df:23  module network, instance green
```

The answer is the program's own text at the lines that fired, the
variables as they were bound, and under them every computed term of
the statement with what it became: the interpolated name, each lookup,
each read, a value still unknown as its `?` label. The same question works for an attribute
(`why 'aws.vpc["main"].tags.team'` shows every author and which rank
won) and for a refusal (`why 'deny(m)'`).

## The language

This is the tour, condensed. `examples/tour` has every section with the
command to run and what it prints; `docs/grammar.md` is the reference.

**Facts and rules.** A fact is a row: `az("us-east-1a", 1)`. A rule
derives rows: `link(h, t) where hub(h), spoke(t)`. Lower-case names are
variables, bound where they first appear; constants are quoted. A
variable used only once in a rule is an error: in a language whose
output is cloud resources, a typo must not become a cross product.

**Resources.** `resource TYPE name { attr = value ... }` says a resource
should exist. With a `where` clause it is a rule, one resource per
answer, and the header may interpolate the clause's variables.

**References and reads.** `vpc_id = main.id` is a reference: an edge in
the apply order, and an unknown until the VPC exists.
`inet.subnet(main.cidr_block, 8, n)` reads the cidr now, because the
function needs its bytes. A dot is a reference where it stands as a whole
value and a read where its content is used. When a read of a computed
value makes a block wait for a later tick, the compiler says so at the
read.

**Types.** Values are typed, and strings stop at the edge. A provider's
schema types every attribute (`cidr_block: inet required`,
`multi_az: bool`, `endpoint: string computed`), an input or a table
column declares its type, and whatever arrives as text, a `--set`, a
YAML cell, a CSV field, is parsed into the declared type there or
rejected with the file and line. Inside the program a network is an
`inet`, not a string that happens to contain dots:

```dform
input vpc_net: inet = "10.0.0.0/16"
az(name: string, index: int) from yaml("data/azs.yaml")

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
  inet.contains(rule.cidr_ipv4, ip("0.0.0.0"))
}
```

A literal takes the type its position expects, as in Postgres:
`cidr_block = "10.0.0.0/16"` is an `inet` because the schema says the
attribute is one, and a literal that does not parse is an error at that
line. Where no type is expected a string stays a string, and a computed
one is parsed on purpose: `inet(text)`. Types are inferred where they are not
declared: a relation's columns from its facts and rules, a `let` from
its value, a module output from its term; `decl` and annotations are
for the edge, the docs and the editor, not for the checker's sake.
`inet.subnet`, `inet.host`, `inet.contains`, `inet.overlaps` and
`inet.prefix_len` are the network arithmetic; `ip`, `inet`, `int`, `string` are the parsers; `enum`, `list`, `set`, `ref(T)` and
`secret(T)` are the other types. `type environment = enum("dev",
"staging", "prod")` names one. A `check` refines any of them: `input
replicas: int = 2 check 1 <= replicas <= 10` on an input,
`cidr_block: inet check inet.prefix_len(cidr_block) <= 24` on a schema
attribute, and a provider ships its own with its schema. A check is a
policy in disguise: it lowers to a `deny` over the value, so it is
checked at compile time when the value is a literal, at evaluation when
it is computed, and after apply by the provider when the value is a
secret the engine never sees; and `why` explains a failed one like any
deny. Refinements also describe the input space, which is what
`dform test` enumerates. Because the
comparisons are typed, the policy above is a real overlap test over
address ranges, not a string match, and "can this range reach that
one" is a question the program can answer before anything is created.

**Externs and functions.** An extern is a relation a provider answers
on demand, with binding modes: `aws.ami[filter]`,
`file.json[path]`, `random.password[key]` (a secret, generated once and
kept). Functions are qualified by the type they are about,
`inet.subnet`, `str.split`, `list.join`, and declared in signature files
you can jump to from the editor; constructors are named by their type,
`int(s)`, `inet(s)`.

**Secrets.** A sensitive value never leaves the provider as bytes. The
compiler tracks where secrets flow and refuses a program that would
print one, compare one, or put one in a public attribute, before
anything runs.

**The header.** A file begins with what it is and what it takes, before
any rule:

```dform
edition 2026
key env: enum("dev", "staging", "prod") = "dev"
key region: enum("us-east-1", "eu-west-1") = "us-east-1"
input owner: string
input db: { multi_az: bool, backup_days: int } = { multi_az: false, backup_days: 3 }
```

A stack is the unit of state and apply, and a file under `stacks/` is
one, named after itself: this is `stacks/shop.df`, so `dform plan shop`.
An `input` is what the outside supplies, with a type and maybe a
default; one without a default must be given. A `key` is an input with
one more property: it selects the deployment, so each value of `env`
and `region` has its own state, and it is given with the target, `dform
plan shop env=prod region=eu-west-1`, never by `--set`. Together they
are the stack's interface, and they come before the first rule. A `provider`
statement is not a header line: it is a rule that configures a provider
from whatever it reads (`provider aws { region }`, or `provider aws`
alone), in scope for the whole program wherever it is written, so it
goes next to what it depends on. Where a stack's state lives and who may
approve a plan are operational, so they live in `dform.toml`, where `[stacks.shop]` is
`stacks/shop.df`:

```toml
[stacks.shop]
backend = 's3("acme-state", "shop/{env}")'
approvals = 'jwks("https://sso.acme.example/keys")'
```

A small project needs none of this: `dform.toml` beside one `.df` file is
a project with one stack.

**Settings.** Configuration is the inputs. The declaration gives the
default; a `settings` block contributes values to them under a
condition, usually on the key; `--set` on the command line wins over
both. Any subset of a composite key, or anything else the program
knows, can be the condition:

```dform
settings { db.multi_az = true, db.backup_days = 14 } where env == "prod"
settings { db.backup_days = 30 } @override where env == "prod", region == "eu-west-1"
settings from yaml("config/${env}.yaml")
```

The deployment's value is just the input's name, `db.backup_days`, and
`why` shows which layer won. Two blocks that both apply and disagree at
the same rank are a conflict naming both, so a broad block that should
lose says `@default`; nothing is decided by how specific a condition
looks. `settings from` takes a whole document, one leaf per input path,
which is how a `config/prod.yaml` written by hand or by another tool
feeds the program. A `let` is a cell too: two `let` rows that disagree
are a conflict like any other.

**Documents.** Data that is not code is loaded as a document and
destructured into relations, and the rows are facts like any other,
each with a line `why` can point at:

```dform
let network = toml("data/network.toml")
az(name: string, index: int) from network          # the [[az]] tables
peering(name: string, peer: string) from network.peerings
pin(app: string, image: string) from toml(git("ops.git", "env/${env}", "pins.toml"))
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

`yaml`, `toml`, `json` and `csv` are the loaders; a loaded document is
a value, and `name(columns) from DOC` reads one row per object by column
name, parsing each cell to its column's type or failing with the file
and line. `from` is the third way to define a relation, beside writing
its rows and deriving them with a rule, and the three mix: `az("local",
9)` beside the `from` is one more row. A relation is owned by the file
that defines it; a module reads the stack's `az` and cannot add to it.
A relation is a set, so order lives in lists: `zone = network.azs[i]`
walks a list with its index, and a relation is sorted first when a
position is wanted. A `git(..)` source is read at a commit the plan
records, so apply reads what plan read even if the branch moved. A file
of plain facts is just a module: `module releases` reads
`data/releases.df`.

**Modules.** A module declares its interface first, then its body;
written inline, or as `module network` alone, which reads
`modules/network.df`, and modules nest. An instance is one copy with
inputs, gated by a clause if you like; `use` applies a module once, with
no inputs, which is what a policy pack is. A module's resources are not
hidden: policy sees them, and stacks wire modules together through
outputs.

```dform
module network {
  input cidr: inet
  output vpc: aws.vpc = vpc

  resource aws.vpc vpc { cidr_block = cidr }
  resource aws.subnet "private-${availability_zone}" {
    vpc_id = vpc.id
    cidr_block = inet.subnet(cidr, 8, n)
    availability_zone
  } where az(availability_zone, n)
}

instance network blue { cidr = "10.1.0.0/16" }
instance network green { cidr = "10.2.0.0/16" } where env == "prod"
```

The module takes one input and offers one output, its VPC. Inside, `vpc`
is the module's own resource; `az(availability_zone, n)` is the stack's
table, which a module reads like any fact. An entry that is just a name
sets the attribute from the variable of the same name, as `{ a }` does
in an object. `green` exists only in prod. The module's
VPC is addressed as `aws.vpc["network.blue::vpc"]` everywhere else, and
another block reads it as `network.blue.vpc`.

**Policies.** A policy is a module of `set`, `deny` and `warn`
statements with no inputs, `module baseline { .. }` in `policies/`, and
`use baseline` applies it. There is no grant to declare: what a pack
touches is visible in the pack and in `dform dev effects`, and ranks
decide who wins.

**Tests.** A test is a named block of denies and the part of the input
space they are about; the name is yours:

```dform
test prod_network_and_backups {
  deny "prod peers the two VPCs" where not "blue-green" in aws.vpc_peering_connection
  deny "prod keeps two weeks of backups" where db in aws.db_instance, db.backup_retention_period < 14
} where env == "prod"
```

`dform test` evaluates the program once for every combination of inputs
the clause admits, against an empty world, and the denies must hold in
each: an equality pins an input, an enum left free is enumerated, an
`int` with a `check` gets its boundaries and samples. A plain `deny` in
the program is policy, enforced on every real plan; a test's denies run
only under `dform test` and never block a plan. A failure prints the
inputs that produced it as `--set` flags, so it reproduces in one
command. `dform test shop env=prod` takes a target like `plan` and
`apply` and pins the key, so only prod worlds run.

## The tool

**A project** is a directory with a `dform.toml`: `stacks/` (one stack
per file), `modules/`, `policies/`, `config/`, `data/`, `providers/`, and a
gitignored `dform.state/`. `dform init` makes one. A command runs on a
target: a stack by name, or one deployment of a keyed stack, `dform plan
shop env=prod`.

**plan** prints the difference between the program and the world:
creates, updates, deletes and replaces grouped by resource, what is
pending on an unknown, what cannot be decided yet, conflicts, and the
apply order by tick. `--json` for machines. `--why` prints under each
change the rule and the base facts that caused it: "because
`data/azs.yaml:3`".

**apply** prints the plan and asks. It applies in ticks; at any tick that
adds a resource the first plan could not name, it prints that tick's
plan and asks again before changing anything. `--yes` for scripts, which
applies only the ticks the plan enumerated and stops before one that
would add more, to be run again. State is written after every
provider call, so an interrupted apply resumes where it stopped.
`--parallel N` overlaps independent calls.

**Plan files and approvals.** `plan --out plan.json` records everything
the plan depended on. `apply plan.json` refuses if the world or the
inputs moved. Policy can say a change needs approval; an approver signs
the plan's digest; `apply --approval` verifies it offline against the
stack's trust root. `dform verify plan.json` recomputes the plan from the
file alone, with no cloud access.

**why, query, diff.** `dform why ADDR` explains a resource; `dform why
'deny(m)'` explains a refusal. `dform query 'attr(aws.subnet, s,
"availability_zone", z)'` asks the fact store anything. `dform diff
--since 2026-09-20` explains what changed between applies, and why.

**test and check.** `dform test` runs the tests over the input space; `dform check --sarif`
runs the policies for CI.

**Stacks and deployments.** A project grows from one file to many
stacks without changing shape. Each `stacks/*.df` is a stack with its
own state; a `key` input makes it one deployment per value, so one
program is `shop[env=dev]`, `shop[env=staging]` and `shop[env=prod]`,
each applied, locked and audited on its own:

```
$ dform stack list
shop[env]    stacks/shop.df
  env=dev      applied 2026-09-30 14:02 by simon at 1c83fe0
  env=prod     applied 2026-09-28 09:40 by ci at 0cebc08, plan pending
platform.cluster[env]   stacks/cluster.df
```

Per-environment values are a settings table the stack's `config` names,
one YAML file per deployment, every leaf a contribution that wins over
the program's `@default` layer:

```toml
[stacks.shop]
config = 'yaml("config/shop/{env}.yaml")'
```

State lives in a directory or an S3 bucket with conditional writes and a
lease, per stack, per deployment. Stacks read each other's outputs
through the same lookup shape as everything else, across projects too
when `dform.toml` names the other project's backend as a remote:

```dform
cluster_endpoint = stacks.platform.cluster[env=env].endpoint
```

`dform state show`, `state mv`, `stack rekey` (move a deployment to a
new key value) and `stack handover` (move a stack's state into the
cluster it bootstrapped, for the controller) are the state operations;
nothing else edits state by hand.

**The controller.** `dform controller run` watches the inputs and the
world and applies the same plan a batch run would, continuously. The same
program bootstraps a cluster in batch mode and later runs inside it.

**Providers.** A provider is a wasm component: one file, any platform,
sandboxed, carrying its own schema, so the editor can jump to a type's
definition with nothing running. A registry is a bucket, the same kind
you keep state in: `[registries] acme = { backend = 's3(..)', keys =
'jwks_file(..)' }` in `dform.toml`, versions immutable, packages signed,
resolved into `dform.lock`. `dform provider publish` runs the
conformance suite and uploads; there is no registry service to run.
Functions can be shipped the same way.

**The editor.** `dform lsp` gives diagnostics, hover with the value of any
term for the selected deployment and who contributed it, jump to a type's
or a function's definition, and the plan's action beside each resource.
`dform fmt` has one normal form per construct.

## From the VPC to the running service, in one project

Infrastructure tools stop at the cluster and hand over to a second tool
chain for what runs on it. The reason is specific: the cluster's
endpoint does not exist until the cluster does, and a provider block is
the one place those tools cannot wait for a value. Terraform's provider
configuration is evaluated before the plan, so a provider fed by a
resource output is a documented limitation and a second root module.

In dform a provider block is a rule like every other statement, and the
evaluation engine that carries unknowns through a resource carries them
through a provider too. `provider kubernetes { endpoint =
cluster.endpoint }` is simply a rule that cannot fire until tick 1 has
made the cluster; the engine knows that, plans the cluster first, learns
the endpoint, configures the provider, and plans what runs on it in
tick 2. There is no second-class corner of the language where values
have to be known in advance: not providers, not module instances, not
names. And because an attribute can have several authors, a Kubernetes
object is assembled the way kustomize assembles one, from a base and
any number of overlays, except that the overlays are rules.

```dform
provider aws { region }

resource aws.vpc main { cidr_block = vpc_net }
resource aws.eks_cluster cluster {
  vpc_config.subnet_ids = [ s.id | s in aws.subnet ]
}

# The Kubernetes provider is bound to the cluster above: its endpoint and
# CA are unknown until tick 1 has created it, so this provider, and
# everything that uses it, waits for that tick.
provider kubernetes {
  endpoint = cluster.endpoint
  ca = cluster.certificate_authority
}

resource k8s.namespace shop { metadata.name = "shop" }

resource k8s.deployment api {
  metadata.namespace = shop.metadata.name
  spec.template.spec.containers = [{ name: "api", image: released_image }]
}
```

The plan says it in its own terms: the namespace and the deployment are
`pending on ?aws.eks_cluster["cluster"].endpoint, resolves after tick
1`. Tick 1 makes the VPC and the cluster; the provider is configured
from the endpoint; tick 2 makes the namespace and the deployment. One
plan, one apply, one state, one `why`.

In a real project this is two stacks, `stacks/platform.df` owning the
cluster and `stacks/shop.df` owning what runs on it, because they change
at different speeds and are applied by different people. The second
reads the first's outputs, `stacks.platform[env=env].endpoint`, and
nothing else changes: the engine treats a value another stack published
exactly as it treats one the cloud will produce. The same policy pack that tags every VPC can set
resource limits on every container, in every module, and the list is
merged by the container's name, not its position:

```dform
set w.spec.template.spec.containers[name].resources.limits = { cpu: "1", memory: "512Mi" } @default
  where w in k8s.deployment, c in w.spec.template.spec.containers, name = c.name
```

Deployments are rules too. A blue/green rollout is the release (a fact
from git), the colour the live Service points at (a world fact), and
three rules: run the migration Job for the release's schema, bring up
the other colour once the Job has succeeded, switch the Service once
every replica of the new colour is ready. Each step waits on a status
field the cluster fills in, which is an unknown until it does, so each
is its own tick and the plan says which:

```dform
migrated(v) where k8s.job["migrate-v${v}"].status.succeeded == 1
run(rollout, released_image, schema) where migrated(schema)
ready(c) where run(c, _, _), app[c].ready_replicas == app[c].total_replicas
let serving = rollout where ready(rollout)
```

`examples/crud-api` is the whole thing: a database, its generated
password as a secret the program never sees, the namespace with a
default-deny network policy, the migration, the two colours, the
cutover, and the invariants (no container without limits, no image
without a digest, no public database) as denies. Run it in batch, or
hand the stack over to the cluster it built and let `dform controller
run` keep it true as releases land.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, then a second run by hand | a pending group; apply runs a second tick |
| a tag on everything, overridable per resource | a variable threaded through every module | `set r.tags.team = "platform" @default where r in resource` |
| "why does this exist?" | read the source, guess | `dform why ADDR` |
| routes from reachability | write them out, keep them in sync | a recursive rule |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| a /20 per team that never moves | a spreadsheet | `allocate`, pinned in state |
| policies tested over every environment | one test per case | `dform test`, over the input space |
| "can A reach B?" before apply | a separate tool, after the incident | `std.net` reachability as a query |
| adopt four hundred existing subnets | one import block each | `dform import --match` with one rule |
| prove an approved plan is what runs | trust | `dform verify plan.json` |
| a secret that never hits disk | `sensitive = true`, and hope | a label the compiler tracks |

## Where next

- `examples/tour`: the tutorial. `examples/crud-api`: the Kubernetes
  rollout above. Each other example under `examples/` shows one thing;
  its README says which.
- `docs/grammar.md`: the language, for reference.
- `docs/reference.md`: every command and flag, state backends, keyed
  stacks, approvals, the audit log, the controller.
- `docs/design.md`: the model: cells and ranks, unknowns and ticks, the
  plan as a Z-set, provenance. `proposals/` is the record of how it was
  decided.
- `DESIGN.org`: decisions and the roadmap.

dform is pre-release. The language changes without compatibility until
it ships; every `.df` in the repository is rewritten when it does.

Apache-2.0. Contributions are accepted under the Developer Certificate
of Origin (sign your commits with `-s`); there is no CLA.
