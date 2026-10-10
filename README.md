# dform

Infrastructure is a database. An account is a table of networks, a
table of instances, a table of DNS records, each row with attributes and
pointing at others. What you want is a set of tables too, and a plan is
the difference between the two. A policy is a query that must return no
rows. "Why is this here" is a question about which rows produced it.

A dform program is facts and rules over those tables, evaluated all at
once to a fixpoint, the way Datalog evaluates a query. Every plan is the
least model of the program over the world as it is, and every line of it
carries its proof, so `why` never disagrees with the plan. Apply
reconciles the world with the plan in ticks, and what one tick creates,
an endpoint or a kubeconfig, is a value the next tick plans with. Each
tick is the least model over the world the one before it left.

Here is a complete program, on the AWS-shaped mock:

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

deny "no public subnets" { subnet: s } where s in aws.subnet, s.map_public_ip_on_launch
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

The subnet block ends in `where`, so it makes one subnet per zone the
provider lists, and when the region gains a zone the next plan has one
more subnet with no edit to the file. The examples run on a fake cloud
built into dform with no credentials. To try one, `cargo install --path
.`, then `dform -C examples/tour plan`.

## Why a rule, not a template

In HCL a resource block is a template, patched by `for_each`,
`depends_on`, `-target` and `default_tags` wherever a template cannot
say what you mean. In dform a block is a rule, and the rest follows.

**Every rule sees every resource, and an attribute has many authors.**
A team's conventions are written once and hold in every module, without
a variable threaded through each one. A `set` writes into any resource
its clause matches, merged per leaf with what the module wrote itself:

```dform
set r.tags = { team: "shop" } where r in resource
```

```
$ dform query 'net.vpc["blue.vpc"].tags'
{ team: "shop", component: "network" }
```

Terraform's `default_tags` does this for tags on one provider; here any
attribute of any resource can be written so.

**Policy adds as well as forbids, in the same file.** Rules about the
change itself sit beside the resources, with no plan JSON exported to a
second engine. The plan is a table of `deformation` rows, so "no deletes
in prod" is `deny "no deletes in prod" where env == "prod",
deformation("delete", _, _)` over this:

```
$ dform query 'deformation(k, r, _)' --set database.backup_days=7
K         R
"update"  db.postgres orders
```

A policy engine over plan JSON passes or guesses at a value known only
after apply. The logic has three values, and nothing is assumed false
for being unknown, so a dform policy over such a value is undetermined
and the plan names the tick that decides it.

**Rules recurse.** Anything shaped like a graph, such as routes over
which networks reach which, is derived and stays right as spokes come
and go. HCL has no recursion, so these are written out by hand:

```dform
link(h, t) where hub(h), spoke(t)
link(t, h) where hub(h), spoke(t)

reaches(a, b) where link(a, b)
reaches(a, c) where reaches(a, b), link(b, c)
```

**A value the cloud produces later is a value now.** One apply makes
what depends on a value the cloud has not assigned yet, and the plan
says how many rounds it will take. What waits names what it waits on
and lands in a later tick:

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

In Terraform a `for_each` over such a value fails with "cannot be
determined until apply", and the way out is `-target` and a second run.
A provider's settings are values too, so `use k8s { kubeconfig =
k3s.kubeconfig }` puts a cluster and what runs on it in one program,
where Terraform's guidance is two root modules.

**Everything explains itself, absence included.** The question you ask
in an incident, why is this missing, has an answer. `dform why` names
the rule that could have made it and the condition that failed:

```
$ dform why 'net.subnet private-us-test-1c'
net.subnet private-us-test-1c: no rule derives it
  stacks/tour.df:104  resource net.subnet "private-${z}" { .. } where zone(z, n)
    zone("us-test-1c", n): no row
    nearest: ("us-test-1a", 1), ("us-test-1b", 2)
```

## The language

**Paths.** A baseline for every container in every deployment is one
line and its exception one more, each written where it belongs. `[_]`
ranges over every element of a list and `[k]` takes one by its key:

```dform
let limits = { cpu: 500m, memory: 256Mi }

set k8s.deployment[_].spec.template.spec.containers[_].resources.limits = limits @default
set k8s.deployment["web"].spec.template.spec.containers["web"].resources.limits = {
  ..limits,
  memory: 1Gi,
}
```

A Helm chart gets the same only for the values its author exposed.

**Documents.** Config others keep, CRDs at a pinned release and a
kubeconfig on a host still booting are read the same way, by path or
uri, and decoded to typed rows that keep their file and line. A read
that is not ready yet waits for the tick that makes it:

```dform
input peering from csv.decode(io.read("data/peerings.csv"))

let net = toml.decode(io.read("data/network.toml"))
set from yaml.decode(io.read("config/${env}.yaml"))

let raw: secret(string) = io.read("ssh://ubuntu@${server.public_ip}/etc/rancher/k3s/k3s.yaml")

resource k8s.custom_resource_definition "${d.metadata.name}" = d where {
  d in yaml.decode(io.read("git+https://github.com/traefik/traefik/docs/crds.yml?ref=v3.7.14"))
}
```

Terraform reads a local file with `file()`, but a file on a host it
just booted takes a provisioner and a `null_resource`.

**Environments.** One file says how lab and prod differ, and each value
of a `key` is its own deployment with its own state. A `set` under a
condition overrides an input's default:

```dform
key env: enum("lab", "prod") = "lab"
input agents: int = 0 check 0 <= agents <= 3
input sizes { synapse: bytes = 20Gi, forgejo: bytes = 20Gi }
output ingress_ip: ip = k3s.ingress_ip

