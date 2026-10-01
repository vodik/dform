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
  cidr_block = inet("10.0.0.0/16")
}

az("us-east-1a", 1)
az("us-east-1b", 2)

resource aws.subnet "private-${z}" {
  vpc_id = main.id
  cidr_block = inet.subnet(main.cidr_block, 8, n)
  availability_zone = z
} where az(z, n)
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

The `az(..)` lines are facts: two rows in a table. The subnet block ends
in a `where` clause, which makes it a rule: the clause is a query over
the tables, and every answer is one subnet. `?` marks a value apply will
learn, here the VPC's id. Add a third availability zone and a third
subnet follows; nothing else changes.

```bash
cargo run -- -C examples/tour plan
```

The examples in this repository run on a fake cloud built into dform, so
they work from a clean clone with no credentials. `examples/tour` is a
tutorial you read top to bottom.

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

**An attribute can have several authors.** A module sets a tag, a policy
sets another, the environment's settings set a third. They merge per
leaf by rank, `@default` below normal below `@override`. Two authors who
disagree at the same rank are a conflict that names both. The order of
statements never matters.

```dform
set r.tags.team = "platform" @default where r in resource
```

```
$ dform query 'aws.vpc["network.main::vpc"].tags'
{component: "network", env: "prod", team: "platform"}
```

**Policy adds as well as forbids.** The line above is a policy. So is
this one, and both live in the same pack.

```dform
deny "prod databases are multi-AZ" where env == "prod", db in aws.db_instance, not db.multi_az
```

**A value the cloud produces later is a value now.** A database's
endpoint does not exist until the database does. dform carries it as a
labeled unknown, `?aws.db_instance["orders"].endpoint`, plans around it,
and applies in ticks: everything that can be made is made, the unknowns
resolve, the rest is planned again and made. A resource whose *name*
depends on an unknown is a pending group; the plan says so instead of
refusing.

```dform
resource aws.iam_policy "connect-${host}" {
  policy = { Statement: [{ Action: "rds-db:connect", Resource: host }] }
} where db in aws.db_instance, host = db.endpoint
```

```
pending groups:
? aws.iam_policy[?] x unknown, on ?aws.db_instance["orders"].endpoint, resolves after tick 1
```

**Rules recurse.** Which VPCs can reach which, through a transit hub, is
a path of any length. Routes for every pair are three lines, and they
stay correct as spokes come and go.

```dform
reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)

resource aws.route "${a}-to-${b}" {
  route_table_id = aws.route_table[a].id
  destination_cidr_block = aws.vpc[b].cidr_block
  transit_gateway_id = hub.id
} where reaches(a, b), a != b
```

**The plan is a database.** Every fact has a derivation and you can ask
for it, down to the line of source or the row of a CSV.

```
$ dform why 'aws.route["blue-to-green"]'
want("aws.route", "blue-to-green")
  by r49: want("aws.route", Addr) :- reaches(A, B), A != B, ...
  with A = "blue", B = "green"
  ├─ reaches("blue", "green")
  │    by r48: reaches(A, C) :- reaches(A, B), link(B, C)
  │    ...
  │         ├─ spoke("blue")   fact, stacks/network.df:41:1
```

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

**The header.** A file begins with what it is, what it talks to and what
it takes, in that order, before any rule:

```dform
edition 2026
provider aws { region = cfg.region }
import "modules/network.df"
key env: enum("dev", "staging", "prod") = "dev"
input az(name: string, index: int) from yaml("data/azs.yaml")
```

A stack is the unit of state and apply, and a file under `stacks/` is
one, named after itself: this is `stacks/shop.df`, so `dform plan shop`.
A `key` is an input that selects the deployment: each value of `env` has
its own state, `dform plan shop env=prod`. A provider line brings a
provider's types and externs into scope and configures it. Imports bring
in modules and policies. Inputs are the stack's interface. None of these
may appear below the first rule. Where a stack's state lives, whether
unknowns at plan time are refused, and who may approve a plan are
operational, so they live in `dform.toml`, where `[stacks.shop]` is
`stacks/shop.df`:

```toml
[stacks.shop]
backend = 's3("acme-state", "shop/{env}")'
unknowns = "strict"
approvals = 'jwks("https://sso.acme.example/keys")'
```

A small project needs none of this: `dform.toml` beside one `.df` file is
a project with one stack.

**Tables.** Data that is not code comes in as a typed relation from a
file, and rows are facts like any other, each with a line `why` can
point at:

```dform
input az(name: string, index: int) from yaml("data/azs.yaml")
input pin(app: string, image: string) from toml(git("ops.git", "env/${env}", "pins.toml"))
```

