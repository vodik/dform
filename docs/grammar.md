# The dform grammar, edition 2026

This is the reference for `src/lexer.rs` and `src/syntax/parser.rs`; keep them
in step. It is E §6 (proposals/E-synthesis.org) with DR-4 and DR-5, plus the
few places noted under "Deviations from E §6" where the example programs of
E §7 or today's evaluator need more. The test suite is the example corpus:
every `.df` file in the repository, `tests/syntax/ok/*.df` (must parse) and
`tests/syntax/err/*.df` (must fail with the diagnostics in the matching
`.txt`).

Parsing never stops at the first error. The parser builds a lossless tree
(rowan): every byte of the file, comments and whitespace included, is in it,
so `dform fmt` can print the file back unchanged. An error abandons the
statement it is in; the rest of that statement up to its terminating `.`, or
up to the `}` that closes the enclosing block, becomes one error node, and
parsing resumes after it. One bad statement is one diagnostic.

## Files

```
file := "edition" INT "." (stmt ".")*
```

Every `.df` file starts with `edition 2026.` (comments may come first). A
file without it is an error that names the pragma. There is no older edition
and no `dform migrate`: the language is pre-release. Text that is not a file
(provider schemas, `dform query` patterns, tests) may leave the pragma out.

## Tokens

```
IDENT    := [a-z][A-Za-z0-9_]*                         ; symbol, predicate, function, name
VAR      := [A-Z][A-Za-z0-9_]* | "_" [A-Za-z0-9_]*       ; "_" alone is the wildcard
QNAME    := IDENT ("." [a-z_][A-Za-z0-9_]*)+             ; one token: no whitespace around "."
FIELD    := [A-Z][A-Za-z0-9_]* ("." [a-z_][A-Za-z0-9_]*)+ ; X.a.b: field access
PATH     := "." SEG ("." SEG | "[" [0-9]+ "]")*          ; keypath literal, one token
SEG      := [a-z_][A-Za-z0-9_]* | STRING
STRING   := "\"" ... "\""          ; escapes \" \\ \n \t \u{hex}
INT      := [0-9]+                 ; -1 is unary minus applied to 1
RANK     := "@default" | "@override"
COMMENT  := ("#" | "//") to end of line
```

Punctuation: `( ) { } [ ] , . : :- = == += != < <= > >= + - * / % |`.

Keywords are token kinds:

```
edition provider stack import input output export contributes module
instance policy apply resource settings scenario extern type decl when not
in exists collect_set collect_list collect_ordered count sum min max
lub_ranked allocate true false null secret persist where moved adopt
lifecycle ignore_changes
```

A keyword is a keyword only where its statement or operator is expected.
Anywhere a plain name is expected it is a name: a keyword followed by `(` is
an atom or a call (`input("env", E)`, `output(k, V)`, `count(X)`,
`lifecycle(T, A, prevent_destroy)`), and a keyword as a key or a symbol is
that word (`min = 1`, `{ type: "INTERNAL" }`, `arg(settings, E, .x, V)`).
`true`, `false` and `null` are always literals.

A keyword inside a QNAME or PATH is just text: `iam.policy`, `.type`.

### Symbol, variable, string

- Uppercase initial or leading `_`: a variable. `_` alone is the wildcard,
  `_Name` a named variable nobody reads.
- Lowercase initial: a symbol (a constant), or a predicate or function name
  when `(` follows. `a.b` is a qualified symbol, one token.
- Anything else is a string: `"us-test-1a"`, `"10.0.0.0/16"`,
  `"projects/x/networks/y"`. `-` and `/` are always operators, so a
  hyphenated or slashed name must be quoted.

Symbols and strings are one value kind to the evaluator: `prod` and
`"prod"` are equal.

### Keypaths

A keypath literal starts with a dot: `.id`, `.tags.team`,
`.gke.static_ips."pngu-grpc".type`, `.containers[0].name`. It is the
attribute path argument of `arg`, `attr`, `setting`, `ref`, `cloud_ref`,
`type_lattice` and the like, and it evaluates to the dotted string (`"id"`,
`"tags.team"`). A quoted segment may hold `-` but not `.`, `[` or `]`.

A `.` directly after `)`, `]`, `}`, a number or a string, with no space, ends
a statement: `p(a).q(b).` is two facts.

### Field access

`X.field` (a variable, a dot, a lowercase name, no spaces) reads a field of
the object bound to `X`; `X.a.b` walks two. It lowers to `__path(X, "a.b")`.

### Addresses

