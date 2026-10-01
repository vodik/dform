# dform

dform is a tool for describing infrastructure and making it so. You write
what should exist. dform compares that with what does exist, shows you the
difference, and applies it. That part is Terraform's job, and dform does
it the same way: plan, review, apply, state.

The difference is the language. A dform program is facts and rules, a
small Datalog. Rules can read everything the program says should exist,
modules included, and every rule is evaluated at once. An attribute can
have several authors. A policy can add things, not only forbid them. A
rule can call itself. A value only the cloud knows is a value, not an
error. And the plan is a database: you can ask it why anything is there.

Here is a whole program.

```dform
edition 2026

provider fake {}
stack tour[env] {}
input env: enum("dev", "prod") = "dev"

resource net.vpc main {
  cidr = inet("10.0.0.0/16")
}

zone("us-test-1a", 1)
zone("us-test-1b", 2)

resource net.subnet "private-${z}" {
  vpc_id = main.id
  cidr = inet.subnet(main.cidr, 8, n)
  zone = z
} where zone(z, n)
```

```
$ dform plan
plan: 3 deformations (3 create)
+ net.vpc["main"]
  cidr = "10.0.0.0/16"
+ net.subnet["private-us-test-1a"]
  cidr = "10.0.1.0/24"
  vpc_id = ?net.vpc["main"].id
  zone = "us-test-1a"
+ net.subnet["private-us-test-1b"]
  cidr = "10.0.2.0/24"
  vpc_id = ?net.vpc["main"].id
  zone = "us-test-1b"
```

`zone(..)` lines are facts, rows in a table. The subnet block has a
`where` clause, so it is a rule: one subnet per row that matches. `?` marks
a value apply will learn. Add a third zone and a third subnet follows.

The cloud here is a fake one built into dform. Nothing to sign up for;
the examples run from a clean clone.

```bash
cargo run -- -C examples/tour plan
```

`examples/tour/stacks/tour.df` is a tutorial you read top to bottom. The
rest of this page is why dform is a language, what the language is, what
the tool does, and what you can do with it that you cannot do elsewhere.

## Why a language

Infrastructure tools keep running into the same wall: the configuration
format cannot say the thing you need, so you reach around it. Terraform
has `for_each`, `dynamic` blocks, `depends_on`, `-target`, `moved`, and a
provider feature just for default tags, each one a patch over something
HCL cannot express. Pulumi answers with a general-purpose language and
loses the ability to reason about the program. dform's answer is a
language small enough to reason about and expressive enough not to need
patches. Six things make the difference.

**Everything is evaluated at once.** A rule can read any resource in the
program, in any module, whether it is declared above or below. This is
not an ordering trick; it is what a Datalog fixpoint is.

```dform
output private_subnet_ids: list(ref(net.subnet)) =
  [ s.id | s in net.subnet, s.visibility == "private" ]
```

**An attribute can have several authors.** A module sets a tag, a policy
sets another, the environment's settings set a third. They merge, per
leaf, by rank: `@default` below normal below `@override`. Two authors who
disagree at the same rank are a conflict that names both. Order never
matters.

```dform
set r.tags.team = "platform" @default where r in resource
```

```
$ dform query 'net.vpc["network.main::vpc"].tags'
{component: "network", env: "prod", team: "platform"}
```

**Policy can add as well as forbid.** The line above is a policy. So is
this one, and both live in the same pack:

```dform
deny "prod db must be multi_az" where env == "prod", pg in db.postgres, not pg.multi_az
```

**A value the cloud knows later is a value now.** A VPC's id does not
exist until the VPC does. dform carries it as a labeled unknown,
`?net.vpc["main"].id`, plans around it, and applies in ticks: everything
that can be made is made, the unknowns resolve, the rest is planned again
and made. A resource whose *name* depends on an unknown is a pending
group, and the plan says so instead of refusing.

