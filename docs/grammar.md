# The dform grammar, edition 2026

This is the reference for `crates/dform-core/src/lexer.rs`,
`crates/dform-core/src/syntax/parser.rs` (the lossless tree) and
`crates/dform-core/src/syntax/resolve.rs` (names, and the lowering to the
AST); keep them in step. It is the surface of proposal H
(`proposals/H-small-surface.org`): one spelling per construct, over the
core of E as revised by F. The places this implementation settles what H
left open are under "Decisions the proposal left open" and in the
proposal's addendum. The test suite is the example corpus: every `.df`
file in the repository, `tests/syntax/ok/*.df` (must parse and lower) and
`tests/syntax/err/*.df` (must fail with the diagnostics in the matching
`.txt`).

Parsing never stops at the first error. The parser builds a lossless tree
(rowan): every byte of the file, comments and whitespace included, is in it,
so `dform fmt` can print the file back. An error abandons the statement it
is in: the rest of it, up to the next line outside the brackets the
statement opened, up to the `}` that closes the enclosing block, or up to a
line that starts a statement in column 0, becomes one error node, and
parsing resumes after it. One bad statement is one diagnostic.

## Files and lines

```
file   := "edition" INT NL (header NL)* (stmt NL)*
header := key | input                 ; in that order: key, input, input p(..) from
```

Every `.df` file starts with `edition 2026` (comments may come first). A
file without it is an error that names the pragma; any other year is an
error too. Text that is not a file (provider schemas, `dform query`
patterns, tests) may leave the pragma out.

### The header

