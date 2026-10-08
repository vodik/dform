# Modules: the design, wargamed

Three candidates were weighed: the pitch (one module concept, `use` and
`instance` split by whether the module has inputs), the user's direction
(module = file of items, `component` = an instantiable item inside it) and
a hybrid. The hybrid is recommended; the definition is section 1, the
reasoning is section 4, the ovh-infra files are section 3.

## 1. The definition (README wording)

A **module** is a `.df` file, named by its path from the project root with
dots (`config`, `modules.net`, `stacks.platform`): a namespace of items, which
are types, relations, rules and denies, `let` values, and components. `use
M` brings a module into my scope: its items are mine to read as `M.x`, and
its rules and denies run over what I can see.

A **component** is the thing that is copied: `component NAME { input ..;
body }`, an item of a module, or a whole file whose top level declares an
`input`. `instance M.NAME name { inputs }` makes one copy with its inputs
bound, named `name` (or named after the component when it is used once);
a copy's resources are `name::x`, its outputs are `name.x`, and
`M.NAME[t].x` ranges over every copy.

A **stack** is a component the tool instances: a file under `stacks/` (or
named in `dform.toml`), whose `key` inputs are the deployment's key. `use
stacks.platform` binds to its deployments, and `platform[env=env].x` reads
one of them.

`std` is a module tree every scope has already used, so `str.split` needs
no `use`.

## 2. Grammar and lowering

    use       := "use" path ("as" NAME)? ("where" body)?
    instance  := "instance" path NAME? block? ("where" body)?
    component := "component" NAME "{" body "}"          ; an item; a file with `input` is one
    path      := NAME ("." NAME)*                        ; a/b.df from the root; std.x; a package mount

- A path is a lookup, never a search: `modules.net` is `modules/net.df`;
  `std.x` is the standard library; a name in dform.toml `[packages]` mounts
  a registry package or another project (`[packages.infra] path =
  "../infra"`, then `use infra.stacks.platform`). The last segment is the
  scope name unless `as` renames it. A project module whose last segment is
  a std module name is an error ("rename it: std.str is always in scope").
- `use` of a file that declares `input` is an error ("postgres is a
  component; `instance` it"); `instance` of a path that is a module or a
  non-component item is an error ("config is a module; `use` it"). So
  `instance config` and `instance list` cannot be written.
- `use a.b where B` lowers b's items under scope `S = <mine>.b`: its rules
  and denies with predicates `S::p`, reading any name b does not define
  outward through my scopes to the stack, then std, then provider types
  (that outward read is why a policy pack needs no inputs and why `env`
  in it is the user's); its `let` values and types as public names `b.x`;
  its components as `b.c`, instantiable. `use` twice in one scope is an
  error; from two scopes it is two activations, each reading from its own
  user. A module holds no resources: a resource at a module's top level is
  an error ("a resource lives in a component or a stack"), which is the
  rule that prevents the double-deploy the pitch had to lint for.
- `instance a.b n { k = V } where B` lowers b's body under `S = <mine>.n`
  (`<mine>.b` with no name): resources `n::x` (address `T["n::x"]`),
  private predicates `S::p`, `input k` as the cell `(input, S, k)` with the
  block's `k = V :- B` at normal rank and the default at `@default`,
  outputs `output(S, k, V)` read as `n.k`, existence gated by `B`, and the
  fact `instance_of("a.b", "n")`, so `b[t]` enumerates and `b["n"]` is
  `n`. Instances mirror resources: a resource is `main` or
  `net.vpc["main"]`, an instance is `blue` or `network["blue"]`; names in
  one scope are one namespace, so two instances named `main` clash.
- `use stacks.platform` lowers to a dependency edge (apply order, R-30)
  and a name; `platform[env=e].k` is the existing remote read
  `remote_output("platform", {env: e}, k, V)` from that deployment's state
  at plan time, secrets by R-45's reveal; a stack with no keys reads as
  `platform.k`. A program cannot `instance` a stack ("stacks.platform is
  deployed by the tool; `use` it"). This is the one `use` of a component,
  stated as the stack rule, not an exception.
- `provider k8s` is already the same shape (a path activated by a
  statement, names under the last segment); the grammar says so once.
- `export` is deleted: a module's items are public, a component's
  internals are private except its outputs.

## 3. ovh-infra under it

Layout: `dform.toml`, `config.df`, `baseline.df`, `postgres.df`,
`synapse.df`, `forgejo.df`, `traefik.df`, `stacks/platform.df`,
`stacks/apps.df`. Each app is a component file (its top level has
`input`), config and baseline are modules. `modules/` would only add
`modules.` to every path.

`config.df`:

```dform
edition 2026

