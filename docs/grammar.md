# The dform grammar, edition 2026

This is the reference for `src/lexer.rs`, `src/syntax/parser.rs` (the
lossless tree) and `src/syntax/resolve.rs` (names, and the lowering to the
AST); keep them in step. It is the surface of proposal G
(`proposals/G-surface-syntax.org`) over the core of E as revised by F; the
places this implementation settles what G left open are under "Decisions
the proposal left open". The test suite is the example corpus: every `.df`
file in the repository, `tests/syntax/ok/*.df` (must parse and lower) and
`tests/syntax/err/*.df` (must fail with the diagnostics in the matching
`.txt`).

Parsing never stops at the first error. The parser builds a lossless tree
(rowan): every byte of the file, comments and whitespace included, is in it,
so `dform fmt` can print the file back unchanged. An error abandons the
statement it is in: the rest of it, up to the next line outside the
brackets the statement opened, up to the `}` that closes the enclosing
block, or up to a line that starts a statement in column 0, becomes one
error node, and parsing resumes after it. One bad statement is one
diagnostic.

## Files and lines

```
file := "edition" INT NL (stmt NL)*
```

Every `.df` file starts with `edition 2026` (comments may come first). A
file without it is an error that names the pragma. Text that is not a file
(provider schemas, `dform query` patterns, tests) may leave the pragma out.

A statement ends at a newline that is outside every `( )`, `[ ]` and the
braces of an object, record or argument; inside those, newlines are
whitespace. A line that ends with `,`, `if`, `for`, `in`, `=`, a comparison
or an arithmetic operator continues on the next. There is no statement
terminator: `p(a).` is an error that says so, and so is `:-`.

Two statements on one line are an error ("expected the end of the line").
A block (`{ }` of a resource, settings, instance, provider or stack) and a
body block (`if { }`) separate their entries by a newline or a comma.

## Tokens

```
IDENT    := [A-Za-z_][A-Za-z0-9_]*       ; case decides nothing; "_" alone is the wildcard
PATH     := "." SEG ("." SEG | "[" [0-9]+ "]")*   ; a keypath literal
SEG      := IDENT | STRING
DOT      := "."                          ; glued to a name, `)`, `]` or a string: member access
STRING   := "\"" ... "\""                ; escapes \" \\ \n \t \u{hex}; {e} interpolates
INT      := [0-9]+                       ; -1 is unary minus applied to 1
RANK     := "@default" | "@override"
COMMENT  := ("#" | "//") to end of line
```

Punctuation: `( ) { } [ ] , . : = == += != < <= > >= + - * / % |`.

A `.` directly after a name, a keyword, `)`, `]` or a string, with no
space, is member access (`vpc.cidr`, `f[0].x`, `."a-b".c`); anywhere else
it starts a keypath literal (`attr(t, a, .cidr, v)`). `/` glued on both
sides to names inside a chain is the instance separator (`m.i/n`);
anywhere else it is division, so write division with spaces. `-` is always
an operator: a hyphenated name is a string, and the parser says so.

Keywords are token kinds:

```
edition provider stack import input output export contributes module
instance policy apply resource settings scenario extern type decl when
not in exists true false null persist where if for let has some with
deny warn constraint
```

A keyword is a keyword only where its construct is expected. Anywhere a
plain name is expected (a key, a path segment, a declared name) it is a
name, and a keyword followed by `(` is an atom or a call (`input("env", v)`,
`deny("m") if ...`). `true`, `false` and `null` are always literals.
`relation`, `from`, `mixed`, `open`, `ordered`, `by`, `as`, `world` are
contextual words.

### Strings and interpolation

A string is a string constant: every constant is quoted (`"prod"`,
`"set"`, `"audit.write"`). Symbols and strings were always one value kind
to the evaluator; quoting changes no value.