```yaml
# data/azs.yaml: a list of objects, one per row
- { name: us-east-1a, index: 1 }
- { name: us-east-1b, index: 2 }
```

```toml
# pins.toml: an array of tables, one per row
[[pin]]
app = "api"
image = "ghcr.io/acme/api@sha256:9f2c..."
```

A cell is checked against its column's type, and a bad row is an error
naming the file and line. A `git(..)` source is read at a commit the
plan records, so apply reads what plan read even if the branch moved.
CSV and JSON work the same way.

**Inputs, settings and lets are cells.** `input env: ..` is a typed
input; `--set env=prod` wins over its default because it is a
higher-rank contribution to the same cell. `settings prod { db.multi_az
= true }` is one environment's values; `let cfg = settings[env]` reads the
selected row, and `cfg.db.multi_az` a leaf of it. Two `let` rows that
disagree are a conflict, like any cell.

**Types and checks.** `type environment = enum("dev", "staging", "prod")`
names a type. `input replicas: int = 2 check 1 <= replicas <= 10` refines
one; a failed check is a deny, with provenance like any other.

**Modules and instances.** A module declares its interface first, then
its body. An instance is one copy, gated by a clause if you like. A
module's resources are not hidden: policy sees them, and stacks wire
modules together through outputs.

```dform
module network {
  input cidr: inet
  output vpc: aws.vpc = vpc

  resource aws.vpc vpc { cidr_block = cidr }
  resource aws.subnet "private-${z}" {
    vpc_id = vpc.id
    cidr_block = inet.subnet(cidr, 8, n)
    availability_zone = z
  } where az(z, n)
}

instance network blue { cidr = inet("10.1.0.0/16") }
instance network green { cidr = inet("10.2.0.0/16") } where env == "prod"
```

A module's resource is addressed as `aws.vpc["network.blue::vpc"]`, the
spelling the plan prints and every command accepts.

**Policies.** `policy baseline { .. }` is a pack of `set`, `deny` and
`warn` statements; `use baseline` applies it. There is no grant to
declare: what a pack touches is visible in the pack and in `dform dev
effects`, and ranks decide who wins.

**Scenarios.** A scenario is policy over hypothetical inputs:

```dform
scenario prod {
  set env = "prod"
  deny "prod peers the two VPCs" where not "blue-green" in aws.vpc_peering_connection
}
```

`dform test` runs every scenario; `dform test --generate` derives cases
from the inputs' types and checks.

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
`data/azs.csv:4`".

**apply** prints the plan and asks. It applies in ticks; at any tick that
adds a resource the first plan could not name, it asks again, showing
only what is new. `--yes` for scripts. State is written after every
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

**test and check.** `dform test` runs the scenarios; `dform check --sarif`
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
sandboxed. A registry is a bucket, the same kind you keep state in, with
signed packages and a lockfile. `dform provider check` is the conformance
suite a provider passes before it is published.

**The editor.** `dform lsp` gives diagnostics, hover with the value of any
term for the selected deployment and who contributed it, jump to a type's
or a function's definition, and the plan's action beside each resource.
`dform fmt` has one normal form per construct.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, then a second run by hand | a pending group; apply runs a second tick |
| a tag on everything, overridable per resource | a variable threaded through every module | `set r.tags.team = "platform" @default where r in resource` |
| "why does this exist?" | read the source, guess | `dform why ADDR` |
| routes from reachability | write them out, keep them in sync | a recursive rule |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| a /20 per team that never moves | a spreadsheet | `allocate`, pinned in state |
| policies tested over every environment | one test per case | `dform test --generate` |
| "can A reach B?" before apply | a separate tool, after the incident | `std.net` reachability as a query |
| adopt four hundred existing subnets | one import block each | `dform import --match` with one rule |
| prove an approved plan is what runs | trust | `dform verify plan.json` |
| a secret that never hits disk | `sensitive = true`, and hope | a label the compiler tracks |

## Where next

- `examples/tour`: the tutorial. Each other example under `examples/`
  shows one thing; its README says which.
- `docs/grammar.md`: the language, for reference.
- `docs/reference.md`: every command and flag, state backends, keyed
  stacks, approvals, the audit log, the controller.
- `docs/design.md`: the model: cells and ranks, unknowns and ticks, the
  plan as a Z-set, provenance. `proposals/` is the record of how it was
  decided.
- `DESIGN.org`: decisions and the roadmap.

dform is pre-release. The language changes without compatibility until
it ships; every `.df` in the repository is rewritten when it does.