#| What every stack shares. `use config`, then `config.base_domain`.
#| Credentials are the ovh provider's own (~/.ovh.conf).
let base_domain = "vodik.xyz"
let admin_email = "admin@vodik.xyz"
# Toronto; BHS5 is the Quebec fallback if Managed Kubernetes is not there yet.
let region: enum("ca-east-tor", "BHS5", "GRA11") = "ca-east-tor"
let ovh_project = "k8s"
```

`baseline.df` is today's file unchanged (`set .. @default`, the denies,
`container`, `requires_approval`); `env` in it is the using stack's.
`postgres.df`, `synapse.df`, `forgejo.df`, `traefik.df` are today's files
with `export type conn` deleted (`postgres.conn` is public as a type).

`stacks/platform.df`:

```dform
edition 2026

#| The OVH project: a private network, one Managed Kubernetes cluster and
#| its node pool, the ingress on a node's public IP.
key env: enum("lab", "prod") = "lab"
input nodes {
  flavor: string = "b3-8"
  count: int = 1 check 1 <= count <= 3
}

settings { nodes.count = 2 } where env == "prod"

use config
use baseline

provider ovh { endpoint = "ovh-eu", project = config.ovh_project }

resource ovh.network main {
  name = "k8s-${env}"
  region = config.region
  vlan_id = 10
}

resource ovh.subnet nodes {
  network = main
  region = config.region
  range = "10.10.0.0/24"
  dhcp = true
}

resource ovh.kube main {
  name = "k8s-${env}"
  region = config.region
  network = main
  subnet = nodes
  version = "1.31"
}

resource ovh.kube_nodepool default {
  cluster = main
  name = "default"
  flavor = nodes.flavor
  desired = nodes.count
  min = nodes.count
  max = nodes.count + 1
  autoscale = true
}

lifecycle(main, "prevent_destroy") where env == "prod"

# The kubeconfig is a secret the ovh provider holds; k8s is configured
# from it, so everything below waits for tick 1.
provider k8s { kubeconfig = main.kubeconfig }

instance traefik {
  host_network = true
  acme_email = config.admin_email
  storage_class = "block"
}

resource k8s.storage_class block {
  metadata.name = "block"
  provisioner = "cinder.csi.openstack.org"
  parameters.type = "high-speed"
  reclaimPolicy = "Retain"
}

# The ingress node's public address, read once the nodes exist (tick 2).
let ingress_ip = list.min([
  a.address |
  node in world.k8s.node,
  a in node.status.addresses,
  a.type == "ExternalIP",
])

output cluster: ovh.kube = main
output ingress_ip: ip = ingress_ip
```

`stacks/apps.df`:

```dform
edition 2026

#| What runs on the cluster: a Matrix homeserver and a git forge.
key env: enum("lab", "prod") = "lab"
input synapse {
  replicas: int = 1 check 1 <= replicas <= 2
  storage_gb: int = 20
}
input forgejo {
  storage_gb: int = 20
}

settings {
  synapse.storage_gb = 100,
  forgejo.storage_gb = 50,
} where env == "prod"

use config
use baseline
use stacks.platform

provider random
provider ovh { endpoint = "ovh-eu", project = config.ovh_project }
provider k8s { kubeconfig = platform[env=env].cluster.kubeconfig }

resource k8s.namespace apps { metadata.name = "apps" }

