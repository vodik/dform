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
file := "edition" INT NL (stmt NL)*
```

Every `.df` file starts with `edition 2026` (comments may come first). A
file without it is an error that names the pragma; any other year is an
error too. Text that is not a file (provider schemas, `dform query`
patterns, tests) may leave the pragma out.

The first token decides what a statement is (H-2): a statement keyword
starts its own statement, and a name followed by `(` is a fact or a rule.
A newline outside every `( )`, `[ ]` and the braces of an object ends a
statement; inside those, newlines are whitespace. Nothing continues a line:
a body of several lines is `where { .. }`, one literal per line, and a long
term wraps inside its brackets. There is no statement terminator: `p(a).`
is an error that says so, and so is `:-`.

Two statements on one line are an error ("expected the end of the line").
A block (`{ }` of a resource, settings, instance, provider or stack) and a
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

Statement keywords, recognised only as the first token of a statement (21):

```
edition  import  provider  stack  type  decl  extern
input  output  let  set  export
module  instance  policy  use  scenario
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
`mixed`, the attribute flags (`required computed id sensitive nullable`).
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
input env: environment = "staging"
```

What a doc comment may document, and the `Kind` and `Name` of its facts:

| statement                                   | Kind        | Name                  |
|---------------------------------------------|-------------|-----------------------|
| `module m`, `policy p`, `scenario s`        | `module`, `policy`, `scenario` | `m`, `p`, `s` |
| `input k`                                   | `input`     | `k`                   |
| `output k`                                  | `output`    | `k`                   |
| `decl p(..)`, `extern p(..)`, `input p(..) from ..` | `predicate` | `p` |
| `p(..) where ..`, a fact, `let k = t`, `deny "m"`, `warn "m"` | `rule` | `p`, `k`, `m` (the message) |
| `type a = T`                                | `alias`     | `a`                   |
| `resource T n`, `resource T "n-${e}"`       | `resource`  | `T["n"]`, `T["n-${e}"]` (as written) |

Inside a module, policy or scenario the name is `BLOCK.NAME`
(`network.vpc_net`). Each pair lowers to a fact of the compiler's own
relation `doc/4`, spanned at the comment: `doc(Kind, Name, Key, Value)`,
so a policy can read and require them (`deny "a module has no owner" {
module: m } where doc("module", m, "description", _), not doc("module", m,
"owner", _)`). The language server shows them on hover, and `dform doc`
renders a project's as Markdown. A doc comment above anything else
documents nothing.

## Names

Case decides nothing; a resolver does (`syntax/resolve.rs`). Resolution is
program-wide: `loader::load_program` parses every file of the program,
collects the declarations of all of them, then lowers, inlining each
`import` where it stands. `parser::parse_file` and `parse_program` lower
one text on its own.

What a name can denote:

| Denotation        | Declared by                                       | Written                          |
|-------------------|---------------------------------------------------|----------------------------------|
| variable          | its occurrences in a rule                         | `x`, `_x`, `_`                   |
| value name        | `input k: T`, a module input, `let k = t`         | `k`                              |
| resource          | a `resource T n {..}` header with a static name   | `n.p` in scope, `T[e].p`         |
| module instance   | `instance m i`                                    | `m.i.k`, `m[e].k`                |
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
| `m[e]`          | instances of module `m` | instance name                     | `output(format("m.%s", e), ..)`      |
| `settings[e]`   | settings rows           | row                               | `setting(e, path, V)`                |
| `world.T[e]`    | live objects of `T`     | the provider's name               | `cloud_attr(T, e, path, V)`          |
| `p[a, b]`       | relation or extern `p`  | every column but the last         | `p(a, b, V)`                         |
| `e[i]`          | a list value            | index (a fresh `i` enumerates)    | `member(e, i, V)`                    |

A name followed by `(` is a relation, a builtin or an extern. Any other
chain `name (.seg | [terms])*` is resolved from its first name, innermost
scope first (rule, then module, pack or scenario, then file, then
program):

1. a typed variable (`x` after `x in T`): a reference;
2. a value name: a read of `k(V)`; a `let` whose value is a reference (a
   settings row, a resource, a live object) reads through it (H-6);
3. `settings[e]`, `world.T[e]`;
4. a resource of that name in scope (a module's own resources, then the
   program's);
5. a module with an instance of that name (`m.i.k`, `m[e].k`);
6. a type `T` followed by `[e]`, an `extern` or a relation followed by
   `[..]`;
7. a variable of the rule;
8. a dotted name in a type namespace: that type's name, a string.

Anything else is `unknown name`, with a hint to quote it.

A bare name (no `.` or `[`) is a variable unless it is a value name. A
variable may not take the name of a resource, a module or a type namespace
in scope ("variable `net` shadows the type namespace `net`"). A bare
resource name is its address in two places: the value of an `output`, and
the left side of `in`, where the type on the right picks among resources
of one name.

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
| `n` (unique in scope) | the declaration of `n`                | `"n"` (scoped by the module)   |
| `T[e]`               | `T` (no declaration needed)            | `e`, relative to the scope     |
| `x` after `x in T`   | `T`                                    | `x`                            |
| `x` after `x in resource` | a fresh type variable             | `x`                            |
| `m.i.k` (`output k: T`, `T` a resource type) | `T`            | the output's value             |
| `k` (`let k = R`, `R` a reference) | `R`'s                    | `R`'s                          |

`T[e]` is relative to the scope (H-10): inside a module it is
`scoped("m.i", e)`; at the top level, in a pack and in CLI arguments it is
the full address, which pastes unchanged from `plan` (H-16):
`net.vpc["network.main::vpc"]`. A resource in scope is written by its
name: `T["n"]` for a resource `n` in scope, and `T.n`, are errors naming
`n`. A name declared twice in scope (three resources named `web`) is an
error listing the candidates by address. A dot on a variable with no
static type is field access on a value: `__path(X, "f")`.

## Statements

```
stmt       := KEYWORD ...                      ; one production per keyword, below
            | NAME ("." NAME)* "(" args ")" RANK? ("where" body)?   ; a fact or a rule

provider   := "provider" NAME block
stack      := "stack" DOTTED ("[" NAME ("," NAME)* "]")? block
import     := "import" STRING
type       := "type" NAME "=" type | "type" DOTTED attrs
decl       := "decl" DOTTED columns "mixed"?
extern     := "extern" DOTTED "(" bindarg ("," bindarg)* ")" "persist"?
bindarg    := ("+" | "-") NAME (":" type)?
input      := "input" NAME ":" type ("=" term)? ("check" body1)?
            | "input" NAME columns "from" term     ; facts(..) | FORMAT(..)
output     := "output" NAME (":" type)? ("=" term)? ("where" body)?
let        := "let" NAME "=" term ("where" body)?
set        := "set" chain ("=" | "+=") term RANK? ("where" body)?
export     := "export" "type" NAME
module     := "module" NAME stmts
instance   := "instance" NAME NAME block ("where" body)?
policy     := "policy" NAME stmts
use        := "use" NAME
scenario   := "scenario" NAME stmts
resource   := "resource" DOTTED hname RANK? block ("where" body)?
settings   := "settings" hname RANK? block ("where" body)?
deny, warn := ("deny" | "warn") STRING object? ("where" body)?
stmts      := "{" (stmt NL)* "}"

attrs      := "{" (attrdecl SEP)* "}"
attrdecl   := blockpath ":" (attrs | type flag* ("check" body1)?)
flag       := "required" | "computed" | "id" | "sensitive" | "nullable"
block      := "{" (entry SEP)* "}"
entry      := blockpath ("=" | "+=") term RANK?
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
instance takes at most one clause, after its block's `}`: `resource T n {
.. } where B`, and a body of several lines is `} where {`, one literal per
line, closed by its own `}`. The clause is a query, and the block is one
resource (or row, or instance) per match. `provider` and `stack` blocks
take no clause. `if`, the clause word of an earlier surface (H-3), is an
error wherever it stands, and the error prints the statement with its
clause spelled `where`.

`set` is the contribution statement (H-5): the chain is a resource's
attribute, a settings row's leaf, or an input (a stack input, or a module
instance's). A `set` with no `where` on a resource, settings row or instance
declared in the same scope is an error that names the block to write the
entry in; a top-level `set` of the program's own input is an error too
(give it a default, or pass `--set`). In a scenario, `set env = "prod"` is
what `--set env=prod` is on the command line.

`let k = t [where B]` is a value (H-6); a `let` may have several rows. When
`t` is a reference (a settings row, a resource, a live object), `k`'s value
is that reference and its static type is the reference's, so a dot on `k`
reads through it: `let cfg = settings[env]`, then `cfg.db.size`.

`output k: T = t [where B]` is one statement (H-7): the type is optional (an
untyped output is `any`), the value is not.

`deny "m" {ctx}? where B` and `warn` are the checks (H-8). The message is a
string like any other: `${e}` reads the body's variables. A deny is checked
after evaluation; no rule may read `deny` or `warn`.

A relation is declared by its columns (H-11): `decl p(a, b)`, a type on a
column optional; `mixed` lets it have both facts and rules. A module's
relations are private to each instance; a value leaves it through an
`output` (DESIGN.org R-5).

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

### Stacks

`stack app { .. }` names the program's stack; its block's settings are
`backend = local("DIR")`, `unknowns = "strict" | "permissive"`,
`role = "bootstrap"` and, on a keyed stack, `isolated = true`.
`stack app[env]` and `stack app[env, region]` key it: each name in the
brackets is one of the program's own inputs (a top-level `input`, not a
secret; an error at the name otherwise), and each value of the key is a
deployment of its own (`app[env=prod]`), with its own state. The key
lowers to nothing: it is read by `stack::config` and decides where a run's
state lives.

A stack's `config = FORMAT(SOURCE)` is not a constant: it is a table of
the deployment's settings (see "Tables"), and needs a key.

### Provider blocks

`provider NAME { .. }`'s `source` is a constant (the stack reads it to
start the provider). Every other setting is a term, read like a rule's
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
provider file {}      file.json(+path, -value: any), file.text(+path, -value: string)
provider env {}       env.var(+name, -value: secret(string))
provider random {}    random.password(+key, -value: secret(string)) persist
```

`extern file.json(..)` in a program is an error naming the `provider`
statement to write instead. `env.var(t)` as a term is the lookup
`env.var[t]`; without `provider env {}` it is an error that says to declare
it. `extern` stays the schema's word: provider schemas and the compiler's
tests declare externs with it, and so, until the compiler reads a
provider's schema (DESIGN.org R-24), does a program for a provider that is
not built in.

### Type aliases

`type NAME = TYPE` names a type: `type environment = enum("dev", "stg",
"prod")`, then `input env: environment` and `input peering(env:
environment, ...) from csv(..)`. An alias is usable anywhere a type is (an
input, a module input, a table's column, an output, an extern's column, a
`decl` column, a `type` block's attribute) and is transparent: the
resolver writes its type in its place, so nothing after it sees an alias.
An alias may name other aliases (`type envs = list(environment)`); one
that reaches itself is an error naming the cycle, each alias in it
labelled. A member of `enum(..)` is a value, never an alias. An alias may
not take a built-in type's name (`int`, `string`, `bool`, `inet`,
`symbol`, `addr`, `any`, `enum`, `list`, `set`, `secret`, `ref`).

Where an alias is in scope: in its file, and in every file that imports
that file, however indirectly; an alias in a module, policy or scenario is
that block's, until the module says `export type NAME`, which puts it in
its file's scope too. `export type` outside a module is an error, and so
is exporting a name the module does not declare. Two aliases of one name
in one scope are an error listing both. A file reached by two imports is
one file: its alias is one alias.

### Relation inputs and tables

`input p(a, b) from facts(SOURCE)` is a relation the world gives: its facts
are a `.df` fact file's (`facts("data/release.facts")`, or
`facts(git(REPO, REF, PATH))`), read from outside and re-read when they
change. `input p(col: type, ...) from FORMAT(SOURCE)` is a table: its rows
are a data file's. FORMAT is `csv`, `json`, `yaml` or `toml`; SOURCE is a
term for the path (a string, holes allowed: a hole is a content position,
so it reads now) or `git(REPO, REF, PATH)`, each a term. A table's columns
are typed, with an input's types (`inputs::check_type`), never `secret`.
Relation inputs are at the top of the program.

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