In an address position, `qname "/" IDENT` is an address:
`ref(net.subnet, network.main/private_a, .id)` names resource `private_a`
of module instance `network.main`, and lowers to
`scoped("network.main", "private_a")`. Address positions are the second
argument of `want`, `arg`, `arg_add`, `attr`, `adopt`, `lifecycle`,
`ignore_changes` and `ref`, and the second and third of `moved`. Elsewhere
`a.b / c` is division.

## Statements

```
stmt       := provider | stack | import | input | inputrel | output | export | contributes
            | extern | typedecl | decl
            | module | instance | policy | apply | scenario | when
            | resource | settings
            | rule | fact

provider   := "provider" IDENT block
stack      := "stack" QNAME block
import     := "import" STRING
input      := "input" IDENT ":" type ("=" term)? ("where" body)?
inputrel   := "input" "relation" IDENT "/" INT "from" term   ; term: file(STRING) | git(STRING, STRING, STRING)
output     := "output" IDENT (":" type | "=" term)
export     := "export" IDENT "/" INT
contributes:= "contributes" "arg" "to" (QNAME | IDENT | "_") "at" (PATH | "_")
            | "contributes" IDENT
extern     := "extern" QNAME "(" bindarg ("," bindarg)* ")" "persist"?
bindarg    := ("+" | "-") IDENT (":" type)?
typedecl   := "type" (QNAME | IDENT) attrs
attrs      := "{" (attrdecl ","?)* "}"
attrdecl   := blockpath ":" (attrs | type flag* ("where" body)?)
flag       := "required" | "computed" | "id" | "sensitive" | "nullable"
decl       := "decl" (QNAME | IDENT) "/" INT "mixed"?
            | "decl" (QNAME | IDENT) "(" VAR ":" type ("," VAR ":" type)* ")"
            | "decl" "type" QNAME "open"

module     := "module" IDENT stmts
instance   := "instance" IDENT IDENT block (":-" body)?
policy     := "policy" IDENT stmts
apply      := "apply" IDENT
scenario   := "scenario" IDENT stmts
when       := "when" lit stmts
stmts      := "{" (stmt ".")* "}"

resource   := "resource" (QNAME | IDENT) (IDENT | VAR) RANK? block (":-" body)?
settings   := "settings" (IDENT | VAR) RANK? block (":-" body)?
block      := "{" (assign ","?)* "}"         ; newline or comma between assignments
assign     := blockpath ("=" | "+=") term RANK?
blockpath  := SEG ("." SEG | "[" INT "]")*  ; a keypath without its leading dot

rule       := head ":-" body
fact       := head
head       := atom RANK?                    ; RANK only on arg/4
body       := lit ("," lit)*
lit        := "not" atom
            | "not" "exists" "(" body ")"
            | atom
            | term cmpop term (cmpop term)*  ; a <= b <= c is a <= b, b <= c
            | term "in" term
            | term "not" "in" term
cmpop      := "=" | "==" | "!=" | "<" | "<=" | ">" | ">="
atom       := (IDENT | QNAME) "(" (term ("," term)*)? ")"
            | IDENT "{" (IDENT ":" term ("," IDENT ":" term)*)? "}"
```

Every statement ends with `.`, block statements too (`}.`): the terminator
is what error recovery resynchronises on.

In a literal position `name(...)` is an atom, unless the closing `)` is
followed by a comparison, `in`, `not in` or an arithmetic operator, in which
case it is a function call inside a term: `f(X) = 3`.

### Blocks and commas

In a `{ ... }` block of assignments (resource, settings, instance, provider,
stack) and in a type block, entries are separated by a newline or a comma;
a trailing comma is allowed. In argument lists, lists, objects and records
commas are required between items; lists and objects allow one trailing
comma. `dform fmt` drops a comma that sits at the end of a line in a block
and the trailing comma of a list or object.

## Terms

```
term       := add
add        := mul (("+" | "-") mul)*
mul        := unary (("*" | "/" | "%") unary)*
unary      := "-" unary | primary
primary    := INT | STRING | "true" | "false" | "null"
            | VAR | FIELD | IDENT | QNAME | PATH | ADDR
            | call | list | object | comprehension | "(" term ")"
call       := (IDENT | QNAME) "(" (term ("," term)*)? ")"
list       := "[" (term ("," term)* ","?)? "]"
object     := "{" (objkey ":" term ("," objkey ":" term)* ","?)? "}"
objkey     := IDENT | STRING
comprehension := "[" term "|" body "]"
              |  "[" term "ordered" "by" term "|" body "]"
type       := IDENT | QNAME | IDENT "(" type ("," type)* ")"
            | "{" IDENT ":" type ("," IDENT ":" type)* "}" | STRING
```

Precedence, loosest first: `+ -` (left), `* / %` (left), unary `-`.
Aggregates (`count(X)`, `collect_set(X)`, ...) are calls whose name is a
keyword.