instance postgres synapse_db {
  namespace = apps
  name = "synapse-db"
  database = "synapse"
  storage_gb = synapse.storage_gb
}

instance synapse matrix {
  namespace = apps
  server_name = config.base_domain
  host = host["matrix"]
  db = synapse_db.conn
  replicas = synapse.replicas
  media_storage_gb = synapse.storage_gb
}

instance forgejo git {
  namespace = apps
  host = host["git"]
  storage_gb = forgejo.storage_gb
  ssh_port = 2222
}

deny "prod keeps its data" { resource: r } where {
  env == "prod"
  deformation(kind, r, _)
  kind in ["delete", "replace"]
  r in k8s.persistent_volume_claim
}

# Each app's name and the record that points it at the ingress; a record
# exists only while its app does.
host(sub, "${sub}.${config.base_domain}") where sub in ["matrix", "git"]

resource ovh.domain_record "${name}" {
  zone = config.base_domain
  subdomain = sub
  type = "A"
  target = platform[env=env].ingress_ip
  ttl = 300
} where host(sub, name)
```

Two creaks found by writing it. Both instances used to be named `main`;
instance names are one namespace per scope, so they are `matrix` and `git`
now, and `instance synapse` alone would also do. And `host["matrix"]` in an
instance block is a read of the stack's relation from inside the block,
which is fine (blocks read outward) but worth a grammar sentence.

## 4. The three candidates

**The pitch** (one concept; `use` for no inputs, `instance` for inputs).
Clean lowering, two keywords, but the kind of a file is implicit in whether
it has an `input`, `modules/network.df` wants to be both a namespace
(types) and a template, and a module holding several templates needs
several files.

**The direction** (module = file of items; `component` = item). It fixes
`instance config` by construction: a module is never instanced, only its
components are. It fixes "a file that is three related templates"
(`modules/net.df` with `component vpc`, `component subnet`). Its cost is
the common case: a file that is one component reads `network.network`, and
every small project has five of those (postgres, synapse, forgejo, traefik,
and every app anyone writes). Rust pays this (`network::Network`) and
mitigates with case; dform's names are all lowercase, so the doubling is
bare.

**The hybrid** (recommended): the direction, plus one rule: a file whose top
level declares an `input` is itself a component, named by its path. That
is not a new kind; a stack is exactly this already (a file with `key` and
`input`, instanced by the tool), so the rule generalises what stacks/ does
rather than adding to it. `postgres.df` is `instance postgres db {}`;
`modules/net.df` with items is `instance modules.net.vpc main {}`. The
error for `use postgres` says "postgres is a component; `instance` it".
File-level `input` in a non-stack file is no longer an error, which the
direction had to make it; the error that remains is a resource at the top
level of a module ("a resource lives in a component or a stack"), and that
one earns its place, because it is what makes `use` from two stacks safe
without a lint.

**The word.** `component` reads right: it is Pulumi's and CDK's word for
exactly this (a parameterised bundle of resources instanced by name), it
is not Terraform's, and "instance of a component" is plain English.
`template` says text, `form` collides with the product's name and reads as
a questionnaire, `part` and `kit` say nothing. Keep `component`.

**Policy packs with inputs**: a component whose body is denies, `instance
prod_rules { threshold = 3 }`, used once and named `prod_rules`; without
inputs a pack is a module and `use baseline` is right. A pack is never a
third thing.

**The keyed read and iteration**: unchanged. `network[t].vpc` is the
enumeration of a component's instances; with the component inside a
module it is `net.vpc[t].cidr` after `use modules.net`. `instance_of`
carries the component's path.

**Stack reading stack**: still `use stacks.platform` then
`platform[env=env].x`. It is a `use` of a component, allowed only because
the component is deployed: the tool made the instances, `use` binds to
them. One sentence in the grammar, no second mechanism.

**What the direction left open, decided**: `let` at a module's top level is
the module's value (`config.region`), `output` belongs to components and
stacks only; a module's items are public and a component's internals are
private but its outputs; `use` and `instance` are body statements with a
clause, header unchanged (`edition`, `key`, `input`); `use` cycles are a
load error; a module may hold rules that read the user's resources
(baseline's `container`), that is the point; a component may `use` modules
and `instance` components; a component cannot hold a `key`.

## 5. Tickets now wrong

- R-37: amendment 2 (delete `use`, anonymous `instance m` for all)
  reversed: `use` for modules, `instance` for components, anonymous only
  for a component used once; amendment 3's stem names and "inline first,
  else modules/x.df" replaced by paths from the root and lookup; "a policy
  is a module you use" stands as written.
- R-64: amendment 1's stems and duplicate-stem error replaced by paths;
  (3) a keyed read needs `use stacks.platform`; (4) `use baseline`, not
  `instance baseline`; amendment 2 (stacks/ convention, dform.toml breaks
  it) stands; add: a stack is a component file.
- R-11b: `stacks.REMOTE.NAME[k=v]` becomes `[packages.infra]` in dform.toml
  and `use infra.stacks.platform`.
- R-55: the instance block is unchanged; `use` takes no block.
- R-29: "the file names the stack" becomes "the path names the module;
  `stacks/` makes a component file a stack"; `key` unchanged; `dform apply
  platform` stays as the stem shorthand among stacks.
- R-26: `use config` and `instance traefik` with nothing to bind are
  braceless; unchanged.
- README.next.md "Modules": the inline `module network { .. }` example
  becomes `component network { .. }` with `instance network blue` as is;
  "Policies": `use baseline`; the `stacks.` reads become `use stacks.net`
  then `net[env=env].vpc`; delete `export`. grammar.md: one "Modules"
  section (modules, components, instances, paths, outward reads, privacy,
  the stack rule), "Stacks and keys" folded in as "Deployed components".
- `module` as a keyword goes; `component` arrives. The keyword count is
  unchanged.

## 6. How far stack and component are one thing

They share everything that is language, and nothing that is tool.

(a) One mechanism for both: instantiation (a body under a scope with inputs
bound as cells, defaults at `@default`), the scope and address prefixes
(`n::x`, `T["n::x"]`), private predicates, outputs (`output(S, k, V)`),
the keyed read (`network[t].vpc` and `platform[env=env].x` are one read,
`instance_of(path, key)` joined to `output`, the key a name when the
program bound it and the key inputs when the tool did), lifecycle facts
(`lifecycle(r, ..)`, `deformation` rows, `requires_approval`) which never
cared which scope a resource came from, the outward read of names, and
`why`, which prints an instance frame the same way for both. The engine
should have one `Instance` and no `Stack` carrier, which is what the
R-30 leftover ticket asks for anyway.

(b) Deployment only: `key` (identity: the thing the tool instances by,
written to state, never bound by a program), the backend and the state
it holds, approvals and their digest, per-tick confirmation, the plan
file, the audit log, `settings` from dform.toml, and the remote read being
served from state rather than evaluated. None of that is a language
feature; it is what the tool wraps around an instance to make it
durable.

So "a stack is a component the tool instantiates, one deployment per key"
is true under this design, and it is the cleaner statement, not "a module
whose body is one anonymous component": a stack file *is* a component
file (its top level has `key` and `input`), the hybrid's rule makes any
such file a component, and `stacks/` or dform.toml says the tool is the
one that instances it. Nothing is anonymous; the deployment is the named
instance, named by its key. The lean-no instinct is right about the
surface, though: the word "stack" stays, because people deploy stacks
and instance components, and `key` stays a stack-only word, because it
names the one difference (who binds it). The generalisation is in the
engine, not the vocabulary.

## 7. Recommendation

Adopt the hybrid: modules are files of items and are `use`d; components
are the copies, declared as items or as whole files with an `input`, and
are `instance`d, anonymously when used once; stacks are component files
the tool instances, and `use stacks.X` binds to those instances; paths from
the project root, looked up; items public, outputs the only public face of
a component, `export` gone; no resource at a module's top level. Two
keywords, two kinds, one directory with meaning, and `instance config`,
`use postgres` and `network.network` are each impossible to write.