A stack's `config = FORMAT(SOURCE)` is the table `stack.config(path: string,
value: any)`, read into `arg("settings", Row, Path, Value, normal)`, `Row`
the key's value (several keys': `format("%s/%s", ..)`). `transform` expands
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
has no finite set of values); `resource T _` and `instance m _` name
nothing; `set T[_].p = t` is `set r.p = t where r in T`, and the error prints
it. A name that starts with `_` (`_x`) is an ordinary name.

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
  item. `vpc_id = vpc.id` is `ref(net.vpc, "vpc", "id")`: an apply-order
  edge, and a null until a computed path resolves.
- **a read** anywhere its content is needed: a body literal, a clause, an
  argument of a builtin or operator, an interpolation hole, an index.
  `inet.subnet(vpc.cidr, 4, i)` reads `attr(net.vpc, "vpc", "cidr", V)`
  now. To read into a field, bind in the clause: `namespace = ns` in the
  block and `where ns = web.name` after it.

`settings[e].p`, an instance output `m.i.k`, a value name, `p[..]` and
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
provider's externs (`file`), a function package (`inet`), a module
(`network`), or a root (`settings`, `world`, `stacks`). Two declarations
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
| `let k = t [where B]`                     | `k(t') :- B, reads`; with neither, the fact `k(t')`   |
| `let k = R` (`R` a reference)             | `k(A) :- reads` for `R`'s key; `k.p` reads through it  |
| `type a = T`, `export type a`             | nothing: each use of `a` is `T`                        |
| `#\| k: v` above an item (Doc comments)  | `doc(Kind, Name, "k", "v")`                            |
| `provider p { k = t, expect_account = a }` | `provider_config("p", {k: t'}) :- reads`, `provider_expect_account("p", a') :- reads` ("Provider blocks") |
| `env.var(t)`                              | `V`, reading `env.var(t', V)`                          |
| `resource T n { f = t } where B`          | `resource T n { f = t' } :- B, reads`                  |
| `resource T "a-${e}" { .. }`              | name `Addr`, `Addr = format("a-%s", e')` last          |
| `settings n @r { .. } where B`            | `settings n @r { .. } :- B, reads`                     |
| `instance m i { k = t } where B`          | `instance m i { k = t' } :- B, reads`                  |
| `set k = v` (in a scenario)               | `input("k", v)`                                        |
| `set m.i.k = v [where B]`                 | the instance input `k`'s contribution                  |
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
| `stack s[k] { config = F(S) }`            | `arg("settings", K, P, V, normal) :- .., table.F.stack.config(.., P, V)` |
| `enum("a", "b")` in a type                | `enum(a, b)`                                           |
| `"a${e}b"`                                | `format("a%sb", e')`                                   |
| `k` (value name)                          | `V`, reading `k(V)`                                    |
| `x.f.g` (a value)                         | `__path(X, "f.g")`                                     |
| `e[i]` (a list value)                     | `V`, reading `member(e', i, V)`                        |
| `pattern = e[i]`                          | `member(e', i, pattern)`                               |
| `p[a, b]`, `ext[a]`                       | `V`, reading `p(a', b', V)`, `ext(a', V)`              |
| `R.p` (whole value)                       | `ref(T, A, "p")`                                       |
| `R.p.q` (content)                         | `V`, reading `attr(T, A, "p", V)`; `__path(V, "q")`    |
| `settings[e].a.b`                         | `V`, reading `setting(e', "a.b", V)`                   |
| `m.i.k`, `m[e].k`                         | `V`, reading `output("m.i", "k", V)`, `output(format("m.%s", e'), "k", V)` |
| `world.T[e].a.b`                          | `V`, reading `cloud_attr("T", e', "a.b", V)`           |
| `x = R.p`, `R.p == c`                     | `attr(T, A, "p", x)`, `attr(T, A, "p", c)`: the read itself |
| `R.p` alone, `not R.p`                    | `attr(T, A, "p", true)`, `not attr(T, A, "p", true)`  |
| `not R.p == c`                            | `not attr(T, A, "p", c)`                               |
| `has R.p`, `not has R.p`                  | `attr(T, A, "p", _)`, `not attr(T, A, "p", _)`        |
| `has x.f`, `has R.p.q` (a walk)           | `Has = __path(X, "f")` after the read; `not` of it through a helper |
| `k == c`, `k`, `has k`                    | `k(c)`, `k(true)`, `k(_)`                              |
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

The module, pack and scenario constructs keep their meaning: a module's
predicates are private per instance (a value leaves through an output), an input
`k` of module `m` is `m.i::k(V) :- attr(input, "m.i", k, V)` with its
default at `@default`, a top-level input also takes `--set`, a pack's body
is lowered once, its predicates are private and its writes need no
grant (ranks decide), `use p` applies pack `p`, `import "f.df"` inlines the file once, `extern p(+a, -b)
persist` is asked on demand, and `declassify(v, "reason")` lowers a
secret's label (E DR-19).

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
- `{ a: a }` is `{ a }`;
- a header name is bare when it is a name, not a keyword, and not bound by
  the clause; else it is quoted;
- `not { lit }` of one literal whose names are all bound is `not lit`;
- `=` between two bound sides is `==`;
- `i = p[k]` with `i` fresh is `p(k, i)`;
- `env("prod")` for a value name is `env == "prod"`.

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
- A `let` may have several rows; nothing checks it is functional.
- An `output` with a body, or whose value reads, is the rule
  `output(k, t') :- B, reads`, in a module too.
- A library (a module or pack file another file imports) reads the
  program's inputs by name, so it lowers only through the programs that
  import it.