## What lowers to what

Lowering (`src/syntax/lower.rs`, then `src/transform.rs`) turns the tree
into today's AST:

| written                          | lowers to                                        |
|----------------------------------|--------------------------------------------------|
| `module m { ... }`               | a module: its body, once per instance            |
| `instance m i { k = V } :- B`    | `arg(input, "m.i", k, V, normal) :- B` per input |
| `input k: T = D` in module `m`   | `m.i::k(V) :- attr(input, "m.i", k, V)`, and `D` at `@default` |
| `input k: T = D` at the top      | `k(V) :- attr(input, "", k, V)`, `D` at `@default`, and `arg(input, "", k, V, normal) :- input("k", V)` (`--set`) |
| `input k: T where R`             | a deny unless `R` holds of the value (`k` names it in `R`) |
| `output k = t` in module `m`     | `output("m.i", k, t)`; `t` is `scoped("m.i", t)` for `output k: addr` |
| a predicate `p` of module `m`    | `m.i::p` (private), `m.i.p` with `export p/N`, `p` with `contributes p` |
| `policy p { ... }` / `apply p`   | the pack's body once, its predicates `p::q` unless granted |
| `import "f.df"`                  | the file's statements, loaded once               |
| `extern p(+a, -b) persist`       | `p/2` is declared; a body literal of it is asked on demand (`src/externs.rs`) |
| `scenario n { ... }`             | nothing, unless run: `dform test`, `plan --scenario n` add its statements (`src/scenario.rs`) |
| `stack n { ... }`, `provider p { ... }` | no rules: the stack's name, backend, unknowns and role, the mock's schemas (`src/stack.rs`) |
| `input relation p/N from S`      | `decl p/N`, and the facts source `S` holds now, re-read when it changes (`src/watch.rs`) |
| `decl p/N`                       | `p/N` is declared (a provider feeds it)          |
| `decl p/N mixed`                 | nothing: `p/N` may have both ground facts and rules; without it, a predicate that has both (a fact in a `when` block is a rule) is a compile error naming both (E §2.6) |
| `declassify(V, R)` in a rule     | `V`, public to the secret pass, and `declassified("file:line:col", R)` derived from the rule's body (E DR-19) |
| `decl p(A: t, BC: t)`            | record fields `a`, `b_c` for `p{a: .., b_c: ..}` |
| `arg(T, A, P, V) @override`      | `arg(T, A, P, V, override)`                      |
| `constraint("msg") :- B`         | a constraint (a deny checked after evaluation)   |
| `.a.b`, `"s"`, `sym`, `a.b`      | the string value                                 |
| `X.a`                            | `__path(X, "a")`                                 |
| `m.i/r` in an address position   | `scoped("m.i", "r")`                             |
| `m.I` (a variable segment)       | `format("m.%s", I)`: the instance scope, `I` bound |
| `x in L` / `x not in L`          | `member(L, x)` / `not member(L, x)`              |
| `a + b` (and `- * / %`)          | `add(a, b)` (`sub mul div mod`)                  |
| `-t`                             | `sub(0, t)`; `-5` is the integer -5              |
| `[T \| B]`                       | a `collect_list` helper rule over `B`            |

These parse and are rejected with "not yet supported", naming the WORK.org
ticket that gives them meaning: `type` blocks and `decl type ... open`
(phase 6 "Refinement types, doc annotations, L15 inet"). With no ticket yet: the `null` literal,
`not exists(...)`, and the ordered comprehension.

## Deviations from E §6

- `output` declarations, `export` and `contributes` are statements; E §6
  lists them only in the keyword set, E §7.2 and §7.3 write them.
- A type block nests (`gke: { ... }`) and separates entries by comma or
  newline; a type may be an object `{ k: t }` or a string (`enum("X")`), as
  E §7.4 writes.
- Comparisons chain (`1 <= x <= 35`, E §7.1) and `==` is `=`.
- `import "f.df" as alias` still parses, to be reported: the alias is
  gone (E DR-3), and a module is how rules are reused.
- `constraint("msg") :- body` is an ordinary rule head that lowers to
  today's constraint.
- INT has no sign; the sign is unary minus, folded on a literal.
- `.` after a closer ends the statement (see Keypaths), which E leaves
  open.
- A comprehension lowers to today's `collect_list` helper, not E's
  `collect_set`; the attribute's lattice decides whether order and
  duplicates matter.
- `input relation p/N from S` is not in E §6 (DESIGN.org "Reactive inputs
  and controller mode"). `relation` and `from` are contextual words:
  `input relation: T` is still an input named `relation`. The declaration
  belongs at the top of the program; `S` is `file("path")` or
  `git("repo", "ref", "path")`, paths relative to the declaring file.