`"a{e}b"` interpolates the term `e`: it lowers to `format("a%sb", e)`. `{{`
and `}}` are literal braces; a lone `}` is an error. A hole may not hold a
string (bind it first). A hole is a content position: a dot in it reads
now (see "Reference or read"). A literal part may not contain `%s`.

## Names

Case decides nothing; a resolver does (`src/syntax/resolve.rs`, G section
3). Resolution is program-wide: `loader::load_program` parses every file
of the program, collects the declarations of all of them, then lowers,
inlining each `import` where it stands. `parser::parse_file` and
`parse_program` lower one text on its own.

What a name can denote:

| Denotation        | Declared by                                       | Written                          |
|-------------------|---------------------------------------------------|----------------------------------|
| variable          | its occurrences in a rule                         | `x`, `_x`, `_`                   |
| value name        | `input k: T`, a module input, a value rule `k = t` | `k`                             |
| `let` alias       | `let a = CHAIN`                                   | `a`                              |
| resource          | a `resource T n {..}` header with a static name   | `n.p`, `T.n.p`, `T[e].p`, `m.i/n.p` |
| module instance   | `instance m i`                                    | `m.i.k`, `m[e].k`, `m.i`         |
| settings row      | `settings n {..}`                                 | `settings.n.p`, `settings[e].p`  |
| live object       | the provider's inventory                          | `world.T[e].p`                   |
| type              | see "Types"                                       | `T` (dotted or not)              |
| relation, builtin | its rules or facts, `decl`, `extern`; a builtin   | `p(..)`, `p[..]`                 |

A name followed by `(` is a relation, a builtin or an extern. Any other
chain `name (.seg | [terms] | /name)*` is resolved from its first name, in
this order, innermost scope first (rule, then module, pack or scenario,
then file, then program):