set { agents = 1, sizes.synapse = 100Gi } where env == "prod"
```

Terraform spreads the same across workspaces and a `.tfvars` per
environment.

**Secrets.** A secret cannot reach an output, an address or a count by
accident. The compiler follows every value made from one, and a leak
is an error at its line:

```dform
output password: string = pw                           # E0304: not declared secret(string)

deny "short password" where pw.len < 12               # E0301: inspecting it leaks it
n(c) where c = count(p), p = pw                        # E0303: a count leaks cardinality
resource aws.iam_user "u-${pw}" { name = "x" }         # E0305: addresses are printed
```

Terraform's `sensitive` redacts what it prints, and the value is still
in state in the clear. dform's state holds no secret, because a
generated one derives from the deployment's master, and `dform secrets
rotate D KEY` changes one, one plan line per place it lands.

**Tests.** Every deny is checked in every environment before anything
ships. `dform test` runs the program once per combination of its enums,
bools and keys, against an empty world:

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

`terraform test` and OPA's tests run the cases you write; this one
enumerates them. `examples/tour` has each idea with the command to run,
and `docs/grammar.md` is the language's reference.

## The tool

**Stacks and deployments.** One command plans and applies a whole
estate, the cluster and what runs on it, each deployment with its own
plan, question and state, in dependency order. Which deployments exist
is code. `project.df` lists them as resources, `resource stacks.platform
lab { env = "lab" }`, and a stack reads another's outputs as
`platform[env].kubeconfig`, so `dform apply apps env=lab` applies
platform first. Terraform does this with root modules and remote state,
and Terragrunt with `dependency` blocks written by hand. Pulumi's stack
references link stacks one by one with no matrix, and Argo CD's
ApplicationSets have the matrix for manifests only.

**apply** shows the plan and asks once, and again only at a tick whose
plan it could not show in full. An interrupted apply resumes where it
stopped, and a create whose answer was lost is found again, never made
twice.

**destroy** is the plan against an empty program, so a deny over
deletes refuses it. Lifecycle is facts policy can condition,
`lifecycle(k3s.server, "prevent_destroy") where env == "prod"`, where
Terraform's `prevent_destroy` takes only a literal.

**status** gives a bird's-eye view of your managed resources' health,
for those whose provider reports it: `healthy`, `progressing`, or
`degraded` with the reason, `CrashLoopBackOff: container migrate`. It
reads like Argo CD's health view, across everything dform manages, and
it fails unless all is well, so it fits a pipeline step or a cron job.

**render** prints a Kubernetes stack as manifests under the program's
policy, with no provider, credentials or state, so GitOps keeps one
source of truth. `dform render apps env=lab` is a YAML stream for Argo
CD's config management plugin or `kubectl apply -f -`, where Helm or
Kustomize would be a second language beside the infrastructure.

**Approvals.** A risky change waits for a person, and the signature is
over the plan's digest, so what was approved is what applies.
`requires_approval(r, reason)` holds the change until `apply --approval
FILE` brings it, where Atlantis gates on a pull request review instead.

**Providers** of your own are written against the SDK in
`docs/providers.md`, and `dform provider check` runs the conformance
suite on them. `docs/reference.md` has every command and flag.

## What you cannot do elsewhere

| You want | The usual workaround | In dform |
|---|---|---|
| a resource per value only apply knows | `-target`, then a second run | it plans in the next tick |
| how many rounds an apply takes | find out during it | the plan is grouped by tick |
| a cluster and what runs on it, in one plan | two root modules | `use k8s { kubeconfig = k3s.kubeconfig }` |
| a tag on everything, overridable | a variable threaded through every module | `set r.tags.team = "shop" @default where r in resource` |
| a policy that sees inside modules | export every value as an output | policy reads any resource |
| rules about the change set | plan JSON through a policy engine | denies over `deformation` rows; the plan says what holds, fails and cannot be known yet |
| "why is this here?" | read the source | every plan line cites its file and line; `dform why ADDR` |
| "why is this not here?" | read the source harder | `dform why` names the condition that failed |
| routes from reachability | write them out by hand | a recursive rule |
| policies tested over every environment | one test per case | `dform test` runs every combination of enums, bools and keys |
| one generated password rotated | taint and hope nothing else moves | `dform secrets rotate D KEY` |
| a state file that leaks nothing | encrypt the bucket | state holds no secret |
| a key the cloud mints, written into another resource | it lands in state in the clear | the provider holds it; dform reveals it into the one call that writes it |
| health after a deploy | apply waits on it, or a second tool | `dform status`, asked when you want it |
| manifests for GitOps, under your policy | a template engine beside the infrastructure tool | `dform render`: no provider, no state |

## What it is not

- Not a general-purpose language. There are no loops, no mutation and
  no effects.
- Not a configuration manager. It runs nothing on a host; cloud-init in
  `user_data` and the cluster's own controllers do that.
- Not a Helm. A manifest of one kind is a resource per document; a
  chart's render of mixed kinds is not read by kind.
- Not a proof. `dform test` enumerates enums, bools and keys and leaves
  every other input at its default.
- Not finished. It is pre-release. The language changes without
  compatibility, and every `.df` here is rewritten when it does. Few
  providers are real yet. Controller mode, `dform controller run`, is
  experimental, behind `DFORM_EXPERIMENTAL=1`.

## Where next

- `examples/tour`: the tutorial. `examples/crud-api`: a blue/green
  rollout gated on a migration Job. Each example's README says what it
  shows and the commands to run.
- `docs/grammar.md`: the language. `docs/reference.md`: every command
  and how to run dform. `docs/best_practices.md`: how to write a
  program.
- `proposals/` and `DESIGN.org`: the model and the decisions.

Apache-2.0. Contributions under the Developer Certificate of Origin
(`git commit -s`); there is no CLA.
