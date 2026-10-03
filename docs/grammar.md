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
header := key | input                 ; in that order: key, input, input p from
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
relation inputs, `input p from ..`). `use` and `instance` are body
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
A block (`{ }` of a resource, `set`, instance or provider) and a
body block (`where { }`) separate their entries by a newline or a comma.

## Tokens

```
IDENT    := [A-Za-z_][A-Za-z0-9_]*       ; case decides nothing; "_" alone is the placeholder
STRING   := "\"" ... "\""                ; may span lines; escapes \" \\ \n \t \u{hex}; ${e} interpolates
INT      := [0-9]+                       ; -1 is unary minus applied to 1
QUANTITY := [0-9]+ ("." [0-9]+)? [A-Za-z][A-Za-z0-9]*   ; 1Gi 500m 1h30m 1.5Gi (R-66)
          | [0-9]+ "." [0-9]+                           ; 0.5: cores, in a cpu position
RANK     := "@default" | "@override"
COMMENT  := "#" to end of line
```

Punctuation: `( ) { } [ ] , . .. ..= : = == += != < <= > >= + - * / % |`.

`.` is always member access, `..` and `..=` a range's (R-56), and `/`
always division. A number with a letter adjacent is one QUANTITY token
(`1Gi`, `us-test-1a`'s `1a`), so it never splits into a number and a
name; what its unit means is the literal's ("Quantities and times").
`-` is always an operator: a hyphenated name is a string, and the parser
says so.

Statement keywords, recognised only as the first token of a statement (18):

```
edition  provider  key  type  decl  extern
input  output  let  set
component  instance  use
resource  deny  warn
```

`settings` is reserved: the statement of an earlier surface (R-38), an
error naming `set`.

Body words: `not in has`. The clause word: `where` (R-1). Literals: `true
false`. These six, and the reserved `if`, are never a name in a term; `if`
is an error wherever it stands, which prints the statement with its clause
spelled `where`. Anywhere a plain name is expected (a
key, a path segment, a declared name) any keyword is a name, and a keyword
followed by `(` is an atom or a call (`input("env", v)`). A statement
keyword may start a chain in a term. Contextual words in
declarations, where the position is fixed: `from`, `check`,
`mixed`, `as` (in `use`), the attribute flags (`required computed id
sensitive nullable`). `module`, `policy`, `import` and `export` are
words of an earlier surface: each is an error that names what to write
(R-65).
Roots: `world`.

### Strings and interpolation

A string is a string constant: every constant is quoted (`"prod"`,
`"set"`, `"audit.write"`). A path that is data is a string too (H-12):
`type_lattice(iam.policy, "statements", "set")`.

`"a${e}b"` interpolates the term `e` (H-13): it lowers to
`format("a%sb", e)`. `$${` is a literal `${`; a lone `{`, `}` or `$` is
itself. A hole may not hold a string (bind it first). A hole is a content
position: a dot in it reads now (see "Reference or read"). A literal part
may not contain `%s`.

A string may span lines (R-61), as in Lisp: its text is everything
between the quotes, newlines and leading spaces included, and nothing is
stripped, so what is written is what the provider gets. A hole works on
any line. For a script or a config file indented with the program,
`str.dedent(s)` removes the indentation its non-blank lines share and the
line break right after the opening quote:

```
resource compute.vm web {
  user_data = str.dedent("
    #!/bin/sh
    echo ${name}
  ")
}
```

`user_data` is `"#!/bin/sh\necho web\n"`. `dform fmt` and the editor
never re-indent the lines inside a string and never break a line inside
one.

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
| `decl p(..)`, `extern p(..)`, `input p from ..` | `predicate` | `p` |
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
| `world.T[e]`    | live objects of `T`     | the provider's name               | `cloud_attr(T, e, path, V)`          |
| `p[a, b]`       | relation or extern `p`  | every column but the last         | `p(a, b, V)`                         |
| `e[i]`          | a list value            | index (a fresh `i` enumerates)    | `member(e, i, V)`                    |

A name followed by `(` is a relation, a builtin or an extern; `m.p(..)`
is the relation `p` of the module `m` a `use` brings. Any other chain
`name (.seg | [terms])*` is resolved from its first name, innermost scope
first (rule, then component, then file, then program):

1. a typed variable (`x` after `x in T`): a reference;
2. a value name: a read of `k(V)`; a `let` whose value is a reference (a
   resource, a live object) reads through it (H-6);
3. `world.T[e]`;
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

Types are names of the core (`net.vpc`, `aws.vpc`, `k8s.deployment`).
A provider's types are `provider.type` (R-36): the provider's name is
their namespace, so `provider aws` serves `aws.vpc` and `aws.s3_bucket`,
`provider k8s` serves `k8s.deployment`, and a bridged Terraform type
drops its prefix's underscore (`google_compute_subnetwork` is
`google.compute_subnetwork`) while its attributes keep their names. A
type no provider of the stack serves is a plan error that names the
provider and the statement to add (`provider aws`); `dform provider
check` refuses a provider whose handshake name is not its types'
namespace. The fake cloud of the examples is the exception: one mock
playing several made-up namespaces (`net`, `compute`, `db`, `iam`).