```
pending groups:
? iam.policy[?] x unknown, on ?db.postgres["orders"].endpoint, resolves after tick 1
```

**Rules recurse.** Which networks can reach which, through a hub, is a
path of any length. Terraform has no way to say it; this is three lines.

```dform
reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)

resource net.route "${a}-to-${b}" {
  destination = net.vpc[v].cidr
} where reaches(a, b), a != b, network_of(b, v)
```

**The plan is a database.** Every fact has a derivation, and you can ask
for it.

```
$ dform why 'net.route["blue-to-green"]'
want("net.route", "blue-to-green")
  by r49: want("net.route", Addr) :- reaches(A, B), A != B, network_of(B, V), ...
  with A = "blue", B = "green", V = "network.green::vpc"
  ├─ reaches("blue", "green")
  │    by r48: reaches(A, C) :- reaches(A, B), link(B, C)
  ...
  │              ├─ spoke("blue")   fact, stacks/tour.df:264:1
```

## The language

This is the tour, condensed. `examples/tour` has every section with the
command to run and what it prints; `docs/grammar.md` is the reference.

**Facts and rules.** A fact is a row: `zone("us-test-1a", 1)`. A rule
derives rows: `link(h, t) where hub(h), spoke(t)`. Lower-case names are
variables, bound where they first appear. Constants are quoted. A variable
used only once in a rule is an error, because in a language whose output
is cloud resources, a typo must not become a cross product.

**Resources.** `resource TYPE name { attr = value ... }` says a resource
should exist. With a `where` clause it is a rule: one resource per answer,
named by the header, which may interpolate the clause's variables:
`resource net.subnet "private-${z}" { .. } where zone(z, n)`.

**References and reads.** `vpc_id = main.id` is a reference: an edge in
the apply order, and an unknown until the VPC exists. `inet.subnet(main.cidr, 8, n)`
reads the cidr now, because the function needs its bytes. A dot is a
reference where it stands as a whole value and a read where its content
is used. The compiler tells you when a read of a computed value makes a
block wait a tick.

**Inputs, settings and lets are cells.** `input env: enum("dev", "prod") = "dev"`
is a typed input; `--set env=prod` on the command line wins over the
default because it is a higher-rank contribution to the same cell.
`settings prod { db.multi_az = true }` is one environment's values;
`let cfg = settings[env]` reads the selected row, and `cfg.db.multi_az`
reads a leaf of it. Two `let` rows that disagree are a conflict, like any
cell.

**Types and checks.** `type environment = enum("dev", "stg", "prod")`
names a type. `input replicas: int = 2 check 1 <= replicas <= 10` refines
one; the check is a deny when it fails, so it has provenance like any
other.

**Modules and instances.** A module declares its interface first:
inputs, outputs. An instance is one copy, gated by a clause if you like.
A module's resources are not hidden: policy sees them, and a stack wires
one module to another through outputs.

```dform
module network {
  input vpc_net: inet
  output vpc: net.vpc = vpc

  resource net.vpc vpc { cidr = vpc_net }
  resource net.subnet "private-${z}" {
    vpc_id = vpc.id
    cidr = inet.subnet(vpc_net, 8, n)
  } where zone(z, n)
}

instance network blue { vpc_net = inet("10.1.0.0/16") }
instance network green { vpc_net = inet("10.2.0.0/16") } where env == "prod"
```

A module's resource is addressed as `net.vpc["network.blue::vpc"]`, the
spelling the plan prints and every command accepts.

**Policies.** `policy baseline { .. }` is a pack of `set`, `deny` and
`warn` statements; `use baseline` applies it. What a pack touches is
visible in the pack and in `dform dev effects`; there is no grant to
declare, because ranks decide who wins.

**Scenarios.** `scenario prod { set env = "prod"  deny ".." where .. }`
is policy over hypothetical inputs. `dform test` runs every scenario;
`dform test --generate` derives cases from the inputs' types and checks.