Every file is a module (see "Modules"); a file under `stacks/` is a
stack, a module the tool uses, named after itself, and `key` makes one
deployment per value. What a file takes is its header, after `edition`
and before its body: `key` lines, then `input` lines (value inputs, then
relation inputs, `input p(..) from ..`). `use` and `instance` are body
statements. Everything
else is the body, `provider` included: a provider block is a rule that
may read values, in scope for the whole program wherever it is written.
A header statement after the body's first statement is an error that says
to move it ("`key env` is a header statement: move it above the body's
first statement, line 5"); `dform fmt` moves it, and puts the header's
kinds in order, keeping the author's order within a kind. The header
reads names the body declares: `input env: environment` above `type
environment = ..` resolves, as every name does, program-wide. A
component's statements are its own (R-11a orders them).

The first token decides what a statement is (H-2): a statement keyword
starts its own statement, and a name followed by `(` is a fact or a rule.
A newline outside every `( )`, `[ ]` and the braces of an object ends a
statement; inside those, newlines are whitespace. Nothing continues a line:
a body of several lines is `where { .. }`, one literal per line, and a long
term wraps inside its brackets. There is no statement terminator: `p(a).`
is an error that says so, and so is `:-`.

Two statements on one line are an error ("expected the end of the line").
A block (`{ }` of a resource, settings, instance or provider) and a
body block (`where { }`) separate their entries by a newline or a comma.

## Tokens

```
IDENT    := [A-Za-z_][A-Za-z0-9_]*       ; case decides nothing; "_" alone is the placeholder
STRING   := "\"" ... "\""                ; escapes \" \\ \n \t \u{hex}; ${e} interpolates
INT      := [0-9]+                       ; -1 is unary minus applied to 1
RANK     := "@default" | "@override"
COMMENT  := "#" to end of line
```

Punctuation: `( ) { } [ ] , . : = == += != < <= > >= + - * / % |`.

`.` is always member access, and `/` always division. `-` is always an
operator: a hyphenated name is a string, and the parser says so.

Statement keywords, recognised only as the first token of a statement (18):

```
edition  provider  key  type  decl  extern
input  output  let  set
component  instance  use
resource  settings  deny  warn
```

Body words: `not in has`. The clause word: `where` (R-1). Literals: `true
false`. These six, and the reserved `if`, are never a name in a term; `if`
is an error wherever it stands, which prints the statement with its clause
spelled `where`. Anywhere a plain name is expected (a
key, a path segment, a declared name) any keyword is a name, and a keyword
followed by `(` is an atom or a call (`input("env", v)`). A statement
keyword may start a chain in a term (`settings[env]`). Contextual words in
declarations, where the position is fixed: `from`, `check`, `persist`,
`mixed`, `as` (in `use`), the attribute flags (`required computed id
sensitive nullable`). `module`, `policy`, `import` and `export` are
words of an earlier surface: each is an error that names what to write
(R-65).
Roots: `settings`, `world`.

### Strings and interpolation

A string is a string constant: every constant is quoted (`"prod"`,
`"set"`, `"audit.write"`). A path that is data is a string too (H-12):
`type_lattice(iam.policy, "statements", "set")`.

`"a${e}b"` interpolates the term `e` (H-13): it lowers to
`format("a%sb", e)`. `$${` is a literal `${`; a lone `{`, `}` or `$` is
itself. A hole may not hold a string (bind it first). A hole is a content
position: a dot in it reads now (see "Reference or read"). A literal part
may not contain `%s`.

A quoted path segment (`x."a-b"`) is one key: it may not hold `.`, `[` or
`]`.

### Doc comments

```
DOCLINE  := "#|" to end of line          ; a COMMENT to the lexer and to tree-sitter
```

`#|` lines directly above a statement, with no blank line between them
and it, document it; a `#|` after code on its line is that line's
comment. Each line is `key: value` when it starts with a lowercase name
(letters, digits, `_`, `-`) and a colon; any other line (`#|` alone is an
empty one) is a line of the item's description, and `description:` adds
to it. Keys are free-form; these are documented:

| key           | what                                          |
|---------------|-----------------------------------------------|
| `description` | what the item is (the bare lines)             |
| `owner`       | who answers for it                            |
| `since`       | the version or date it appeared               |
| `deprecated`  | why not to use it, and what to use instead    |

```dform
#| The deployment's environment, the stack's key.
#| owner: platform
key env: environment = "staging"
```

What a doc comment may document, and the `Kind` and `Name` of its facts:

| statement                                   | Kind        | Name                  |
|---------------------------------------------|-------------|-----------------------|
| `component c`                               | `component` | `c`                   |
| `input k`, `key k`                          | `input`     | `k`                   |
| `output k`                                  | `output`    | `k`                   |
| `decl p(..)`, `extern p(..)`, `input p(..) from ..` | `predicate` | `p` |
| `p(..) where ..`, a fact, `let k = t`, `deny "m"`, `warn "m"` | `rule` | `p`, `k`, `m` (the message) |
| `type a = T`                                | `alias`     | `a`                   |
| `resource T n`, `resource T "n-${e}"`       | `resource`  | `T["n"]`, `T["n-${e}"]` (as written) |

Inside a component the name is `COMPONENT.NAME`
(`network.vpc_net`). Each pair lowers to a fact of the compiler's own
relation `doc/4`, spanned at the comment: `doc(Kind, Name, Key, Value)`,
so a policy can read and require them (`deny "a component has no owner" {
component: c } where doc("component", c, "description", _), not
doc("component", c, "owner", _)`). The language server shows them on hover, and `dform doc`
renders a project's as Markdown. A doc comment above anything else
documents nothing.

## Names

Case decides nothing; a resolver does (`syntax/resolve.rs`). Resolution is
program-wide: `loader::load_program` parses the entry file and every module
its paths name (see "Modules"), collects the declarations of all of them,
then lowers each: the entry as the program's top level, each module as
itself. `parser::parse_file` and `parse_program` lower one text on its
own.

What a name can denote:

| Denotation        | Declared by                                       | Written                          |
|-------------------|---------------------------------------------------|----------------------------------|
| variable          | its occurrences in a rule                         | `x`, `_x`, `_`                   |
| value name        | `input k: T`, a component's input, `let k = t`    | `k`                              |
| resource          | a `resource T n {..}` header with a static name   | `n.p` in scope, `T[e].p`         |
| module            | `use m`, `use a.b as m`                           | `m.x` (a value, an output, a resource), `m.p(..)` |
| copy              | `instance c n`                                    | `n.k` (an output), `c[e].k`      |
| stack deployment  | `use stacks.s`                                    | `s[k=v].out`, `s.out` unkeyed    |
| settings row      | `settings n {..}`                                 | `settings[e].p`                  |
| live object       | the provider's inventory                          | `world.T[e].p`                   |
| type              | see "Types"                                       | `T` (dotted or not)              |
| relation, builtin | its rules or facts, `decl`, `extern`; a builtin   | `p(..)`, `p[..]`                 |

`.` is static and `[ ]` is a key (H section 5.1): a postfix `.name` is a
member the program text names, which the resolver finds at compile time (a
resource in scope, an instance's output, an attribute path, a record
field), and a name it cannot find is an error. A postfix `[term]` is a
lookup by a key computed at run time: a join, which may give no row, one or
several, and never an error for a key that is not there.

| Written         | Collection              | Keyed by                          | Lowers to                            |
|-----------------|-------------------------|-----------------------------------|--------------------------------------|
| `T[e]`          | resources of type `T`   | address, relative to the scope    | `want(T, A)`, a dot reads `attr`     |
| `c[e]`          | copies of component `c` | the copy's name                   | `instance_of("c", User, e), output(e, ..)` |
| `s[k=v]`        | deployments of stack `s` | each key, by name                | `stack_output("s[k=v]", ..)`         |
| `settings[e]`   | settings rows           | row                               | `setting(e, path, V)`                |
| `world.T[e]`    | live objects of `T`     | the provider's name               | `cloud_attr(T, e, path, V)`          |
| `p[a, b]`       | relation or extern `p`  | every column but the last         | `p(a, b, V)`                         |
| `e[i]`          | a list value            | index (a fresh `i` enumerates)    | `member(e, i, V)`                    |

A name followed by `(` is a relation, a builtin or an extern; `m.p(..)`
is the relation `p` of the module `m` a `use` brings. Any other chain
`name (.seg | [terms])*` is resolved from its first name, innermost scope
first (rule, then component, then file, then program):

1. a typed variable (`x` after `x in T`): a reference;
2. a value name: a read of `k(V)`; a `let` whose value is a reference (a
   settings row, a resource, a live object) reads through it (H-6);
3. `settings[e]`, `world.T[e]`;
4. a resource of that name in scope (a component's own resources, then
   the program's);
5. a copy (`n.k`, its output), a stack a `use` binds (`s[k=v].out`), a
   component's copies (`c[e].k`, `m.c[e].k`, or by its path from the
   root), a used module's item (`m.x`);
6. a type `T` followed by `[e]`, an `extern` or a relation followed by
   `[..]`;
7. a variable of the rule;
8. a dotted name in a type namespace: that type's name, a string.

Anything else is `unknown name`, with a hint to quote it.

A bare name (no `.` or `[`) is a variable unless it is a value name. A
variable may not take the name of a resource, a module, a copy, a
component or a type namespace in scope ("variable `net` shadows the type
namespace `net`"). A bare
resource name is its address in two places: the value of an `output`
typed by a resource type, and the left side of `in`, where the type on the
right picks among resources of one name. It is a reference value (R-42),
which prints as its address `T["A"]`, everywhere else it stands for the
resource: a whole value given to something (an entry, `vpc = main` or the
pun `vpc`, also inside the component that declares it; a `set`, a `let`,
an input of a copy, any other output, an element of a list or object there,
a comprehension's item), a column that takes a resource (the plan's
`deformation(kind, r, before)` and `world_digest(r, now)`,
`requires_approval(r, reason)`, `lifecycle(r, what)`, `adopt(r, remote)`,
`ignore_changes(r, path)`, the last of `moved(T, "old-address", r)`), and
either side of `==` or `!=` with a resource on the other (R-43). `T[e]`
and a typed variable are references in the same places.

A variable must have a binding occurrence somewhere in its rule: an
argument of a relation (a pattern in it included), either side of `=`, the
left of `in`, a named argument, an index of a read, or the block's clause.
A name with none was meant as a string: `env == prod` is `unknown name
prod`. `==`, `!=` and the orders test; they do not bind.

### Types

Types are names of the core (`net.vpc`, `gke_cluster`, `k8s.deployment`).
The known types are every resource header's type, every `type` block's, the
first argument of a `type_*` fact (a schema's `type_provider`, `type_attr`
rows), and the types the built-in provider schemas declare. A dotted name
used as a type must be a known type, else `unknown type` (H-10): a typo is
an error, never a string. The built-in schemas close their namespaces but
`k8s` (a cluster's types are its own); a type in a namespace no built-in
schema closes may be a provider schema's the compiler does not read, so
`T[e]` and `x in T` there take `T` as written.

`query` and `why` patterns are read without the program's declarations:
there, any dotted name that names nothing else is a type.

### References and their type

A reference is a pair (type, address). A dot on a reference needs its type
statically:

| Written              | Type                                   | Address                        |
|----------------------|----------------------------------------|--------------------------------|
| `n` (unique in scope) | the declaration of `n`                | `"n"` (scoped by the copy)     |
| `T[e]`               | `T` (no declaration needed)            | `e`, relative to the scope     |
| `x` after `x in T`   | `T`                                    | `x`                            |
| `x` after `x in resource` | a fresh type variable             | `x`                            |
| `n.k` (`output k: T`, `T` a resource type) | `T`              | the output's value             |
| `m.x` (`m` used, `x` its resource) | `x`'s                    | `"m::x"`                       |
| `k` (`let k = R`, `R` a reference) | `R`'s                    | `R`'s                          |

`T[e]` is relative to the scope (H-10): inside a component it is
`scoped("n", e)`, `n` the copy; in a module, a constant is the module's
own (`scoped("m", "x")`) and a variable any resource its user sees; at the
top level and in CLI arguments it is the full address, which pastes
unchanged from `plan` (H-16): `net.vpc["main::vpc"]`. A resource in scope is written by its
name: `T["n"]` for a resource `n` in scope, and `T.n`, are errors naming
`n`. A name declared twice in scope (three resources named `web`) is an
error listing the candidates by address. A dot on a variable with no
static type is field access on a value: `__path(X, "f")`. A variable a
reference column binds with no `in` (`deformation(k, r, _)`) is a
reference of no known type, and `r.p` on it is an error that says to bind
it with `r in T` (R-43).

A program never reads an id (R-43). A reference is the resource, and
where an attribute identifies another resource the provider's schema
types it `ref(T)` (`list(ref(T))`, `set(ref(T))`), so the attribute takes
the resource itself: `vpc = main`, `subnets = [ s | s in net.subnet ]`.
The provider gives its API whatever identifies the object (the schema's
`id`, `schema::IDENTITY`) once it exists; until then the plan prints the
resource unknown, `vpc = ?net.vpc["main"]`, and after it the resource,
`vpc = net.vpc["main"]` (the id under `--json`). `x.id` is an error naming
the reference. `ref(r)` writes the reference out where an attribute that
is no `ref(T)` needs the id as text (a bridged provider's `string`). An
input or output of a module or component typed `ref(T)`, `list(ref(T))`
or by a resource type holds references.

A literal in a position whose type is known is checked as that type at
compile time (R-31, Postgres's unknown-literal rule): a schema attribute's
type (`inet`, `int`, `bool`, `enum(..)`, `ref(T)`), an input's declared
type for its default and a copy's value, a function's parameter.
`cidr_block = "10.0.0/16"` in an `inet` attribute, `vpc = "main"` in a
`ref(net.vpc)` one and `subnets = [main]` (a `ref(net.vpc)` where
`ref(net.subnet)` is wanted) are errors at the entry, naming both types; a
string literal where an `inet` is declared is read as one. Without a type
a literal is a string. A reference and a string never compare: `r ==
"main"` is an error that names `r == main` or `r == T["main"]`.

## Statements

```
stmt       := KEYWORD ...                      ; one production per keyword, below
            | NAME ("." NAME)* "(" args ")" RANK? ("where" body)?   ; a fact or a rule

provider   := "provider" NAME block?                ; no block when it has no entries
type       := "type" NAME "=" type | "type" DOTTED attrs
decl       := "decl" DOTTED columns "mixed"?
extern     := "extern" DOTTED "(" bindarg ("," bindarg)* ")" "persist"?
bindarg    := ("+" | "-") NAME (":" type)?
input      := ("input" | "key") NAME ":" type ("=" term)? ("check" body1)?
            | "input" NAME columns "from" term     ; facts(..) | FORMAT(..)
output     := "output" NAME (":" type)? ("=" term)? ("where" body)?
let        := "let" NAME "=" term RANK? ("where" body)?
set        := "set" chain ("=" | "+=") term RANK? ("where" body)?
use        := "use" path ("as" NAME)? block? ("where" body)?
instance   := "instance" path NAME block? ("where" body)?
component  := "component" NAME stmts             ; an item of a module
path       := NAME ("." NAME)*                    ; a/b.df from the root; std.x; a package mount
resource   := "resource" DOTTED hname RANK? block ("where" body)?
settings   := "settings" hname RANK? block ("where" body)?
deny, warn := ("deny" | "warn") STRING object? ("where" body)?
stmts      := "{" (stmt NL)* "}"

attrs      := "{" (attrdecl SEP)* "}"
attrdecl   := blockpath ":" (attrs | type flag* ("check" body1)?)
flag       := "required" | "computed" | "id" | "sensitive" | "nullable"
block      := "{" (entry SEP)* "}"
entry      := blockpath (("=" | "+=") term)? RANK?   ; `zone` alone is `zone = zone`
blockpath  := SEG ("." SEG | "[" INT "]")*
SEG        := NAME | STRING
hname      := NAME | STRING                        ; see "Block names"
columns    := "(" NAME (":" type)? ("," NAME (":" type)?)* ","? ")"
body       := body1 | "{" lit (SEP lit)* "}"
body1      := lit ("," lit)*
SEP        := "," | NL
DOTTED     := NAME ("." NAME)*                     ; no spaces
```

Every statement is `head where body` (R-1): the clause follows its head,
on the head's line, and a block is a head. A resource, settings row or
instance or `use` takes at most one clause, after its block's `}`: `resource T n {
.. } where B`, and a body of several lines is `} where {`, one literal per
line, closed by its own `}`. The clause is a query, and the block is one
resource (or row, or copy) per match. A `provider` block takes no
clause. `if`, the clause word of an earlier surface (H-3), is an
error wherever it stands, and the error prints the statement with its
clause spelled `where`.

An entry that is only a path is the pun of its last segment (R-33), as
`{ a }` is `{ a: a }` in an object: `availability_zone` alone is
`availability_zone = availability_zone`, `spec.selector.color` is
`spec.selector.color = color`, and a rank may follow (`tags @default`).
The segment is resolved where the value would be, as a clause variable, a
value name or anything else a bare name can be; a path whose last segment
is not a name (`a[0]`, `"a-b"`) is an error. A provider's `source` is a
constant, never a pun.

`set` is the contribution statement (H-5): the chain is a resource's
attribute, a settings row's leaf, or an input (a stack input, or a copy's,
`set blue.cidr = ..`). A `set` with no `where` on a resource, settings row
or copy declared in the same scope is an error that names the block to write the
entry in; a top-level `set` of the program's own input is an error too
(give it a default, or pass `--set`). `scenario` is gone (R-32): the
program's denies are its tests, and `dform test` runs them over the
inputs' values; a what-if plan is `plan --set k=v`.

`let k = t [@rank] [where B]` is a value (H-6), a cell of the attribute
aggregate like an input (R-3): each row contributes to the cell `(let,
SCOPE, k)` (scope `""` for the program's, `n` in the copy or import
`n`), and a read
of `k` reads the collapsed cell. Rows that agree are one value; two that
disagree at the winning rank are a conflict naming both; a `@default` row
gives way to any other. When `t` is a reference (a settings row, a
resource, a live object), `k`'s value is that reference and its static
type is the reference's, so a dot on `k` reads through it: `let cfg =
settings[env]`, then `cfg.db.size`.

`output k: T = t [where B]` is one statement (H-7): the type is optional (an
untyped output is `any`), the value is not.

`deny "m" {ctx}? where B` and `warn` are the checks (H-8). The message is a
string like any other: `${e}` reads the body's variables. A deny is checked
after evaluation; no rule may read `deny` or `warn`.

A relation is declared by its columns (H-11): `decl p(a, b)`, a type on a
column optional; `mixed` lets it have both facts and rules. A copy's
relations are its own (`n::p`, which no source spells); a value leaves it
through an `output` (DESIGN.org R-5). A module's are its import's,
`m::p`, read as `m.p(..)`.

### The core is written only where the surface cannot reach

The core relations (`want`, `arg`, `arg_add`, `attr`, `setting`, `output`,
`input`, `cloud_attr`, `cloud_exists`, `member`, `deny`, `warn`) are the
lowering's (H-15). In a program file, writing one where a surface form says
the same is an error naming the form: a body `want(T, x)` with a static
type is `x in T`; `attr(T, "n", "p", v)` is `v = T["n"].p`; a head
`arg(T, "n", "p", t)` is `set T["n"].p = t`; `output("k", t)` is
`output k = t`; `deny("m")` is `deny "m" where ..`; `member` is `in` or an
index. The core stays writable where no surface form reaches: a variable
type or path (`arg(t, a, "tags", {..}) where want(t, a)`), a raw `arg` read in
a body, and text that is not a program file (schemas, the compiler's own
tests).

### Modules

Every `.df` file is a module, named by its path from the project root
with dots (R-65): `config.df` is `config`, `modules/net.df` is
`modules.net`, `stacks/platform.df` is `stacks.platform`. A path is
looked up, never searched: `a.b` is `a/b.df` under the root (outside every
project, beside the entry file), `std.x` is the standard library, and a
first segment `dform.toml`'s `[packages.NAME] path = "../infra"` names is
that project's root (`use infra.stacks.platform`). A module named like a
standard library one (`modules/str.df`) is an error: `std` is in every
scope, so `str.split` needs no `use`. A `use` cycle is an error at the
statement that closes it.

`use m [as n] [{ k = v }] [where B]` imports the module once under `n`
(the path's last segment unless `as` names it):

- its rules and denies run over what the importing scope sees; a name
  the module does not define reads outward, its user's (`env` in a policy
  pack is the stack's);
- its items read as `n.x`: a `let` or an input (`config.region`), a
  relation (`n.p(..)`), an output (`n.k`), a resource (`n.x`, the address
  `T["n::x"]`), a type alias (`n.T`), a component (`n.c`, to `instance`);
- its inputs are bound by the block, as a copy's are, else by their
  defaults; an input with neither is the error a stack input's is (`input
  traefik.acme_email is required and has no value`);
- its resources, if it has any, are stamped once under `n` (`T["n::x"]`);
  a module used from two stacks runs in both, each in its own state;
- with a clause, all of it exists only while `B` holds.

`use` twice of one name in a scope is an error, and so is `use` of a
component; from two scopes (a stack, and a component it instances) it is
two imports, each reading its own user's names.

`component NAME { .. }` is an item of a module, the only thing stamped
many times: `instance PATH NAME { k = v } [where B]` makes one copy, by
the component's path (`instance modules.net.vpc main`, `instance net.vpc
main` after `use modules.net`, `instance network blue` for one the file
declares). A copy is named; its resources are `NAME::x`, its relations its
own, its outputs `NAME.k`, and `c[t].k` ranges over the copies of `c` the
scope makes, `instance_of(c, user, name)` joined to their outputs. A copy
inside a copy is scoped under it (`edge.left::vpc`). The names a scope's
`use`s and `instance`s bind are one namespace. `instance` of a module is
an error naming `use`, and so is one with no name.

### Deployed modules

A stack is a module the tool uses: a file under `stacks/` (or one
dform.toml's `[stacks.NAME]` names), named after itself: `stacks/shop.df`
is the stack `shop` (docs/layout.md), and a program says nothing about
which stack it is. `key env: T` declares an input that selects the
deployment: it is an input in every respect (typed, a cell, read as `env`,
documented and hovered as an input) but that the target gives it (`dform
plan shop env=prod`), never `--set` (an error naming the target form), it
may not be `secret`, and its value names the deployment, with its own
state. Several `key` lines make a composite key in source order
(`shop[env=prod,region=eu]`). A key is declared at the top of the stack's
file; one in any other module is an error. `stack`, the statement of an
earlier surface, is an error that says so.

`use stacks.platform` binds the stack's deployments, which the tool made:
`platform[env=e].out` reads one deployment's output from what it
published (`stack_output("platform[env=e]", "out", V)`), each key given
once, and `platform.out` reads an unkeyed stack's. Such a read is what
`apply X` applies first (R-30). A program never instances a stack
("stacks.platform is deployed by the tool; `use` it"), and its `use`
takes no block and no clause.

### Stack settings

A stack's operational settings are not in the program: they are
dform.toml's `[stacks.NAME]` for `stacks/NAME.df`, over `[defaults]`. A
term is written as a string, `{stack}` in it the stack's name and `{k}`
the value of its key `k` (in `backend` and `config`). The list is closed;
any other key is an error naming it:

| setting      | value                                                                 |
|--------------|-----------------------------------------------------------------------|
| `backend`    | where the state lives: `'local("DIR")'` or `'s3("BUCKET", "PREFIX", {endpoint, region})'` |
| `role`       | `"bootstrap"`: it creates what a controller runs in, and stays batch  |
| `approvals`  | who approves a plan: `'jwks("URL")'`, `'jwks_file("PATH")'`, or a list |
| `audit_sink` | a command each audit log entry is piped to                            |
| `isolated`   | `true`: each key value deploys into its own account (needs a key)     |
| `config`     | `'FORMAT("config/NAME/{env}.yaml")'`: a table of the deployment's settings (needs a key) |

The loader reads them into one `Stmt::Stack` of the program, at their
place in dform.toml, so an error in one is reported there; `config` is
lowered as a table (see "Relation inputs and tables").

### Provider blocks

`provider NAME { .. }`, or `provider NAME` with no settings, names a
provider the program uses; a program with no `provider` statement starts
none, and what evaluates it against providers (`plan`, `apply`, `query`,
`why`, `test`) refuses it, naming the fix (`dev --provider` runs it
anyway). Its `source` is a constant (the stack reads it to start the
provider; without one, dform.toml's `[providers]` entry of the name).
Every other setting is the provider's own, which its schema may declare;
one it does not is passed to Configure as written. Each is a term, read like a rule's
(inputs, settings rows, value names, tables, `env.var`), and the block
lowers to one rule for them all, plus one for `expect_account`:

```
provider p { k1 = t1, k2 = t2 }    provider_config("p", { k1: t1', k2: t2' }) :- reads
expect_account = t                 provider_expect_account("p", t') :- reads
```

A setting is a content position: a dot in it reads now. A block takes no
clause, no `+=` and no rank; a setting given twice is an error.

A `provider` statement also brings the provider's externs into scope, with
their binding modes (DESIGN.org R-8): a program does not write `extern`
for them. `file`, `env` and `random` are built-in fact providers, declared
like any provider and needing no `dform.toml` source (`externs::BUILTINS`):

```
provider file         file.json(+path, -value: any), file.text(+path, -value: string)
provider env          env.var(+name, -value: secret(string))
provider random       random.password(+key, -value: secret(string)) persist
```

`extern file.json(..)` in a program is an error naming the `provider`
statement to write instead. `env.var(t)` as a term is the lookup
`env.var[t]`; without `provider env` it is an error that says to declare
it. `extern` stays the schema's word: provider schemas and the compiler's
tests declare externs with it, and so, until the compiler reads a
provider's schema (DESIGN.org R-24), does a program for a provider that is
not built in.

### Type aliases

`type NAME = TYPE` names a type: `type environment = enum("dev", "stg",
"prod")`, then `input env: environment` and `input peering(env:
environment, ...) from csv(..)`. An alias is usable anywhere a type is (an
input, a component's input, a table's column, an output, an extern's column, a
`decl` column, a `type` block's attribute) and is transparent: the
resolver writes its type in its place, so nothing after it sees an alias.
An alias may name other aliases (`type envs = list(environment)`); one
that reaches itself is an error naming the cycle, each alias in it
labelled. A member of `enum(..)` is a value, never an alias. An alias may
not take a built-in type's name (`int`, `string`, `bool`, `inet`,
`symbol`, `addr`, `any`, `enum`, `list`, `set`, `secret`, `ref`).

Where an alias is in scope: in its file, an alias in a component in the
component. Another module's aliases are public, read through the name
its `use` binds or its path (`config.environment`, `network.subnets`).
Two aliases of one name in one scope are an error listing both.

### Relation inputs and tables

`input p(a, b) from facts(SOURCE)` is a relation the world gives: its facts
are a `.df` fact file's (`facts("data/release.facts")`, or
`facts(git(REPO, REF, PATH))`), read from outside and re-read when they
change. `input p(col: type, ...) from FORMAT(SOURCE)` is a table: its rows
are a data file's. FORMAT is `csv`, `json`, `yaml` or `toml`; SOURCE is a
term for the path (a string, holes allowed: a hole is a content position,
so it reads now) or `git(REPO, REF, PATH)`, each a term. A table's columns
are typed, with an input's types (`inputs::check_type`), never `secret`.
Relation inputs are the last of the file's header (see "The header").

A table lowers to externs (`src/tables.rs`), the source its bound inputs:

```
decl p(col, ...)
extern table.FORMAT.p(+path, -at, -col: type, ...)
p(Col, ...) :- reads, Path = PATH', table.FORMAT.p(Path, At, Col, ...)
```

and from git, the ref resolved to a commit first:

```
extern table.git.p(+repo, +ref, -commit)
p(Col, ...) :- reads, Repo = .., Ref = .., Path = .., table.git.p(Repo, Ref, Commit),
               table.FORMAT.p(Repo, Commit, Path, At, Col, ...)
```

A stack's `config = 'FORMAT(SOURCE)'` (dform.toml) is the table
`stack.config(path: string, value: any)`, read into `arg("settings", Row,
Path, Value, normal)`, `Row` the key's value (several keys':
`format("%s/%s", ..)`), and `{k}` in SOURCE the hole `${k}`. `transform` expands
that rule into one per settings path the program writes or reads, and a
deny for a leaf at any other path.

### Block names

A resource's or settings row's header name is a string or a name. A string
with holes (`"private-${z}"`) is the variable `Addr`, bound last in the
body by `format`. A name the block's clause binds is that variable
(`resource net.vpc t { .. } where tenant(t, i)`); any other name is the
static name (`resource net.vpc shared`), and it may not be a value in
scope (`settings env` with `input env` is an error: write `settings _`
for every row, or `settings "env"` for the literal one). A block (header,
entries, clause, interpolated names) is one rule; a header name's scope
is its block and its clause. The clause follows the block, so a header
name it binds is read forward: the header names a variable the reader
meets in the clause below, as a rule's head names variables its body
binds (R-1 keeps the rule and moves only the clause).

`settings _ @r { .. }` contributes to every settings row that exists: a
row the program reads (`settings[e]`) or anything writes (a named block,
`set settings[e]`, the stack's `config`). It lowers to
`arg("settings", Row, P, V, r) :- settings_row(Row)`, and `transform`
derives `settings_row` from the program's reads and writes (the literals
before a read, less those that read the settings).

### The placeholder `_`

`_` alone stands where a variable could and is never accessed (H-17): an
argument of a relation in a body, the left of `in`, an index (`xs[_]`), a
part of a pattern. `_.p`, `_[k]`, and `_` as a field's value, a
function's argument, an interpolation or a comparison's side are errors
that say to name it. `p(_)` in a head is an error naming the column (it
has no finite set of values); `resource T _` and `instance c _` name
nothing; `set T[_].p = t` is `set r.p = t where r in T`, and the error prints
it. A name that starts with `_` (`_x`) is an ordinary name, but for one
thing: any other variable written once in its rule (header, clause,
entries and interpolated names together, a `not { }` body once) is an
error, a typo or a placeholder that should say so (R-2); `_x` opts out.

## Literals and terms

```
lit        := "not" lit1 | "not" "{" body "}" | lit1
lit1       := atom
            | "has" chain                        ; the attribute has a value
            | chain                              ; a truth test: == true
            | term cmpop term (cmpop term)*      ; a <= b <= c is a <= b, b <= c
            | term "in" ("resource" | term)
            | term "not" "in" term
cmpop      := "=" | "==" | "!=" | "<" | "<=" | ">" | ">="
atom       := chain "(" args ")"
args       := (arg ("," arg)* ","?)?
arg        := term | NAME ":" term               ; a named argument: its column's name

term       := add
add        := mul (("+" | "-") mul)*
mul        := unary (("*" | "/" | "%") unary)*
unary      := "-" unary | primary
primary    := INT | STRING | "true" | "false"
            | chain | call | list | object | comprehension | "(" term ")"
chain      := NAME ("." SEG | "[" term ("," term)* "]")*
call       := chain "(" args ")"
list       := "[" (term ("," term)* ","?)? "]"
object     := "{" (key (":" term)? ("," key (":" term)?)* ","?)? "}"  ; `{ a }` is `{ a: a }`
key        := NAME | STRING
comprehension := "[" term "|" body1 "]"
type       := DOTTED ("(" type ("," type)* ")")? | "{" NAME ":" type ("," NAME ":" type)* "}" | STRING
```

In a literal position a chain applied to arguments is an atom, unless an
operator follows it (`f(x) == 3` compares a call). Precedence, loosest
first: `+ -` (left), `* / %` (left), unary `-`. Aggregates (`count(x)`,
`sum(x)`, `collect_set(x)`, ...) are calls in a head. Named arguments
(`project(id: i)`) are for a relation declared with named columns; they
lower to a record pattern.

Membership (H-9): `x in e` for a list, `x in T` for a type, `x in
resource` for any, `x in world.T` for a live object; `x = e[i]` gives the
index and the value.

### Reference or read

A dot on a reference means one of two things, decided by position (G-6):

- **a reference** where it is a whole value: a field's value, a head or
  output argument, an element of a list or object there, a comprehension's
  item. `endpoint = db.endpoint` is `ref(db.postgres, "db", "endpoint")`:
  an apply-order edge, and a null until a computed path resolves. The
  resource alone, `vpc = vpc`, is `ref(net.vpc, "vpc", "")`.
- **a read** anywhere its content is needed: a body literal, a clause, an
  argument of a builtin or operator, an interpolation hole, an index.
  `inet.subnet(vpc.cidr, 4, i)` reads `attr(net.vpc, "vpc", "cidr", V)`
  now. To read into a field, bind in the clause: `namespace = ns` in the
  block and `where ns = web.name` after it.

`settings[e].p`, a copy's output `n.k`, a value name, `p[..]` and
`world.T[e].p` are always reads.

### Where reads go

A read is hoisted: in a rule body just before the literal that holds it
(positive, even under `not`); from a head, a field, an instance input or a
comprehension item, appended to the body, after the clause, in source
order of first use. A block has one shared body: a read in any field gates
the whole block (the want and every field). Each distinct read is made
once per rule; a value name is read once per rule.

The variables a read binds are named after what they read (`vpc.cidr` is
`Cidr`, `env` is `Env`, `zone_index[z]` is `ZoneIndex`); a source variable
is its name capitalised (`vpc_net` is `VpcNet`, `_c` is `_C`), which is how
`strata`, `why` and diagnostics print it.

## Functions

A function is pure and deterministic: a call is a term, evaluated when its
arguments are ground, and a call with no value (an argument of the wrong
kind, a partial function off its domain) makes the literal that holds it
fail. Impurity enters only through externs. Every function is declared in
a signature file shipped with dform, `std/*.df`, which the compiler, the
language server (hover, completion, signature help)
and the secrets pass read; the engine's bodies are looked up by the
declared name, and a test keeps the two in step. A call of a name no
signature file declares is `unknown function`, with the name meant when
one is a qualification away (`split` is `str.split`).

```
sigfile    := "package" NAME NL (DOC* fnsig NL)*
fnsig      := "internal"? "fn" NAME "(" (param ("," param)* ("," "...")?)? ")" "->" type "?"? flags?
param      := NAME ":" type
flags      := flag ("," flag)*
flag       := "forwards" | "forwards" "nulls"
```

`?` marks a partial function. `forwards`: a secret argument flows through
to the result uninspected (otherwise a call over a secret is E0301).
`forwards nulls`: a null argument is not a content position (Rule 2).
`internal`: the lowering's own, not callable from a program. A `#|` doc
comment above a signature is its summary, and its `example:` key the
example hover shows.

A function is named by its package, the type it is about; the prelude's
are written bare.

| package   | functions                                                                 |
|-----------|---------------------------------------------------------------------------|
| prelude   | the constructors `int(s)`, `string(x)`, `inet(s)`, `ip(s)`, `iprange(a, b)`; `format(t, v, ...)`, `len(x)`, `ref(T, n, p)`, `scoped(s, n)`, `cloud_ref(T, n, p)`, `declassify(v, why)` |
| `inet`    | `inet.subnet(net, bits, n)`, `inet.host(net, n)`, `inet.addr(net, n)`, `inet.contains(net, a)`, `inet.overlaps(a, b)`, `inet.prefix_len(net)` |
| `ip`      | `ip.unspecified(a)`                                                       |
| `str`     | `str.split(s, sep)`, `str.lower(s)`, `str.upper(s)`                       |
| `list`    | `list.len(l)` (`len` in the prelude), `list.join(l, sep)`                 |

A function to `bool` is also a predicate: `inet.contains(n, a)` as a body
literal holds when the call is true. Conversions are constructors named
by their type; strings never coerce silently. Arithmetic (`a + b`) lowers
to the prelude's internal `add`, `sub`, `mul`, `div`, `mod`, and an
interpolation to `format`.

A dotted name's first segment names one thing: a type namespace (`net`), a
provider's externs (`file`), a function package (`inet`), a module, a
component or a copy (`config`, `network`, `blue`), or a root (`settings`,
`world`). Two declarations
that claim one head are an error naming both. A constructor is the one
function that may share a name with a type (`inet(s)`, the type `inet`,
the package `inet`).

## What lowers to what

Lowering produces the AST; `transform.rs` and everything after it take it
as it is.

| written                                   | lowers to                                              |
|-------------------------------------------|--------------------------------------------------------|
| `head where body`                         | `head :- body`                                         |
| `p(t)` with reads in `t`                  | `p(t') :- reads` (a rule)                              |
| `p(a: x)` (columns `a, b`)                | `p{a: x}`, a record pattern                            |
| `let k = t [@r] [where B]`                | `arg("let", S, "k", t', r) :- B, reads` (`r` normal by default, `S` the scope); `k(V) :- attr("let", S, "k", V)` once per `k` |
| `let k = R` (`R` a reference)             | the cell holds `R`'s key; `k.p` reads through it       |
| `type a = T`                              | nothing: each use of `a` is `T`                        |
| `#\| k: v` above an item (Doc comments)  | `doc(Kind, Name, "k", "v")`                            |
| `provider p { k = t, expect_account = a }` | `provider_config("p", {k: t'}) :- reads`, `provider_expect_account("p", a') :- reads` ("Provider blocks") |
| `env.var(t)`                              | `V`, reading `env.var(t', V)`                          |
| `resource T n { f = t } where B`          | `resource T n { f = t' } :- B, reads`                  |
| `resource T "a-${e}" { .. }`              | name `Addr`, `Addr = format("a-%s", e')` last          |
| `settings n @r { .. } where B`            | `settings n @r { .. } :- B, reads`                     |
| `instance c n { k = t } where B`          | `instance c n { k = t' } :- B, reads`; the copy exists while `B` holds |
| `use m { k = t } where B`                 | the same, of the module `m`, under `m`                 |
| `set k = v where B`                       | `input("k", v) :- B`                                   |
| `set n.k = v [where B]`                   | the copy `n`'s input `k`'s contribution                |
| `deny "m" {o} where B`                    | `deny("m", {o}) :- B` (`warn` the same)                |
| `deny "a ${x}" where B`                   | `deny(M, ..) :- B, M = format("a %s", X)`              |
| `set R.p = t @r where B` (`+=`: `arg_add`) | `arg(T, A, "p", t', r) :- B, reads`                   |
| `set settings[e].p = t`                   | `arg("settings", e', "p", t')`                         |
| `output k: T = t` (`T` a resource type)   | `output k: addr`, and its value                        |
| `output k = t` (no reads)                 | `output k = t'`                                        |
| `output k = t where B` (reads, or a body) | `output(k, t') :- B, reads`                            |
| `decl p(a: t, b_c: t)`                    | record fields `a`, `b_c`                               |
| `input p(a) from facts(S)`                | a relation read from `S`, re-read when it changes      |
| `input p(c: t) from F(S)`                 | `p(C) :- reads, Path = S', table.F.p(Path, At, C)` ("Relation inputs and tables") |
| `key k: T` (`[stacks.s] config = 'F(S)'`) | `arg("settings", K, P, V, normal) :- .., table.F.stack.config(.., P, V)` |
| `enum("a", "b")` in a type                | `enum(a, b)`                                           |
| `"a${e}b"`                                | `format("a%sb", e')`                                   |
| `k` (value name)                          | `V`, reading `k(V)`                                    |
| `x.f.g` (a value)                         | `__path(X, "f.g")`                                     |
| `e[i]` (a list value)                     | `V`, reading `member(e', i, V)`                        |
| `pattern = e[i]`                          | `member(e', i, pattern)`                               |
| `p[a, b]`, `ext[a]`                       | `V`, reading `p(a', b', V)`, `ext(a', V)`              |
| `R.p` (whole value)                       | `ref(T, A, "p")`                                       |
| `R` (a value given: an entry, an output, a `let`) | `ref(T, A, "")`; in a document, the provider's id of `T[A]` |
| `ref(R)`                                  | `ref(ref(T, A, ""))`: the reference, written out       |
| `R.p.q` (content)                         | `V`, reading `attr(T, A, "p", V)`; `__path(V, "q")`    |
| `settings[e].a.b`                         | `V`, reading `setting(e', "a.b", V)`                   |
| `n.k`, `c[e].k`                           | `V`, reading `output("n", "k", V)`; `instance_of("c", "", E), output(E, "k", V)` |
| `m.x` (`use m`; a value, a resource)      | `V`, reading `m::x(V)`; `T["m::x"]`                    |
| `s[k=v].o` (`use stacks.s`)               | `V`, reading `stack_output("s[k=v]", "o", V)`          |
| `world.T[e].a.b`                          | `V`, reading `cloud_attr("T", e', "a.b", V)`           |
| `x = R.p`, `R.p == c`                     | `attr(T, A, "p", x)`, `attr(T, A, "p", c)`: the read itself |
| `R.p` alone, `not R.p`                    | `attr(T, A, "p", true)`, `not attr(T, A, "p", true)`  |
| `not R.p == c`                            | `not attr(T, A, "p", c)`                               |
| `has R.p`, `not has R.p`                  | `attr(T, A, "p", _)`, `not attr(T, A, "p", _)`        |
| `has x.f`, `has R.p.q` (a walk)           | `Has = __path(X, "f")` after the read; `not` of it through a helper |
| `k == c`, `k`, `has k`                    | `k(c)`, `k(true)`, `k(_)`                              |
| `lifecycle(r, "f")`, `deformation(k, r, _)` (a column that takes a resource) | `r` as `ref(T, A, "")`: a value in a fact or head, taken apart in a body; `r` with no static type is the reference itself |
| `r == n`, `r != T[e]` (a resource on either side) | `R = ref(T, "n", "")`, `R != ref(T, e', "")`; a typed `r` is `ref(T, R, "")` |
| `x in T`, `x in resource`, `R in T`       | `want(T, x)`, `want(Type, x)`, `want(T, A)`            |
| `"n-${e}" in T`                           | `Name = format(..), want(T, Name)`                     |
| `x in world.T`                            | `cloud_exists(T, x)`                                   |
| `x in e`                                  | `member(e', x)`                                        |
| `x not in e`, `not x in T`                | `not member(e', x)`, `not want(T, x)`                  |
| `not { B }` (or a `not` of a nested path) | `not __neg_N(ȳ)`, `__neg_N(ȳ) :- P, B'`: ȳ the variables the body so far binds, `P` its positive literals |
| `a + b` (and `- * / %`)                   | `add(a, b)` (`sub mul div mod`)                        |
| `[t \| B]`                                | a `collect_list` helper rule over `B`                  |

`not R.p` holds when the attribute is absent, false, the API `null`, or
anything but `true`; it does not check that `R` exists (G-13). Write
`R in T` beside it when that matters.

A copy and an import are one mechanism (`modules::expand`): the body
under the scope `n`, its predicates `n::p` (a module's read as `n.p`, a
component's private to the copy, a value leaving it through an output),
an input `k` the cell `n::k(V) :- attr(input, "n", k, V)` with its default
at `@default`, its resources `n::x`, its writes needing no grant (ranks
decide); a top-level input also takes `--set`. A copy inside a copy puts
the outer scope in front (`edge.left::vpc`). `extern p(+a, -b) persist`
is asked on demand, and `declassify(v, "reason")` lowers a secret's label
(E DR-19).

A refinement (`check`, R-1) names the attribute or input by its own name,
as text (G-24): `input replicas: int = 2 check replicas >= 1`,
`db.backup_days: int check 1 <= db.backup_days <= 35`. `where` in a
refinement's place is an error naming `check`: `where` has one meaning,
the clause.

Removed from the grammar until the evaluator supports them: an open type
(`decl type T open`), the `null` literal and the ordered comprehension.
A flag on a `type` block attribute and a `type` block anywhere but the top
of the program are rejected with "not yet supported".

## Formatting

`dform fmt` prints the normal form of each construct (H section 3) and
keeps the author's other line breaks (at most one blank line in a row),
the spaces within a line and the indentation: a line is one step deeper
than the line that holds the innermost bracket, statement, block entry or
clause still open at its first token, and a line that starts with a closer
sits with the line that opened it. The normal forms:

- a body goes on one line (`where a, b`) when the line fits in 100 columns,
  else into a `{ }` block, one literal per line;
- `{ a: a }` is `{ a }`, and a block's entry `k = k` is `k`;
- a `provider` or `instance` with no entries has no block: `provider aws`;
- a header name is bare when it is a name, not a keyword, and not bound by
  the clause; else it is quoted;
- `not { lit }` of one literal whose names are all bound is `not lit`;
- `=` between two bound sides is `==`;
- `i = p[k]` with `i` fresh is `p(k, i)`;
- `env("prod")` for a value name is `env == "prod"`;
- the header is `key`, `input`, `input p(..) from`, before the
  body, each statement with the comments directly above it and on its line
  (see "The header").

It drops the commas a newline makes redundant (in blocks and `{ }` bodies)
and the trailing comma of a list or object. A formatted file prints back
byte for byte.

## Decisions the proposal left open

- The edition is `edition 2026` (the user's decision): there are no
  releases and 2026 was not formalized, so proposal H's grammar is edition
  2026 itself, and the grammar before it is gone.
- A read in any field gates the whole block, as F10 has it.
- Type namespaces are known from headers, `type` blocks, `type_*` facts
  and the built-in provider schemas; the resolver runs before providers are
  chosen, so a namespace no built-in schema closes may hold a provider's
  type it does not see (see "Types").
- `=` binds either side; `==` binds neither (G-28 is about how `fmt` prints
  them; here it decides which one may introduce a variable).
- An `output` with a body, or whose value reads, is the rule
  `output(k, t') :- B, reads`, in a module or a component too.
- A module reads its user's names outward, so it lowers only through the
  programs that use it (R-65).
- (R-65) A stack is a file under `stacks/` or one `[stacks.NAME]` names,
  for `use`; discovery keeps its fallback (with no `stacks/`, the root's
  files are the stacks).
- (R-65) `use` of a component item is an error naming `instance`: a
  module is imported, a component copied.