The known types are every resource header's type, every `type` block's, the
first argument of a `type_*` fact (a schema's `type_provider`, `type_attr`
rows), and the types the built-in provider schemas declare. A dotted name
used as a type must be a known type, else `unknown type` (H-10): a typo is
an error, never a string. The built-in schemas close their namespaces but
`k8s` (a cluster's types are its own) and the mocks of part of a real
provider (`aws`, `google`); a type in a namespace no built-in schema
closes may be a provider schema's the compiler does not read, so `T[e]`
and `x in T` there take `T` as written.

`query` and `why` patterns are read without the program's declarations:
there, any dotted name that names nothing else is a type.

**Column types** (R-34). A relation's columns are typed: by its `decl`
where it has one, else by its uses. A non-string literal is its kind
(`az("a", 1)` gives `az(string, int)`); a string literal is unknown, as
in Postgres, until its column is settled. A variable in two columns makes
them one (a join, a head taking its body's column); a function's
parameter types its argument's column and its result the column it is
bound into (`s = inet.subnet(n, 8, 1)`); an input's type, an extern's
column and, with the provider's schema, an attribute read (`c = v.cidr`)
type the column they reach; `input p from FORMAT("path")` with no `decl`
takes its first document's columns. Two uses that disagree are an error
naming both; a column with a type checks each literal against it, a
string read as that type (`"10.0.0.0/8"` in a column `inet.contains`
reads is a network; `"foo"` there is an error); literals of two kinds
with nothing else are an error naming both; a column of string literals
only is a string. `n + 1` on a column that is no number, and a
comparison of two types that are never equal, are errors, not a silent
non-match. A column declared `any` (`decl release(key, value: any)`)
takes every type and joins nothing. Variables are never coerced, but a
string column may hold the text of an `inet` or an `ip` a function reads.
The settled signature (`az(string, int)`, a `decl`'s or a rule head's
column names where there are some) is what the editor's hover prints.

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

### Quantities and times

A quantity is a number with its unit, held in the dimension's base unit
(R-66): `bytes` (bytes), `cpu` (millicores) and `duration` (R-62). A
quantity literal is one token, its unit adjacent:

| type       | written                                             | base       | prints          |
|------------|-----------------------------------------------------|------------|-----------------|
| `bytes`    | `512Mi`, `1.5Gi`, `2Ti`, `4096` (`Ki Mi Gi Ti Pi`)  | bytes      | `1536Mi`, `512` |
| `cpu`      | `2`, `500m`, `2000m`, `0.5`                         | millicores | `2`, `500m`     |
| `duration` | `30d`, `6h`, `1h30m`, `1y6mo` (`y mo w d h m s ms us ns`), `"PT6H"` | months, days, ns | `1h30m` |

One spelling per unit: bytes take binary units only, and `20GB`, `1kB`,
`512MiB` are errors naming the unit to write (`20Gi`, or the byte count
`20000000000`); any magnitude in a unit is fine (`2000m` is `2`, `1.5Gi`
is `1536Mi`), a fraction that is not a whole base unit is an error
(`1.3Gi`). A duration's numbers are whole with one unit each, largest
first (`1.5h` is an error naming `1h30m`); in a string a duration may be
ISO 8601 (`"P1M"`). A bare integer is bytes in a `bytes` position and
cores in a `cpu` one; in a `duration` position it is an error (`30s`).
Every quantity prints canonically, in the largest unit that divides
exactly, so equal values print alike; `string(q)` and `"${q}"` give
that.

`m` is millicores in a `cpu` position and minutes in a `duration` one, so
`500m` (and a bare fraction, `0.5`) is read by its position: an
attribute, an input, a function's parameter, or the other side of an
operator (`1h + 30m`, `cpu(1) > 500m`). Where nothing gives it a type
the literal is an error naming both readings; `cpu(500m)` and
`duration(30m)` say which.

The algebra (R-66 amendments 3, 4): a quantity scales by a number
(`512Mi * 2`, `max_size / 2`, whole base units as integer division),
adds, subtracts and compares only within its dimension, `sum`, `min` and
`max` aggregate within one, and a quantity over one of its own dimension
is a plain number (`limits.cpu / requests.cpu <= 4`). Nothing changes a
dimension: `1Gi + 500m`, `1Gi + 2`, `512Mi * 2Gi` are compile errors where
the operands are literals; at evaluation a mix has no value. Two
durations compare with a day taken as 24 hours; months compare only with
months (`1mo` and `30d` do not, a month's length depends on the date).

A `time` (R-62) is a zoned instant, the Temporal model: written as a
string in a `time` position or to `time(..)`, RFC 3339 with an offset
(`"2026-10-02T09:00:00Z"`, `Z` is UTC) or a date and time with a zone
(`"2026-10-02T09:00[Europe/Paris]"`). It carries its zone, prints as
`2026-10-02T09:00:00+02:00[Europe/Paris]`, and orders by its instant
whatever the zones (`<`, `time.before`, `min`); `==` is the same instant
in the same zone, as a value is equal only to itself. `t + d`, `t - d` and `time.add(t, d)` are
calendar-aware in the time's zone: a month is a calendar month (the 31st
plus a month is the next month's last day), a day a calendar day across
a DST change (23 or 25 hours). Zones are the tzdb built into dform
(`dform version` prints its release), never the host's. There is no
time literal, and no `now` function: the current time is the `time`
provider's extern, `time.now()` ("Memo").

A provider takes a quantity or a time in the form its schema gives the
attribute's type: `bytes(quantity)` (Kubernetes's string, the default),
`bytes(gib)`, `bytes(mib)`, `bytes(bytes)` (whole numbers),
`cpu(quantity)`, `cpu(millicores)`, `duration(friendly)`,
`duration(iso)`, `duration(seconds)`, `time(rfc3339)`. `storage = 20Gi`
is then one spelling for every provider; a value its form cannot hold
(`1536Mi` as whole GiB) is a plan error naming the attribute, and what a
provider sends back at such an attribute is read the same way.

A `url` is parsed at the edge, like `inet`: written as a string in a
`url` position or to `url(..)`, and printed canonically (its scheme
lower-case, its path and query percent-encoded consistently) so equal
URLs print alike. `url.join`, `url.with_scheme`, `url.with_host`,
`url.with_port`, `url.with_path` and `url.with_query` change one part
and reprint the rest; `url.parse` breaks every part out as a plain
object instead, for reading rather than rebuilding.

A literal in a position whose type is known is checked as that type at
compile time (R-31, Postgres's unknown-literal rule): a schema attribute's
type (`inet`, `int`, `bool`, `url`, `enum(..)`, `ref(T)`), an input's declared
type for its default and a copy's value, a function's parameter.
`cidr_block = "10.0.0/16"` in an `inet` attribute, `vpc = "main"` in a
`ref(net.vpc)` one and `subnets = [main]` (a `ref(net.vpc)` where
`ref(net.subnet)` is wanted) are errors at the entry, naming both types; a
string literal where an `inet` is declared is read as one, and so is one
where a quantity or a `time` is (`memory = "512Mi"`). Without a type
a literal is a string; a quantity literal is its unit's. A reference and a string never compare: `r ==
"main"` is an error that names `r == main` or `r == T["main"]`.

## Statements

```
stmt       := KEYWORD ...                      ; one production per keyword, below
            | NAME ("." NAME)* "(" args ")" RANK? ("where" body)?   ; a fact or a rule

provider   := "provider" NAME block?                ; no block when it has no entries
type       := "type" NAME "=" type | "type" DOTTED attrs
decl       := "decl" DOTTED columns "mixed"?
extern     := "extern" DOTTED "(" bindarg ("," bindarg)* ")"
bindarg    := ("+" | "-") NAME (":" type)?
input      := ("input" | "key") NAME ":" type ("=" term)? ("check" body1)?
            | "input" NAME fields                  ; an object input (R-54)
            | "input" NAME ("from" term ("where" body)?)?   ; rows of a relation (R-55)
fields     := "{" (field SEP)* "}"
field      := NAME ":" (fields | type ("=" term)? ("check" body1)?)
output     := "output" NAME (":" type)? ("=" term)? ("where" body)?
            | "output" NAME ofields ("where" body)?         ; an object output
            | "output" NAME                        ; a relation exported (R-55)
ofields    := "{" (ofield SEP)* "}"
ofield     := NAME (":" type)? "=" term | NAME ":" ofields
let        := "let" NAME "=" term RANK? ("where" body)?
set        := "set" chain ("=" | "+=") term RANK? ("where" body)?
            | "set" "{" (chain ("=" | "+=") term RANK? SEP)* "}" RANK? ("where" body)?
            | "set" "from" term selector? RANK? ("where" body)?   ; a document's leaves (R-38)
selector   := ("." SEG | "[" "*" "]")+                ; a path into a document (R-39)
use        := "use" path ("as" NAME)? cblock? ("where" body)?
instance   := "instance" path NAME cblock? ("where" body)?
component  := "component" NAME stmts             ; an item of a module
path       := NAME ("." NAME)*                    ; a/b.df from the root; std.x; a package mount
resource   := "resource" DOTTED hname RANK? block ("where" body)?
deny, warn := ("deny" | "warn") STRING object? ("where" body)?
stmts      := "{" (stmt NL)* "}"

attrs      := "{" (attrdecl SEP)* "}"
attrdecl   := blockpath ":" (attrs | type flag* ("check" body1)?)
flag       := "required" | "computed" | "id" | "sensitive" | "nullable"
block      := "{" (entry SEP)* "}"
entry      := blockpath (("=" | "+=") term)? RANK?   ; `zone` alone is `zone = zone`
cblock     := "{" ((entry | row) SEP)* "}"          ; a `use` or `instance` block
row        := NAME "(" args ")" ("where" body)?     ; a row of a relation it takes
            | NAME "from" term ("where" body)?
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
on the head's line, and a block is a head. A resource, `set` block,
instance or `use` takes at most one clause, after its block's `}` (and a
`set` block's rank): `resource T n { .. } where B`, and a body of
several lines is `} where {`, one literal per line, closed by its own
`}`. The clause is a query, and the block is one resource (or set of
contributions, or copy) per match. A `provider` block takes no
clause. `if`, the clause word of an earlier surface (H-3), is an
error wherever it stands, and the error prints the statement with its
clause spelled `where`.

The verb says what a statement gives (R-57): `=` gives a value, `from`
gives rows. `input x: T = d`, `let x = t` and `output x = t` are values;
`input p from DOC` and a copy's `p from TERM` are relations, a row per
element, and `set from DOC` a document's leaves. A value written with `from`, or rows
with `=` (`input p = [..]`), is an error that says so; rows written in
the program are facts, `p("a", 1)`.

An entry that is only a path is the pun of its last segment (R-33), as
`{ a }` is `{ a: a }` in an object: `availability_zone` alone is
`availability_zone = availability_zone`, `spec.selector.color` is
`spec.selector.color = color`, and a rank may follow (`tags @default`).
The segment is resolved where the value would be, as a clause variable, a
value name or anything else a bare name can be; a path whose last segment
is not a name (`a[0]`, `"a-b"`) is an error. A provider's `source` is a
constant, never a pun.

`set` is the contribution statement (H-5): the chain is a resource's
attribute, or an input (a stack input, a field of an object one, a used
module's, `set traefik.acme_email = ..`, or a copy's, `set blue.cidr =
..`). A `set` with no `where` on a resource, a copy or a used module
declared in the same scope is an error that names the block to write the
entry in; a top-level `set` of the program's own input is an error too
(give it a default, or pass `--set`). `set { a.b = 1, c = 2 } [@rank]
[where B]` is several under one clause and rank, each entry a `set`'s,
and `set from DOC` gives the inputs from a document ("Giving inputs"). `scenario` is gone (R-32): the
program's denies are its tests, and `dform test` runs them over the
inputs' values; a what-if plan is `plan --set k=v`.

A list the schema keys (`type_list_key(k8s.deployment,
"spec.template.spec.containers", ["name"])`) is written by element
(R-35): `set w.spec.template.spec.containers[k].resources.limits = ..`
writes the element whose key is `k`, any term but an integer (an integer
stays the position): a variable, a path (`c.name`), a string
(`containers["api"]`), an object for a list keyed by several fields. Its
content joins that element leaf by leaf at the write's own rank, so a
`@default` gives way to the element's own value and two writes of one
leaf at one rank conflict, named by the element (`containers[name=api]`);
a key no list has makes the element. Whole lists from several authors
merge by key too, the highest rank's lists winning as a set's do. A list
with no key is one value: writing an element of it is an error naming the
`type_list_key` to declare. One index per written path: below the element
the path is fields. A variable an `in` binds to an element of a
resource's list (`c in w.spec.template.spec.containers`, or `(i, c) in`)
is that element (R-69): `set c.resources.limits = { cpu: 500m, memory:
256Mi } @default where w in k8s.deployment, c in
w.spec.template.spec.containers` writes `w`'s container `c` by its key,
as `containers[c.name]` would; over a list with no key it is the error
above. The rule that writes elements reads its resource's
attribute without element writes (its base: the blocks' and whole-list
writes), so it may read the list it writes (`not has c.resources.limits`)
without a cycle; it does not see another rule's element writes.

`let k = t [@rank] [where B]` is a value (H-6), a cell of the attribute
aggregate like an input (R-3): each row contributes to the cell `(let,
SCOPE, k)` (scope `""` for the program's, `n` in the copy or import
`n`), and a read
of `k` reads the collapsed cell. Rows that agree are one value; two that
disagree at the winning rank are a conflict naming both; a `@default` row
gives way to any other. When `t` is a reference (a resource, a live
object), `k`'s value is that reference and its static type is the
reference's, so a dot on `k` reads through it: `let pg =
db.postgres["main"]`, then `pg.endpoint`.

`output k: T = t [where B]` is one statement (H-7): the type is optional (an
untyped output is `any`), the value is not ("Inputs and outputs").

`deny "m" {ctx}? where B` and `warn` are the checks (H-8). The message is a
string like any other: `${e}` reads the body's variables. A deny is checked
after evaluation; no rule may read `deny` or `warn`.

A relation is declared by its columns (H-11): `decl p(a, b)`, a type on a
column optional; `mixed` lets it have both facts and rules. A copy's
relations are its own (`n::p`, which no source spells); a value leaves it
through an `output` (DESIGN.org R-5), a relation through `output p`
("Inputs and outputs"). A module's are its import's, `m::p`, read as
`m.p(..)`.

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

### Inputs and outputs

Inputs and outputs are one grammar in both directions and in every scope
(R-55): a declaration is a value, `input|output k: T [= t]`, an object
by its fields, or a relation.

**Values.** `input k: T [= d] [check B]` is a cell of the attribute
aggregate the outside gives: the default contributes `@default`, `set k
= t where B` and an `--input-file`'s `k(v)` at the normal rank unless
marked, and `--set k=v` at `@override` ("Giving inputs").
`output k [: T] = t [where B]` hands a value out (H-7): read as
`n.k` from a copy or a used module, `c[t].k` from every copy, and
`stack[k=v].k` from another stack's deployment.

An object input is declared by its fields (R-54): `input nodes { flavor:
string = "b3-8", count: int = 1 check 1 <= count <= 3, pool: { size: int }
}`, a nested object in braces, each field's default and check its own (the
check names the field or its path). It is one cell per leaf: the field's
default contributes `@default` to `nodes.count`, the aggregate merges the
leaves into the object `nodes`, read whole (`nodes`) or by a path
(`nodes.count`). `input nodes: node_pool = { .. }`, an alias of an object
type and an object default, is the same input with the same leaves. Every
way of giving it addresses a leaf or an object by its path: `--set
nodes.count=2` (read as the leaf's type; a path that names no field is an
error listing the object's fields), `--set nodes=@nodes.yaml` (each field
the document has; one the object has not is an error), `set nodes.count = 2
where env == "prod"`, an instance block's `nodes.count = 2` or `nodes = {
.. }`, an input file's `nodes({ count: 2 })`. A field with no default is
required like an input, by its path. `why nodes.count` shows the leaf's
layers. A `key` is a scalar and takes no block.

**Relations.** A relation is declared once, by `decl p(a: T, ..)` or by
its uses (R-34); `input` and `output` name it and never re-spell its
columns:

- `input p from TERM [selector] [where B]`, in a stack, gives `p` rows
  out of a document ("Documents"): a loader's, `csv("data/p.csv")`, a
  selection into one, `toml("net.toml").peerings`, or any document value,
  an input or a `let`, read with the decl's columns and checked against
  their types. Several lines are one
  relation, their rows together, and facts the program states join
  them. With no `decl`, the columns are the first source's: the first
  row's keys in the document's order (a CSV's header), typed by their
  values where they say (an int, a bool) and by the program's uses
  ("Column types"); later lines are read with the same columns. A first
  source the compiler cannot read now (`git(..)`, a path with holes, a
  selection, a document value, a missing file) is an error that says to
  declare the columns.
- `input p`, in a module or a component, is a relation its user gives.
  The `use` or `instance` block gives its rows beside the values:
  `zone("a", 0)`, a rule over the user's relations `zone(z, n) where
  az(z, n)` (a row with a clause ends its line), or a table `zone from
  csv("zones.csv") [where B]` with the module's columns. The rows are the
  copy's own relation, written in the user's scope, and exist while the
  copy does. A relation the module does not take is an error naming
  those it does.
- `output p` exports the relation `p` the scope declares or defines: a
  copy's rows are read `blue.p(x, y)` and every copy's `network[t].p(x,
  y)`, one fact per row, and a stack's `output p` publishes its rows,
  read `platform[env=e].p(x, y)`. A column the `decl` types by a
  resource type (`decl subnet(s: net.subnet)`) holds the copy's
  resource and leaves it as its address. A copy's relation it does not
  export is its own: reading it is an error that names `output p`, and
  another copy's relation is never a head. A used module's relations are
  public, `m.p(..)`.
- `output k { f = t, g: T = u } [where B]` is an object output by its
  fields, typed by them (`any` where a field gives no type).

A stack's inputs are flat (R-55): its own and every used module's, by
the module's name, `--set traefik.acme_email=..`, `why traefik.acme_email`,
and `dform test` enumerates them all. A used module's input nothing gives
is the stack input's error, at the `use`. Only a component's inputs nest:
its instance block gives them.

### Giving inputs

Configuration is the inputs (R-38), and a `set` is a contribution to
one, under a condition:

```
set db.backup_days = 30 @override where env == "prod", region == "eu-west-1"
set { db.multi_az = true, db.backup_days = 14 } where env == "prod"
set { traefik.acme_email = "ops@example.com" } where env != "dev"
set from yaml("config/${env}.yaml")
```

A `set`'s target, alone or an entry of a block, is the program's own
input (`region`), a field of an object input or the object
(`db.backup_days`, `db = { .. }`), a used module's (`traefik.acme_email`,
the cell `(input, traefik, acme_email)`), or a copy's (`blue.cidr`); a
path that names no field is an error naming the object's fields, and a
key is the target's. A block, `set { a.b = 1, c = 2 } [@rank] [where
B]`, is several `set`s under one clause and rank, each entry its own
`set` (any target, an entry's rank its own), one rule per entry:
`arg(input, "", path, t, Rank) :- B, reads`. A `set` of the program's own
input, or of a used module's or a copy's declared in the same scope, has
a clause (H-5); with none it is an error naming the default or the block
to write.

The layers are ranks, never specificity: the declaration's default
(`@default`) < a `set` (normal unless marked) < `--set` (`@override`).
Two `set`s that both hold and give one leaf different values at the
winning rank are a conflict naming both, so a broad one says `@default`
and a narrow one that should win says `@override`. A read is the input's
name, `db.backup_days`, and `why db.backup_days` shows the layers, each
where it is written.

`set from DOC [@rank] [where B]` gives every leaf of a document to the
input at its path, a leaf by its dotted path (`db: {backup_days: 14}` is
`db.backup_days`; a CSV document has the columns `path` and `value`);
the document is a loader call, a selection into one, or any document
value ("Documents"); a string is read as the input's type by its
constructor (an `inet`, a quantity, a time; a CSV cell as an `int` too).
A leaf at a path that is no input is a deny naming the file and line and
the inputs there are. The document is the table `set(path, value)` read
by the file provider, one rule per input the scope gives
(`tables::expand_set_from`). This replaces dform.toml's `config`, and
needs no clause.

An input a `set` gives with no default is required only in the
deployments none of them holds in: there it is a violation, `input k is
required and has no value`. `dform test` leaves it to the program: it is
no axis of the space.

Gone (R-38): the `settings` statement and its rows (`settings prod { ..
}`, `settings _`), their reads (`settings[e].p`, `let cfg =
settings[env]`), the `settings` pseudo-type (`type settings`,
`type_lattice(settings, ..)`), and a stack's `config`; each is an error
naming the form to write.

**A list or a relation.** A list is a small ordered value handled whole:
`verbs = ["get", "list"]`, an attribute's value, a document's array. What
is iterated, joined or keyed is a relation: a row per thing, read a row
at a time, given by rows (`--set`, `set` and `why` address a relation
by its rows, never its position). An output that is a list of every
subnet is a table, `output private_subnet`; an attribute that takes a
list builds it where it is set, `subnets = [ s | subnet(s), s in
net.subnet ]`.

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
the value of its key `k` (in `backend`). The list is closed; any other
key is an error naming it, and `config`, gone (R-38), one naming `set
from` ("Giving inputs"):

| setting      | value                                                                 |
|--------------|-----------------------------------------------------------------------|
| `backend`    | where the state lives: `'local("DIR")'` or `'s3("BUCKET", "PREFIX", {endpoint, region})'` |
| `role`       | `"bootstrap"`: it creates what a controller runs in, and stays batch  |
| `approvals`  | who approves a plan: `'jwks("URL")'`, `'jwks_file("PATH")'`, or a list |
| `audit_sink` | a command each audit log entry is piped to                            |
| `isolated`   | `true`: each key value deploys into its own account (needs a key)     |

The loader reads them into one `Stmt::Stack` of the program, at their
place in dform.toml, so an error in one is reported there.

### Provider blocks

`provider NAME { .. }`, or `provider NAME` with no settings, names a
provider the program uses; a program with no `provider` statement starts
none, and what evaluates it against providers (`plan`, `apply`, `query`,
`why`, `test`) refuses it, naming the fix (`dev --provider` runs it
anyway). Its `source` is a constant (the stack reads it to start the
provider; without one, dform.toml's `[providers]` entry of the name).
Every other setting is the provider's own, which its schema may declare;
one it does not is passed to Configure as written. Each is a term, read like a rule's
(inputs, value names, tables, `env.var`), and the block
lowers to one rule for them all, plus one for `expect_account`:

```
provider p { k1 = t1, k2 = t2 }    provider_config("p", { k1: t1', k2: t2' }) :- reads
expect_account = t                 provider_expect_account("p", t') :- reads
```

A setting is a content position: a dot in it reads now. A block takes no
clause, no `+=` and no rank; a setting given twice is an error.

A `provider` statement also brings the provider's externs into scope, with
their binding modes (DESIGN.org R-8): a program does not write `extern`
for them. `file`, `env` and `time` are built-in fact providers, declared
like any provider and needing no `dform.toml` source (`externs::BUILTINS`);
dform answers them itself:

```
provider file         file.text(+path, -value: string); the loaders, `yaml(p)` .. ("Documents")
provider env          env.var(+name, -value: secret(string))
provider time         time.now(-t: time)
provider aws          aws.availability_zone(+state, -name: string, -index: int)
```

An extern is asked again every run (a plan file records what its plan
read, and its apply reads that): nothing keeps an answer but `memo.first`
("Memo"). `random` is not a provider: `provider random` is an error
naming the std functions `random.password` and friends ("Functions").

A data source (Terraform's `data` block) is such an extern, and a table:
`aws.availability_zone("available", az, n)` binds each zone's name and its
`index`, a stable ordinal the provider defines (the position among the
names sorted, unless the API has an order of its own), so a program
enumerates zones with a column and a plan never reshuffles. The aws mock
answers it from `crates/dform-mock/schemas/aws-mock.externs.df`.

`extern file.text(..)` in a program is an error naming the `provider`
statement to write instead. `env.var(t)` as a term is the lookup
`env.var[t]`, `time.now()` the lookup `time.now[]`; without the
`provider` statement either is an error that says to declare it.
`persist` after an extern is an error naming `memo.first`. `extern`
stays the schema's word: provider schemas and the compiler's tests
declare externs with it, and so, until the compiler reads a provider's
schema (DESIGN.org R-24), does a program for a provider that is not
built in.

### Memo

`memo.first(+key: string, +candidate, -value)` keeps a value across runs
(R-60): the first candidate ever given for a key is the value on that
run and every later one, whatever the candidate becomes. It is a built-in
relation, in scope with no `provider` statement; `memo.first(k, c)` as a
term is its value. The program says what is kept, where it reads it:

```
let pw = memo.first("db-pw", random.bytes("db-pw", 32))   # made once, kept
let created = memo.first("db-created", time.now())        # observed once

# The rotation idiom: a creation time kept, compared with the clock.
warn "rotate the database password" where {
  memo.first("db-created", time.now(), created)
  time.before(time.add(created, 30d), time.now())
}
```

Within a run the first call of a key answers every other one, so two
sites agree. A plan keeps nothing; an apply keeps what it read in the
deployment's state when it completes a tick. `dform state taint memo KEY
[TARGET]` forgets a kept value: the next run gives the candidate again,
and the next apply keeps it. `why` names a kept value's source as
`memo, first kept <when>`.

A memo whose candidate is a secret (the secrets pass decides, per
literal) keeps a secret: state holds it sealed with a key derived from
the stack's key file (`state.key`, which moves with the state), the run
that reads it opens it in memory, and neither the plan file nor any
output carries it in the clear. A memo of a public candidate is kept as
it is, readable in `state show`.

Use a memo for what cannot be produced again: a time observed once, a
value an API generated, a secret that must survive a change of master.
A derived secret (`random.password(key)`) needs none: it is the same on
every run by construction.

### Type aliases

`type NAME = TYPE` names a type: `type environment = enum("dev", "stg",
"prod")`, then `input env: environment` and `decl peering(env:
environment, ...)`. An alias is usable anywhere a type is (an
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
its `use` binds or its path (`config.environment`, `network.node_pool`).
Two aliases of one name in one scope are an error listing both.

### Documents

Data that is not code is a document (R-39), loaded by the file
provider's loaders, spelled bare: `yaml(PATH)`, `toml(PATH)`,
`json(PATH)` and `csv(PATH)` (a list of objects by its header, every cell
text), each also over `git(REPO, REF, PATH)`, read at the commit the ref
names, which the plan file records, so `apply PLAN` reads what plan read
though the branch moved since. A path is a term (holes allowed: a hole is
a content position, so it reads now), from the project root. A loader
call is a value: `let net = toml("data/network.toml")`, then
`net.region`, `net.az[0].name`. `file.json` is gone; `file.text(PATH)`
stays, the file's text.

`input p from DOC [selector] [where B]` destructures a document into the
relation `p`, by the columns of its `decl`, or with no `decl` by its first
source's (R-34; "Inputs and outputs"):

```
input az from toml("data/network.toml")                 # its [[az]] tables
input peering from toml("data/network.toml").peerings    # a selection
input service from yaml("teams.yaml").teams[*].services  # every team's
input vlan from vlans                                    # an input, list(vlan)
```

A list of objects is a row per object, each field a column by name; a
whole TOML document is its `[[p]]` tables, by the relation's name (the
document may hold other relations' too). A selector is a path into the
document, `.name` a field and `[*]` every element of a list, chained; a
list at its end is its elements. A column a row lacks is the nearest
enclosing object's that has it; else it is an error naming the row, and
so is a field no column takes and a cell that is not its column's type
(read to it as an input's, `inputs::check_type`, never `secret`). Rows
read from a file carry their place (`net.toml:7`, `teams.yaml:row 2`),
which `why` prints. A `.df` file of facts is a module (`use
data.releases`, then `releases.release(app, k, v)`), re-read like any
program file; `facts(..)` is gone, an error that says so. A copy's
relation input takes a document the same way, `p from DOC` in its
`instance` or `use` block.

A loader's table lowers to externs (`src/tables.rs`), the source its bound
inputs, the selector in the table's name:

```
extern table.FORMAT.p(+path, -at, -col: type, ...)
p(Col, ...) :- B, reads, Path = PATH', table.FORMAT.p(Path, At, Col, ...)
decl p(..) mixed
```

from git, the ref resolved to a commit first:

```
extern table.git.p(+repo, +ref, -commit)
p(Col, ...) :- reads, Repo = .., Ref = .., Path = .., table.git.p(Repo, Ref, Commit),
               table.FORMAT.p(Repo, Commit, Path, At, Col, ...)
```

any other document value, `table.value.p(+doc, -at, -col, ..)`, answered
in process; and a loader call as a value, `table.FORMAT.document(Path,
At, V)`. The controller watches every file and ref a run's tables and
documents read, and every program file.

`set from DOC` is the table `set(path: string, value: any)`, every leaf
of the document a row, read into `arg(input, "", Path, Value, Rank)`;
`transform` expands that rule into one per input the scope gives, and a
deny for a leaf at any other path ("Giving inputs").

### Block names

A resource's header name is a string or a name. A string
with holes (`"private-${z}"`) is the variable `Addr`, bound last in the
body by `format`. A name the block's clause binds is that variable
(`resource net.vpc t { .. } where tenant(t, i)`); any other name is the
static name (`resource net.vpc shared`), and it may not be a value in
scope (`resource net.vpc env` with `input env` is an error: write
`"env"` for the literal one). A block (header,
entries, clause, interpolated names) is one rule; a header name's scope
is its block and its clause. The clause follows the block, so a header
name it binds is read forward: the header names a variable the reader
meets in the clause below, as a rule's head names variables its body
binds (R-1 keeps the rule and moves only the clause).

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
            | pattern "=" term                   ; a tuple or object pattern matches (R-58)
            | term "in" ("resource" | term | range)
            | tuple "in" term                    ; `(k, v) in obj`, `(i, x) in list`
            | term "not" "in" (term | range)
cmpop      := "=" | "==" | "!=" | "<" | "<=" | ">" | ">="
atom       := chain "(" args ")"
args       := (arg ("," arg)* ","?)?
arg        := term | tuple | NAME ":" term       ; a named argument: its column's name

range      := add (".." | "..=") add          ; only after `in` (R-56)
term       := add
add        := mul (("+" | "-") mul)*
mul        := unary (("*" | "/" | "%") unary)*
unary      := "-" unary | primary
primary    := INT | QUANTITY | STRING | "true" | "false"
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
first: `+ -` (left), `* / %` (left), unary `-`. An aggregate (`count(x)`,
`sum(x)`, `collect_set(x)`, ...) is bound in a body, `n = count(x)`
("Aggregates"). Named arguments
(`project(id: i)`) are for a relation declared with named columns; they
lower to a record pattern.

Membership (H-9): `x in e` for a list, `x in T` for a type, `x in
resource` for any, `r in NS` for any resource of a provider's namespace,
`x in E` for each value of an enum type, `x in world.T` for a live
object, `i in lo..hi` for
the integers from `lo` up to `hi` (half-open) and `i in lo..=hi` up to
and including it (R-56); `(i, x) in e` gives each index and element of
a list, `(k, v) in e` each key and value of an object (R-58,
"Patterns"), and `x = e[i]` the element at an index. A
range's ends are bound integers, and it is enumerated in order. A range
is for "once per i", things that have a position and no identity: a
replica, a shard, the n-th /24; anything with a name is a relation, a
row per thing (R-55). A range anywhere but after `in` is an error, "a
range is enumerated with `in`; `[lo..hi]` is not a list", so it never
becomes a list by accident: `int.range(lo, hi, step)` is the function
that gives one.

`x in E`, `E` an enum type alias (`type environment = enum("staging",
"prod")`), binds `x` to each value in the order the type declares them,
as a range does, and `(i, x) in E` each with its position (R-70): a
bucket per environment is `resource T "b-${e}" { .. } where e in
environment`, with no fact restating the values. `why` shows the type as
the leaf. An input of an enum type holds one value, so `x in env` is an
error that says to name the type and range over it; an inline
`enum(..)` has no name, so it says to declare one. `dform test` takes an
enum input's values from the same type, in the same order.

`r in NS`, `NS` a provider's name (R-36: its types' namespace, `k8s`), is
a resource of any type in it (R-49): `set r.metadata.labels.owner =
"platform" where r in k8s` labels every Kubernetes object and nothing
else; `r in resource` stays every resource of any provider. Its types
are the program's own in the namespace and the built-in schemas' the
provider of that name serves (the fake mock's `k8s.cluster` is
`fakecloud`'s). `r.p` reads an attribute every one of those types has
that the compiler knows the attributes of; otherwise it is an error
naming the types that lack it. With `r` already bound by a reference
column (`deformation(k, r, _)`, a plan row), `r in k8s` tests its type,
so a deleted object binds the same way.

### Aggregates

```
lit1      := NAME "=" aggregate | ...
let       := "let" NAME "=" aggregate ("where" body)?
aggregate := ("count" | "sum" | "min" | "max" | "any" | "all"
            | "collect_set" | "collect_list") "(" term ")"
```

`n = count(x)` in a body binds `n` to the fold of `x` over the body's
matches (R-59). The groups are the head's other variables: in
`subnets(v, n) where n = count(s), s in aws.subnet, s.vpc_id == v` there
is a count per `v`. A head with no other variable is one group, and so is
a `let`: `let n = count(s) where s in aws.subnet`. The literal may stand
anywhere in the body; a literal that reads `n` (`n > 3`) is applied after
the fold, and what it reads of the body is part of the group too
(`over(v) where n = count(s), subnet(s, v), limit(v, m), n > m` groups by
`v` and `m`). Two aggregates in one body fold over the same body, group
by group. What an aggregate folds is bound by the rest of the body:
`n = count(x)` with no literal binding `x` is an error, "`count(x)`
aggregates `x`, which the body does not bind". An aggregate anywhere else
(a head, a field, an argument, inside `not { }` or a comprehension) is an
error naming the body form.

| aggregate         | of        | gives                                                    |
|-------------------|-----------|----------------------------------------------------------|
| `count(x)`        | anything  | the number of the body's matches                         |
| `sum(x)`          | ints      | their sum                                                |
| `min(x)`, `max(x)` | ints or strings | the least, the greatest                         |
| `any(x)`, `all(x)` | bools    | whether some, whether every, is true                    |
| `collect_set(x)`  | anything  | the distinct values, as a list in sorted order           |
| `collect_list(x)` | anything  | every value, in the order of the body's rows             |

The order of the body's rows: the first relation the body reads, in its
order (a relation's rows sorted, a list's elements as the list has them,
`i in lo..hi` counting up), then within each of its rows the next
relation's, and so on; so `collect_list(z)` over `zs(l), z in l` is `l`
itself. A comprehension `[t | B]` is a `collect_list` over `B` and keeps
the same order. A value of the wrong kind in a group (`sum` of a string)
derives a deny naming the group instead of its fact, and a group with a
null in a folded value is undetermined (`count`, `sum`, `min`, `max`,
`any`, `all`; the collects keep the null). An empty group derives nothing:
a `let n = count(x) where B` with no match of `B` has no value, so "none"
is `not { B }`, not `n == 0`. `why` of an aggregate's fact prints the
statement and the rows of its group.

### Patterns

```
pattern    := "_" | NAME | literal | tuple | "{" pfield ("," pfield)* ","? "}"
tuple      := "(" pattern "," pattern ("," pattern)* ","? ")"
pfield     := key (":" pattern)?                ; `{ a }` is `{ a: a }`
```

One production, in three places (R-58): on the left of `in`, on the left
of `=` in a body (a rule's, a check's, a `let`'s clause), and as a
relation's argument. A name binds, or compares when it is bound
already; `_` matches anything; a literal compares. A tuple needs the
exact arity: `(a, b) = pair` matches a list of two elements and fails
the match otherwise. An object pattern binds the fields it names and
ignores the rest: `{ host, port } = conn` matches `{host: "db", port:
5432, user: "app"}`, and an object without `port` does not match.
Patterns nest: `(i, (n, x)) in pairs`, `(env, { cidr: c }) in nets`.

- After `in`, a tuple is `(key, value)` of an object or `(index,
  element)` of a list: `(k, v) in labels` once per label, `(i, x) in xs`
  once per element. `x in obj` is an error naming the pattern: an
  object's entries are `(key, value)`. `not (k, _) in obj` holds when no
  entry matches. An object on the left of `in` is a value, as before:
  `{ a: 1 } in xs` looks for that object.
- Against a function's result, a pattern is a test as well as a
  binding: `(repo, tag) = str.split(image, ":", 1)` binds both when the
  image has a tag and fails otherwise, so `not (_, _) = str.split(image,
  ":", 1)` reads "no tag".
- A relation of several named columns (`decl zone(name, index)`) takes
  one object pattern, the record pattern of the columns it names:
  `zone({ name })` reads the names, as `zone(name: name)` does; a tuple
  argument matches a list column, `pair((2, s))`.

A tuple is a pattern and never a value: in a field, a head or the right
of `=` it is an error that says where a pattern goes, and a list is
`[a, b]`; a list on the left of `=` is written as a tuple. `why` prints
the statement with its patterns as written.

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

A copy's output `n.k`, a value name, `p[..]` and
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
param      := NAME "?"? ":" type             ; `?`: a call may leave it out (the last ones only)
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
| prelude   | the constructors `int(s)`, `string(x)`, `inet(s)`, `ip(s)`, `iprange(a, b)`, `bytes(x)`, `cpu(x)`, `duration(x)`, `time(s)`, `url(s)` (a literal argument read at compile time); `format(t, v, ...)`, `len(x)`, `ref(T, n, p)`, `scoped(s, n)`, `cloud_ref(T, n, p)`, `declassify(v, why)` |
| `inet`    | `inet.subnet(net, bits, n)`, `inet.host(net, n)`, `inet.addr(net, n)`, `inet.contains(net, a)`, `inet.overlaps(a, b)`, `inet.prefix_len(net)` |
| `int`     | `int.range(lo, hi, step)` (what `i in lo..hi` enumerates)                 |
| `ip`      | `ip.unspecified(a)`                                                       |
| `str`     | `str.split(s, sep[, limit])`, `str.lower(s)`, `str.upper(s)`, `str.dedent(s)`, `str.trim(s)`, `str.replace(s, from, to)`, `str.starts_with(s, p)`, `str.ends_with(s, p)`, `str.contains(s, n)`, `str.format(fmt, args)`, `str.pad_left(s, w, pad)`, `str.pad_right(s, w, pad)`, `str.len(s)`, `str.slice(s, start[, end])` |
| `list`    | `list.len(l)` (`len` in the prelude), `list.join(l, sep)`, `list.sort(l)`, `list.sort_by(l, field)`, `list.unique(l)`, `list.flatten(l)`, `list.zip(a, b)`, `list.min(l)`, `list.max(l)`, `list.sum(l)`, `list.contains(l, v)`, `list.first(l)`, `list.last(l)` |
| `time`    | `time.parse(s)`, `time.format(t, layout)`, `time.in_zone(t, zone)`, `time.add(t, d)`, `time.until(a, b)`, `time.before(a, b)` |
| `duration`| `duration.parse(s)`, `duration.total(d, unit)`                            |
| `bytes`   | `bytes.to(q, unit)` (`"Mi"`: a whole number of them, else no value)      |
| `cpu`     | `cpu.to(q, unit)` (`"m"` or `""` for cores)                               |
| `random`  | `random.password(key[, length[, alphabet]])`, `random.bytes(key, length)`, `random.signing_key(key)` (secrets); `random.id(key[, length])`, `random.uuid(key)` |
| `regex`   | `regex.match(s, re)`, `regex.capture(s, re, n)`, `regex.replace(s, re, with)` (`re` a `regex`-typed pattern, checked at compile time, R-31) |
| `semver`  | `semver.parse(s)`, `semver.satisfies(v, range)`, `semver.compare(a, b)`   |
| `oci`     | `oci.parse(ref)`, `oci.pinned(ref)`, `oci.with_digest(ref, d)` (the OCI distribution reference grammar, `[registry/]repository[:tag][@digest]`) |
| `hash`    | `hash.sha256(s)`, `hash.short(s, n)`                                      |
| `base64`  | `base64.encode(s)`, `base64.decode(s)`                                    |
| `url`     | `url.parse(s)`, `url.join(base, segment)`, `url.with_scheme(u, s)`, `url.with_host(u, h)`, `url.with_port(u, p)`, `url.with_path(u, p)`, `url.with_query(u, q)`, `url.encode(s)` |
| `path`    | `path.join(a, b, ...)`, `path.dir(p)`, `path.base(p)`, `path.ext(p)`, `path.rel(from, to)`, `path.clean(p)` (POSIX slashes, independent of the host) |
| `json`, `yaml`, `toml` | `.decode(text)`, `.encode(value)`, on a document's text already in hand; the loader (`yaml(path)`, docs/layout.md) stays for reading one |

`random.*` are derived, not drawn: each value is HKDF-SHA256 of the
deployment's master secret (`RANDOM_MASTER` in the environment, else a
key derived from the stack's key file, made on first use) with the
function, the deployment, the key and every knob in the derivation, so a
value is the same on every run, nothing stores it, and changing the
length, the alphabet (`"alnum"`, the default, `"ascii"`, `"hex"`,
`"base64"`), the key or the master is a new value: rotation is a new key
(`"db-pw-2"`) or a new master. `random.password` (32 alphanumerics by
default), `random.bytes` (base64 text) and `random.signing_key` (an
ed25519 key in Synapse's format, `ed25519 a_XXXX SEED`) are declared `->
secret(string)`: a function returning `secret(T)` is a source of the
secrets pass like a secret input. `random.id` (hex) and `random.uuid` are
public, so a name may carry one. A value that must be made once and kept
whatever the master becomes is `memo.first(KEY, random.bytes(KEY, 32))`
("Memo").

A function to `bool` is also a predicate: `inet.contains(n, a)` as a body
literal holds when the call is true. Conversions are constructors named
by their type; strings never coerce silently. Arithmetic (`a + b`) lowers
to the prelude's internal `add`, `sub`, `mul`, `div`, `mod`, and an
interpolation to `format`.

A dotted name's first segment names one thing: a type namespace (`net`), a
provider's externs (`file`), a function package (`inet`), a module, a
component or a copy (`config`, `network`, `blue`), or the root `world`.
Two declarations
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
| `set { k = t } @r where B`                | each entry a `set`: `arg("input", "", "k", t', r) :- B, reads` (`m.k`, a used module's: `arg("input", "m", "k", ..)`) |
| `set from F(S) @r where B`                | `arg("input", S, "p", V, r) :- B, reads, Path = S', table.F.set(Path, At, "p", V)` per input path `p`; a deny for any other ("Giving inputs") |
| `instance c n { k = t } where B`          | `instance c n { k = t' } :- B, reads`; the copy exists while `B` holds |
| `use m { k = t } where B`                 | the same, of the module `m`, under `m`                 |
| `set k = v [@r] where B`                  | `arg("input", "", "k", v, r) :- B` (`k` a leaf's path too; `r` normal by default) |
| `--set k=v`                               | `input("k", v)`, read as `arg("input", S, "k", V, override) :- input("k", V)` (a key's: normal) |
| `set n.k = v [where B]`                   | the copy `n`'s input `k`'s contribution                |
| `deny "m" {o} where B`                    | `deny("m", {o}) :- B` (`warn` the same)                |
| `deny "a ${x}" where B`                   | `deny(M, ..) :- B, M = format("a %s", X)`              |
| `set R.p = t @r where B` (`+=`: `arg_add`) | `arg(T, A, "p", t', r) :- B, reads`                   |
| `set R.l[k].p = t @r where B` (`l` keyed) | `arg(T, A, "l[]", [k', {p: t'}], r) :- B, reads`, the body's reads of `R`'s attribute `attr_base(..)` |
| `set c.p = t where .., c in R.l`          | `set R.l[c].p = t`: the key is `c`'s key fields        |
| `output k: T = t` (`T` a resource type)   | `output k: addr`, and its value                        |
| `output k = t` (no reads)                 | `output k = t'`                                        |
| `output k = t where B` (reads, or a body) | `output(k, t') :- B, reads`                            |
| `output k { f = t }`                      | `output k = { f: t }`, typed by its fields             |
| `output p` (a copy's relation)            | `__rows(Scope, "p", [X, ..]) :- p(X, ..)`, read by `n.p(x, ..)` and `c[t].p(x, ..)` |
| `output p` (a stack's relation)           | `output p = [ [X, ..] \| p(X, ..) ]`, read by `s[k=v].p(x, ..)` as `member(Rows, [x, ..])` |
| `input k { f: T = d }` (R-54)             | the leaf `k.f`'s `arg("input", S, "k.f", d, default)`, its check `k.f`'s refinement |
| `decl p(a: t, b_c: t)`                    | record fields `a`, `b_c`                               |
| `yaml(S)` (a loader, as a value)          | `V`, reading `table.yaml.document(S', At, V)`          |
| `input p from F(S) where B`               | `p(C) :- B, reads, Path = S', table.F.p(Path, At, C)` ("Documents") |
| `input p from t`                          | `p(C) :- reads, Doc = t', table.value.p(Doc, At, C)`   |
| `instance c n { p(t) where B }`           | `n::p(t') :- B, reads`, the copy's relation `p`        |
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
| `x in E` (`E` an enum type)               | `__enum("E", L), member(L, X)`, the fact `__enum("E", [values])` at `E`'s declaration |
| `r in NS` (`NS` a provider's namespace)   | `__namespace("NS", Type), want(Type, R)` (a type test, `r` bound), a fact `__namespace("NS", T)` per type |
| `"n-${e}" in T`                           | `Name = format(..), want(T, Name)`                     |
| `x in world.T`                            | `cloud_exists(T, x)`                                   |
| `x in e`                                  | `member(e', x)`                                        |
| `(k, v) in e`                             | `member(e', K, V)`: an object's keys, a list's indexes |
| `(a, b) = e`                              | `[A, B] = e'`: a list of exactly two                   |
| `{ a, b: p } = e`                         | `O = e', A = __path(O, "a"), P = __path(O, "b")`       |
| `zone({ name })` (columns `name, index`)  | `zone{name: Name}`, a record pattern                   |
| `i in lo..hi`, `i in lo..=hi`             | `member(int.range(lo', hi', 1), i)`, `member(int.range(lo', add(hi', 1), 1), i)` |
| `x not in e`, `not x in T`                | `not member(e', x)`, `not want(T, x)`                  |
| `not { B }` (or a `not` of a nested path) | `not __neg_N(ȳ)`, `__neg_N(ȳ) :- P, B'`: ȳ the variables the body so far binds, `P` its positive literals |
| `a + b` (and `- * / %`)                   | `add(a, b)` (`sub mul div mod`)                        |
| `[t \| B]`                                | a `collect_list` helper rule over `B`                  |
| `p(k, n) where n = count(x), B`           | `p(K, count(X)) :- B` (an aggregate head; "Aggregates") |
| `p(k) where n = count(x), B, n > 2`       | `__agg_N(K, count(X)) :- B`, `p(K) :- __agg_N(K, N), N > 2` |
| `let n = count(x) where B`                | `arg("let", S, "n", count(X), r) :- B`: one group      |

`not R.p` holds when the attribute is absent, false, the API `null`, or
anything but `true`; it does not check that `R` exists (G-13). Write
`R in T` beside it when that matters.

A copy and an import are one mechanism (`modules::expand`): the body
under the scope `n`, its predicates `n::p` (a module's read as `n.p`, a
component's private to the copy, a value leaving it through an output),
an input `k` the cell `n::k(V) :- attr(input, "n", k, V)` with its default
at `@default`, its resources `n::x`, its writes needing no grant (ranks
decide); a top-level input also takes `--set`. A copy inside a copy puts
the outer scope in front (`edge.left::vpc`). `extern p(+a, -b)` is asked
on demand, and `declassify(v, "reason")` lowers a secret's label
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
- the header is `key`, `input`, `input p from`, before the
  body, each statement with the comments directly above it and on its line
  (see "The header").

It drops the commas a newline makes redundant (in blocks and `{ }` bodies)
and the trailing comma of a list or object. A string is printed as
written: the lines inside one that spans lines keep their indentation,
and no line is broken inside a string. A formatted file prints back
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
