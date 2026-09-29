# dform / Datalog Best Practices

This document captures the patterns that make Datalog-style configuration scale.
These are the same ideas that make Terraform modules practical: reuse, composition,
and predictable behavior.

## Model Your Branching as Data

If you see lots of repeated guards like:

```dform
set pg.backup_days = 14 if env == "prod", pg in db.postgres
set pg.backup_days = 3 if env == "staging", pg in db.postgres
```

Prefer a lookup table:

```dform
settings prod {
  db.backup_days = 14
}

settings staging {
  db.backup_days = 3
}

set pg.backup_days = settings[env].db.backup_days if pg in db.postgres
```

Why it scales:
- you change env behavior by editing facts, not duplicating rules
- rules stay reusable across stacks/components
- you can move the settings into policy packs later

## Keep Facts Ground (No Variables in Facts)

In dform, a fact must be fully ground. If you want a statement to apply to
many bindings, it must be a rule, and a name no literal binds is not a
variable at all:

Bad (`sn` is an unknown name):

```dform
set net.subnet[sn].visibility = "private"
```

Good:

```dform
set sn.visibility = "private" if sn in net.subnet
```

## Prefer Sugar That Lowers to Core IR

For authoring, prefer:

- `resource` blocks over repeated `want/arg`
- reads where the value is used (`cidr = vpc.cidr`, `cfg.region`) over a
  variable bound in one line and used in another
- named arguments (`peering(env: env, name: n)`) over positional arguments
- object/list literals (`{k: v}`, `[a, b]`) over lots of `tags.foo` entries
- list comprehensions (`[x | ...]`) over hand-written `collect(...)` rules
- a module `instance` with an `if` clause to gate a group of resources on
  one guard
- declare a set lattice for list attributes several sources contribute to (`type_lattice(iam.policy, "statements", "set")`)

## Explode Lists Into Rows With `in`

If humans want to write lists, but your infra wants one resource per item, convert the list into a relation:

```dform
settings prod {
  vm.ips = ["10.0.0.10", "10.0.0.11"]
}

vm_ip(e, ip) if ip in settings[e].vm.ips

# If you need stable indices (order-sensitive), bind the index too:
vm_ip_indexed(e, i, ip) if ip = settings[e].vm.ips[i]

resource compute.vm "vm-${ip}" {
  if vm_ip(env, ip)
  private_ip = ip
}
```

This is the Pattern A win: derive one resource per row, not index-based `count`.

Note: Datalog has no intrinsic ordering, so dform's aggregates are deterministic:

- `collect_set(x)` returns a sorted list of unique values
- `collect_list(x)` returns a sorted list that may include duplicates

## One Merge Law for Shared Attributes

Every contribution to one attribute, whichever resource block, module or
policy pack it comes from, meets in one cell: the attribute aggregate
`attr(Type, Name, Path, Value)`. A read of an attribute (`vpc.cidr` in a
body, `settings["prod"].x`, `m.i.k`) reads that collapsed value, never a single
contribution, and the stratifier runs them after every contribution is in.
Statement order never matters.

How a path merges is its lattice:

- a scalar or a list is one value (Flat): two sources with different values
  are a conflict;
- an object is a map, merged per key (nested objects too, so the dotted
  paths `spec.replicas` and `spec.template.spec.containers` both land in `spec`): `tags = { env: dev }` in a resource and
  `set r.tags = { team: "platform" } if r in resource` in a policy pack give both tags;
  two sources disagreeing on one key are a conflict;
- a list declared a set is the union of every source at the highest rank
  present; a `@default` set is replaced wholesale by a normal one:

```dform
type_lattice(iam.policy, "statements", "set")
type_lattice(settings, "audit.sinks", "set")
set settings["prod"].audit.sinks += ["s3"]

settings prod {
  audit.sinks += ["cloudwatch"]
}
```

`+=` is a plain contribution, the same as `=`;
the lattice, not the operator, decides how they merge. A dotted path
`tags.team` contributes `{ team: V }` to `tags`.

A conflict is a `deny("conflicting attribute contributions", ...)` naming the
resource, the path, and every contributing rule with its value. It is resolved
by rank, not by order. `@default` after a value loses to a plain contribution,
`@override` beats it, and a disagreement at a losing rank is only a warning.
After a `resource` or `settings` header the rank applies to every field
without its own. The core form is `arg(T, N, P, V, default|normal|override)`.

## Layer Environments With `@default`

Write the common settings once at `@default` and let each environment
override only what differs; an override wins per leaf:

```dform
env_name("staging")
env_name("prod")

settings e @default {
  if env_name(e)
  db = { backup_days: 3, multi_az: false }
}

settings prod {
  db = { backup_days: 14, multi_az: true }
}
```

The same shape gives org-wide defaults from a policy pack without reading the
attribute it defaults: `set r.tags = { team: "platform" } @default if r in resource`.

Because the engine lowers these to the same small core, you keep composition and
predictability without paying the verbosity tax.

## Prefer Small, Composable Predicates

- Keep predicates narrow and reusable (`zone_index/2`, `peer/2`).
- Avoid embedding meaning into long strings; derive them from facts.
- Use helper predicates for readability rather than repeating long bodies.

## Every Predicate Read Must Be Defined

A rule body may only read a predicate that some fact or rule defines, a
builtin, or one the provider feeds (`input`, `data`, `cloud_exists`, ...). A
misspelled predicate is a compile error naming it and the rule, not an empty
relation. A table that may legitimately have no rows is declared:

```dform
decl mesh_allow_direct_route(from, to)
```

## Convert Explicitly

Arithmetic takes integers; `"10" + 1` is an error, not `11`. Convert with
`to_int`, `to_string`, and shape strings and lists with `len`, `lower`,
`upper`, `split(S, Sep)` and `join(List, Sep)`.

## Use Stratified Negation for Defaults

Negation is most useful for defaults and "absence" checks.

Pattern:

```dform
input region: string = "us-east1"      # a default, and --set region=... wins

# or, over a relation that may have no row:
has_region(true) if region_of(_)
let region = r if region_of(r)
let region = "us-east1" if not has_region(true)
```

Rules that rely on `not ...` should be:
- non-recursive through negation
- safe: the negated atom must be ground at evaluation time

## Treat Components as the Unit of Reuse

Components give you the Terraform module benefits:
- stable names/addresses via scoping
- local naming (`"vpc"`, `"db"`, `"cluster"`)
- explicit outputs for wiring

Inside a component:
- keep resources local (`resource` blocks)
- publish only what consumers need via `output k: T = t`
- bring in policy with a pack: `use baseline`

## Keep Resource Identity Stable

Prefer stable resource names and express change via attributes. If you bake lots
of configuration into the resource name, you create needless replacements.

Good:
- `net.vpc["network.main::vpc"]` (stable)
- `cidr` changes across envs

Risky:
- names that include computed values, timestamps, or random suffixes

## Make Denies Read Like Policies

Denies are strongest when written as invariants:

- "db must not be public"
- "prod db must be multi_az"

Avoid denies that replicate half of your config; prefer checking the final
desired shape.