**Providers and functions.** `provider fake {}` brings a provider's types
and its externs into scope. An extern is a relation the provider answers
on demand, with binding modes: `file.json[path]` reads a file,
`random.password[key]` is a secret that is generated once and kept.
Functions are qualified by the type they are about, `inet.subnet`,
`str.split`, `list.join`, and declared in signature files you can jump
to from the editor. Constructors are named by their type: `int(s)`,
`inet(s)`.

**Secrets.** A sensitive value never leaves the provider as bytes. The
compiler tracks where secrets flow and refuses a program that would print
one, compare one, or put one in a public attribute, before anything runs.

## The tool

**A project** is a directory with a `dform.toml`: `stacks/` (one stack
per file), `modules/`, `policies/`, `config/`, `data/`, `providers/`, and a
gitignored `dform.state/`. `dform init` makes one. A command runs on a
target: a stack by name, or one deployment of a keyed stack,
`dform plan shop env=prod`.

**plan** prints the difference between the program and the world:
creates, updates, deletes and replaces, grouped by resource; what is
pending on an unknown; what cannot be decided yet; conflicts; and the
apply order by tick. `--json` for machines. `--why` prints under each
change the rule and the base facts that caused it: "because
`data/zones.csv:4`".

**apply** prints the plan and asks. It applies in ticks; at any tick that
adds a resource the first plan could not name, it asks again, showing
only what is new. `--yes` for scripts. State is written after every call,
so an interrupted apply resumes. `--parallel N` overlaps independent
calls.

**Plan files and approvals.** `plan --out plan.json` writes a file that
records everything the plan depended on. `apply plan.json` refuses if the
world or the inputs moved. Policy can say a change needs approval; an
approver signs the plan's digest; `apply --approval` verifies it offline
against the stack's trust root. `dform verify plan.json` recomputes a plan
from its file alone, with no cloud access.

**why and query.** `dform why ADDR` explains a resource; `dform why
'deny(m)'` explains a refusal. `dform query 'attr(net.subnet, s, "zone", z)'`
asks the fact store anything. `dform diff --since 2026-09-20` explains
what changed between applies and why.

**test and check.** `dform test` runs the scenarios; `dform check --sarif`
runs the policies for CI.

**State and stacks.** `dform stack list`, `dform state show`, `state mv`,
`stack rekey`. State lives in a directory or an S3 bucket with conditional
writes and a lease. A stack keyed by `env` is one deployment per value,
each with its own state. Another stack's outputs are read as
`stacks.platform.cluster[env="prod"].endpoint`.

**The controller.** `dform controller run` watches the inputs and the
world and applies the same plan a batch run would, continuously. The same
program bootstraps a cluster in batch mode and later runs inside it.

**Providers.** A provider is a wasm component: one file, any platform,
sandboxed. A registry is a bucket, the same kind you keep state in, with
signed packages and a lockfile. `dform provider check` is the conformance
suite a provider passes before it is published.

**The editor.** `dform lsp` gives diagnostics, hover with the value of any
term for the selected deployment and who contributed it, jump to a type's
or function's definition, and the plan's action beside each resource.
`dform fmt` has one normal form per construct.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, two runs by hand | a pending group; apply runs a second tick |
| a tag on everything, overridable per resource | thread a variable through every module | `set r.tags.team = "platform" @default where r in resource` |
| "why does this exist?" | read the source, guess | `dform why ADDR` |
| routes from reachability | write them out, keep them in sync | a recursive rule |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| a /20 per team that never moves | a spreadsheet | `allocate`, pinned in state |
| policies tested over every environment | one test per case | `dform test --generate` |
| "can A reach B?" before apply | a separate tool after the incident | `std.net` reachability as a query |
| adopt four hundred existing subnets | one import block each | `dform import --match` with one rule |
| prove an approved plan is what runs | trust | `dform verify plan.json` |
| a secret that never hits disk | `sensitive = true`, hope | a label the compiler tracks |

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