1. a `let` alias (text substitution: the alias's chain, then the rest);
2. a value name: a read of `k(V)`;
3. `settings`, `world`;
4. a resource of that name in scope (a module's own resources, then the
   program's);
5. a module with an instance of that name (`m.i`, `m[e]`), or `a.b/c`;
6. a type `T` followed by `.n` for a resource `n` declared with type `T`
   (the longest such `T`), or by `[e]`; an `extern` or a relation followed
   by `[..]`;
7. a variable of the rule;
8. a dotted name in a type namespace: that type's name, a string.

Anything else is `unknown name`, with a hint to quote it.

A bare name (no `.`, `[` or `/`) is a variable unless it is a `let` or a
value name. A variable may not take the name of a resource, a module or a
type namespace in scope ("variable `net` shadows the type namespace
`net`"): resources, modules and types are reached only through `.`, `[` or
`/`. A bare resource name is its address in two places: the value of an
`output`, and the left side of `in` or `exists`.

A variable must have a binding occurrence somewhere in its rule: an
argument of a relation (a pattern in it included), either side of `=`, the
left of `in`, a `some` binder, a record field, an index of a read, or an
enclosing `for`/`when` or block clause. A name with none was meant as a
string: `env == prod` is `unknown name prod`. `==`, `!=` and the orders
test; they do not bind.

### Types

Types are names of the core (`net.vpc`, `gke_cluster`, `k8s.deployment`).
The known types are every resource header's type, every `type` block's, the
first argument of a `type_*` fact (a schema's `type_provider`, `type_attr`
rows), and the types the built-in provider schemas declare. A dotted name
whose first segment is one of their first segments (a type namespace) and
which names nothing else is a type: its value is its text. `x in T`
prefers the type (`x in db.postgres` in module `database`, which has a
resource `db`).

`query` and `why` patterns are read without the program's declarations:
there, any dotted name that names nothing else is a type.

### References and their type

A reference is a pair (type, address). A dot on a reference needs its type
statically:

| Written              | Type                                   | Address                        |
|----------------------|----------------------------------------|--------------------------------|
| `n` (unique in scope) | the declaration of `n`                | `"n"` (scoped by the module)   |
| `T.n`                | `T`                                    | `"n"`                          |
| `T[e]`               | `T` (no declaration needed)            | `e`                            |
| `m.i/n`              | module `m`'s declaration of `n`        | `scoped("m.i", "n")`           |
| `x` after `x in T`   | `T`                                    | `x`                            |
| `x` after `x in resource` | a fresh type variable             | `x`                            |
| `m.i.k` (`output k: T`, `T` a resource type) | `T`            | the output's value             |

A name declared twice in scope (three resources named `web`) is an error
listing the candidates; `T.n` disambiguates. A dot on a variable with no
static type is field access on a value: `__path(X, "f")`.

## Statements

```
stmt       := provider | stack | import | input | inputrel | output | export
            | contributes | extern | typedecl | decl | let
            | module | instance | policy | apply | scenario | when | for | with
            | resource | settings
            | check | contribution | valuerule | rule | fact

provider   := "provider" NAME block
stack      := "stack" DOTTED ("[" NAME ("," NAME)* "]")? block  ; `[` glued to the name
import     := "import" STRING
input      := "input" NAME ":" type ("=" term)? ("where" body1)?
inputrel   := "input" "relation" NAME "/" INT "from" term   ; file(STRING) | git(STRING, STRING, STRING)
            | "input" "relation" NAME columns "from" term  ; FORMAT(PATH) | FORMAT(git(REPO, REF, PATH))
columns    := "(" NAME ":" type ("," NAME ":" type)* ","? ")"
output     := "output" NAME (":" type | "=" term)           ; type may be a resource type
export     := "export" NAME "/" INT
contributes:= "contributes" chain       ; `p`, `_.path`, `settings.path`, `TYPE.path`
extern     := "extern" DOTTED "(" bindarg ("," bindarg)* ")" "persist"?
bindarg    := ("+" | "-") NAME (":" type)?
typedecl   := "type" DOTTED attrs
attrs      := "{" (attrdecl SEP)* "}"
attrdecl   := blockpath ":" (attrs | type flag* ("where" body1)?)
flag       := "required" | "computed" | "id" | "sensitive" | "nullable"
decl       := "decl" DOTTED "/" INT "mixed"?
            | "decl" DOTTED columns
            | "decl" "type" DOTTED "open"
let        := "let" NAME "=" chain

module     := "module" NAME stmts
instance   := "instance" NAME NAME block
policy     := "policy" NAME stmts
apply      := "apply" NAME
scenario   := "scenario" NAME stmts
when       := "when" body1 stmts
for        := "for" body1 stmts
with       := "with" NAME "=" term                           ; in a scenario
stmts      := "{" (stmt NL)* "}"

resource   := "resource" DOTTED hname RANK? block
settings   := "settings" hname RANK? block
hname      := NAME | STRING                                  ; see "Block names"
block      := "{" (clause SEP)* (assign SEP)* "}"
clause     := ("for" | "if") body1
assign     := blockpath ("=" | "+=") term RANK?
blockpath  := SEG ("." SEG | "[" INT "]")*                  ; a keypath without its dot

check      := ("deny" | "warn" | "constraint") STRING object? ("if" body)?
contribution := chain ("=" | "+=") term RANK? ("if" body)?  ; chain: a reference and a path
valuerule  := NAME "=" term ("if" body)?
rule       := head "if" body
fact       := head
head       := atom RANK?                                     ; RANK only on arg/4
body       := body1 | "{" lit (SEP lit)* "}"
body1      := lit ("," lit)*
SEP        := "," | NL
DOTTED     := NAME ("." NAME)*                               ; no spaces
```

`if` and the clause keywords are on the line of what they guard; a body
of several lines is a `{ }` block or ends each line with `,`.

A block states its `for` and `if` clauses first, before any field; both
append to the block's one body (`for` binds, `if` guards; the split is for
the reader). `provider` and `stack` blocks take no clause.

### Stacks

`stack app { .. }` names the program's stack; its block's settings are
`backend = local("DIR")`, `unknowns = "strict" | "permissive"`,
`role = "bootstrap"` and, on a keyed stack, `isolated = true`.
`stack app[env]` and `stack app[env, region]` key it: each name in the
brackets is one of the program's own inputs (a top-level `input`, not a
secret; an error at the name otherwise), and each value of the key is a
deployment of its own (`app[env=prod]`), with its own state. The `[` is
glued to the stack's name, like an index; inside the brackets, spaces and
the comma are free, and `fmt` prints `stack app[env, region]`. The key
lowers to nothing: it is read by `stack::config` and decides where a run's
state lives. For tree-sitter: the stack statement takes an optional key
after its name, `"[" NAME ("," NAME)* "]"`, before the block.

A stack's `config = FORMAT(SOURCE)` is not a constant: it is a table of
the deployment's settings (see "Tables"), and needs a key.

### Tables

`input relation p(col: type, ...) from FORMAT(SOURCE)` is a table: its
rows are a data file's. FORMAT is `csv`, `json`, `yaml` or `toml`; SOURCE
is a term for the path (a string, holes allowed: a hole is a content
position, so it reads now) or `git(REPO, REF, PATH)`, each a term. A column
type is an input's type (`inputs::check_type`), never `secret`. Like `p/N`,
a table is at the top of the program. `fmt` glues the `(` to the name.
`p/N` and `p(` share the statement: after the name, `/` is a fact file's
relation and `(` a table's columns; anything else is "expected `/` or
`(`". For tree-sitter: the name is followed by `"/" INT` or by `"("
field_declaration ("," field_declaration)* ","? ")"`.

It lowers to externs (`src/tables.rs`), the source its bound inputs:

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
that rule into one per settings path the program writes or reads (a
variable path would make every settings cell one partition), and a deny for
a leaf at any other path.

### Block names

A resource's or settings row's header name is a string or a name. A string
with holes (`"private-{z}"`) is the variable `Addr`, bound last in the body
by `format`. A name the block's clauses bind is that variable
(`resource net.vpc t { for tenant(t, i) ... }`); any other name is the
static name (`resource net.vpc shared`).

## Literals and terms

```
lit        := "not" lit1 | "not" "{" body "}" | lit1
lit1       := atom
            | "exists" chain                     ; the resource exists (want)
            | "has" chain                        ; the attribute has a value
            | chain                              ; a truth test: == true
            | term cmpop term (cmpop term)*      ; a <= b <= c is a <= b, b <= c
            | "some" term ("," term)? "in" term
            | term "in" ("resource" | term)
            | term "not" "in" term
cmpop      := "=" | "==" | "!=" | "<" | "<=" | ">" | ">="
atom       := chain "(" (term ("," term)*)? ")"
            | NAME "{" (NAME ":" term ("," NAME ":" term)*)? "}"   ; a record, glued

term       := add
add        := mul (("+" | "-") mul)*
mul        := unary (("*" | "/" | "%") unary)*
unary      := "-" unary | primary
primary    := INT | STRING | "true" | "false" | "null" | PATH
            | chain | call | list | object | comprehension | "(" term ")"
chain      := NAME ("." SEG | "[" term ("," term)* "]" | "/" NAME)*   ; each part glued
call       := chain "(" (term ("," term)*)? ")"
list       := "[" (term ("," term)* ","?)? "]"
object     := "{" (key (":" term)? ("," key (":" term)?)* ","?)? "}"  ; `{ a }` is `{ a: a }`
key        := NAME | STRING
comprehension := "[" term "|" body1 "]"
              |  "[" term "ordered" "by" term "|" body1 "]"
type       := DOTTED ("(" type ("," type)* ")")? | "{" NAME ":" type ("," NAME ":" type)* "}" | STRING
```

In a literal position a chain applied to arguments is an atom, unless an
operator follows it (`f(x) == 3` compares a call). Precedence, loosest
first: `+ -` (left), `* / %` (left), unary `-`. Aggregates (`count(x)`,
`collect_set(x)`, ...) are calls in a head.

### Reference or read

A dot on a reference means one of two things, decided by position (G-6):

- **a reference** where it is a whole value: a field's value, a head or
  output argument, an element of a list or object there, a comprehension's
  item. `vpc_id = vpc.id` is `ref(net.vpc, "vpc", "id")`: an apply-order
  edge, and a null until a computed path resolves.
- **a read** anywhere its content is needed: a body literal, a clause, an
  argument of a builtin or operator, an interpolation hole, an index.
  `inet_subnet(vpc.cidr, 4, i)` reads `attr(net.vpc, "vpc", "cidr", V)`
  now. To read into a field, bind in a clause: `if ns = web.name` then
  `namespace = ns`.

`settings.n.p`, an instance output `m.i.k`, a value name, `p[..]` and
`world.T[e].p` are always reads.

### Where reads go

A read is hoisted: in a rule body just before the literal that holds it
(positive, even under `not`); from a head, a field, an instance input or a
comprehension item, appended to the body, after the clauses, in source
order of first use. A block has one shared body: a read in any field gates
the whole block (the want and every field), as before. Each distinct read
is made once per rule; a value name is read once per rule.

The variables a read binds are named after what they read (`vpc.cidr` is
`Cidr`, `env` is `Env`, `zone_index[z]` is `ZoneIndex`); a source variable
is its name capitalised (`vpc_net` is `VpcNet`, `_c` is `_C`), which is how
`strata`, `why` and diagnostics print it.

## What lowers to what

Lowering produces today's AST; `src/transform.rs` and everything after it
are unchanged.

| written                                   | lowers to                                              |
|-------------------------------------------|--------------------------------------------------------|
| `head if body`                            | `head :- body`                                         |
| `p(t)` with reads in `t`                  | `p(t') :- reads` (a rule)                              |
| `k = t [if B]`                            | `k(t') :- B, reads`; with neither, the fact `k(t')`   |
| `let a = CHAIN`                           | nothing: each `a` is `CHAIN`                           |
| `resource T n { for B1 if B2 f = t }`     | `resource T n { f = t' } :- B1, B2, reads`             |
| `resource T "a-{e}" { .. }`               | name `Addr`, `Addr = format("a-%s", e')` last          |
| `settings n @r { for B .. }`              | `settings n @r { .. } :- B, reads`                     |
| `instance m i { if B k = t }`             | `instance m i { k = t' } :- B, reads`                  |
| `when B { S }`, `for B { S }`             | one nested `when` per literal of `B`                   |
| `with k = v` (in a scenario)              | `input("k", v)`                                        |
| `deny "m" {o} if B`                       | `deny("m", {o}) :- B` (`warn` the same)                |
| `constraint "m" if B`                     | a constraint                                           |
| `R.p = t @r if B` (`+=`: `arg_add`)       | `arg(T, A, "p", t', r) :- B, reads`                    |
| `settings.n.p = t`                        | `arg("settings", "n", "p", t')`                        |
| `contributes _.p`, `contributes T.p`      | a grant of `.p` on any type, on `T`                    |
| `output k: T` (`T` a resource type)       | `output k: addr`                                       |
| `output k = t` (no reads)                 | `output k = t'`                                        |
| `output k = t` (with reads)               | `output(k, t') :- reads`                               |
| `decl p(a: t, b_c: t)`                    | record fields `a`, `b_c`                               |
| `input relation p(c: t) from F(S)`        | `p(C) :- reads, Path = S', table.F.p(Path, At, C)` ("Tables") |
| `stack s[k] { config = F(S) }`            | `arg("settings", K, P, V, normal) :- .., table.F.stack.config(.., P, V)` |
| `enum("a", "b")` in a type                | `enum(a, b)`                                           |
| `"a{e}b"`                                 | `format("a%sb", e')`                                   |
| `k` (value name)                          | `V`, reading `k(V)`                                    |
| `x.f.g` (a value)                         | `__path(X, "f.g")`                                     |
| `e[i]` (a list value)                     | `V`, reading `member(e', i, V)`                        |
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
| `x in T`, `x in resource`, `exists R`     | `want(T, x)`, `want(Type, x)`, `want(T, A)`            |
| `"n-{e}" in T`                            | `Name = format(..), want(T, Name)`                     |
| `x in world.T`                            | `cloud_exists(T, x)`                                   |
| `x in e`, `some i, x in e`                | `member(e', x)`, `member(e', i, x)`                    |
| `x not in e`, `not x in T`                | `not member(e', x)`, `not want(T, x)`                  |
| `not { B }` (or a `not` of a nested path) | `not __neg_N(ȳ)`, `__neg_N(ȳ) :- P, B'`: ȳ the variables the body so far binds, `P` its positive literals |
| `a + b` (and `- * / %`)                   | `add(a, b)` (`sub mul div mod`)                        |
| `[t \| B]`                                | a `collect_list` helper rule over `B`                  |

`not R.p` holds when the attribute is absent, false, the API `null`, or
anything but `true`; it does not check that `R` exists (G-13). Write
`exists R` beside it when that matters.

The module, pack and scenario constructs keep their meaning: a module's
predicates are private per instance unless exported or granted, an input
`k` of module `m` is `m.i::k(V) :- attr(input, "m.i", k, V)` with its
default at `@default`, a top-level input also takes `--set`, a pack's body
is lowered once and its predicates are private unless granted,
`import "f.df"` inlines the file once, `extern p(+a, -b) persist` is asked
on demand, `input relation p/N from S` is read from outside and re-read
when it changes, `input relation p(c: t) from F(S)` is a table (see
"Tables"), `decl p/N mixed` lets `p/N` have both facts and rules, and
`declassify(v, "reason")` lowers a secret's label (E DR-19).

A refinement (`where`) names the attribute or input by its own name, as
text (G-24): `input replicas: int = 2 where replicas >= 1`,
`db.backup_days: int where 1 <= db.backup_days <= 35`.

These parse and are rejected with "not yet supported": `decl type ...
open`, a flag on a `type` block attribute and a `type` block anywhere but
the top of the program (phase 6 "Refinement types, doc annotations, L15
inet"); the `null` literal and the ordered comprehension (no ticket yet).

## Formatting

`dform fmt` keeps the author's line breaks (at most one blank line in a
row) and sets the spaces within a line and the indentation: a line is one
step deeper than the line that holds the innermost bracket, statement,
block entry or clause still open at its first token, and a line that
starts with a closer sits with the line that opened it. It drops the
commas a newline makes redundant (in blocks and `{ }` bodies) and the
trailing comma of a list or object; the commas that continue a one-line
body onto the next line stay. A formatted file prints back byte for byte.

## Decisions the proposal left open

- The edition is not bumped (the user's decision 10): the grammar changed
  in place under `edition 2026`, and the Prolog-shaped one is gone.
- A read in any field gates the whole block, as F10 has it (decision 9).
- Type namespaces are known from headers, `type` blocks, `type_*` facts
  and the built-in provider schemas (G section 10 asks for "the provider
  schemas' `type_provider` rows"; the resolver runs before providers are
  chosen, so it uses the built-in ones). An unknown dotted name is an
  error, not a string, except in `query` and `why` patterns.
- `=` binds either side; `==` binds neither (G-28 is about how `fmt` prints
  them; here it decides which one may introduce a variable).
- `a.b/c` with no path is the address `scoped("a.b", "c")` even when
  module `a` is not in view (a file lowered on its own).
- A value rule may have several rows; nothing checks it is functional.
- `output k = t` whose value reads is the rule `output(k, t') :- reads`
  in a module too.
- A library (a module or pack file another file imports) reads the
  program's inputs by name, so it lowers only through the programs that
  import it.
