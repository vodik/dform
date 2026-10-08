//! The language's reference, one entry per name a program may write: each
//! function (from `std/*.df`), aggregate, builtin extern and keyword, its
//! signature, a summary and an example. The language server's hover,
//! completion detail and signature help read it.

/// What a [`Reference`] entry documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A function a program may call, declared in `std/*.df` (`functions`).
    Function,
    /// An aggregate, bound in a body: `n = count(x)` (`partition::AGGREGATES`).
    Aggregate,
    /// A built-in provider's extern (`env.var`).
    Extern,
    /// A keyword (`lexer::KEYWORDS`).
    Keyword,
}

/// One entry of the language's reference: a function, an aggregate, a
/// builtin extern or a keyword. The language server's hover, completion
/// detail and signature help read it ([`references`]); a builtin without
/// an entry fails `tests::every_builtin_and_keyword_has_a_reference``.
#[derive(Debug, Clone, Copy)]
pub struct Reference {
    pub name: &'static str,
    pub kind: RefKind,
    /// A call's `name(param: type, ...) -> type` (signature help splits
    /// its parameters at the top-level commas), or a keyword's syntax.
    pub signature: &'static str,
    pub summary: &'static str,
    pub example: &'static str,
}

const fn r(
    name: &'static str,
    kind: RefKind,
    signature: &'static str,
    summary: &'static str,
    example: &'static str,
) -> Reference {
    Reference {
        name,
        kind,
        signature,
        summary,
        example,
    }
}

use RefKind::{Aggregate, Extern as Ext, Keyword as Kw};

/// The reference beside the functions (which `std/*.df` documents): the
/// aggregates and builtin externs, then the keywords.
const REFERENCE: &[Reference] = &[
    r(
        "collect_set",
        Aggregate,
        "collect_set(x: any) -> set",
        "The set of every `x` the body binds, per group of the head's other variables.",
        "ids(l) where l = collect_set(s.id), s in net.subnet",
    ),
    r(
        "collect_list",
        Aggregate,
        "collect_list(x: any) -> list",
        "The list of every `x` the body binds per group, in the order of the body's rows; a comprehension lowers to it.",
        "names(l) where l = collect_list(n), host(n)",
    ),
    r(
        "count",
        Aggregate,
        "count(x: any) -> int",
        "The number of the body's matches per group of the head's other variables.",
        "subnets(v, n) where n = count(s), s in net.subnet, s.vpc_id == v",
    ),
    r(
        "sum",
        Aggregate,
        "sum(x: int) -> int",
        "The sum of `x` over every match of the body per group; a non-int is a deny.",
        "total(n) where n = sum(x), size(_, x)",
    ),
    r(
        "min",
        Aggregate,
        "min(x: int | string) -> int | string",
        "The least `x` per group, ints or strings (a mix is a deny).",
        "first(n) where n = min(x), size(_, x)",
    ),
    r(
        "max",
        Aggregate,
        "max(x: int | string) -> int | string",
        "The greatest `x` per group, ints or strings (a mix is a deny).",
        "last(n) where n = max(x), size(_, x)",
    ),
    r(
        "any",
        Aggregate,
        "any(x: bool) -> bool",
        "Whether `x` is true for some match of the body per group (a non-bool is a deny).",
        "exposed(v, p) where p = any(public), subnet(v, public)",
    ),
    r(
        "all",
        Aggregate,
        "all(x: bool) -> bool",
        "Whether `x` is true for every match of the body per group (a non-bool is a deny).",
        "let healthy = all(ok) where check(_, ok)",
    ),
    r(
        "env.var",
        Ext,
        "env.var(name: string) -> secret(string)",
        "The environment variable of the process that plans: the built-in `env` provider's extern, a secret.",
        "let token = env.var(\"API_TOKEN\")",
    ),
    r(
        "import",
        Kw,
        "import \"PATH\"",
        "Include a file, once, where the import stands; paths are from the project's root.",
        "import \"modules/network.df\"",
    ),
    r(
        "key",
        Kw,
        "key NAME: TYPE (= DEFAULT)? (check BODY)?",
        "An input the target gives (`dform plan shop env=prod`), never `--set`: each value is a deployment of the stack, with its own state.",
        "key env: enum(\"dev\", \"prod\") = \"dev\"",
    ),
    r(
        "type",
        Kw,
        "type NAME = TYPE | type TYPE { PATH: TYPE FLAG*, ... }",
        "A type alias, or a resource type's attributes.",
        "type environment = enum(\"dev\", \"prod\")",
    ),
    r(
        "decl",
        Kw,
        "decl NAME(COLUMN [: TYPE], ...) mixed?",
        "Declare a relation by its columns: one fed from outside, or one with both facts and rules (`mixed`); its columns name its arguments.",
        "decl zone_index(zone, index) mixed",
    ),
    r(
        "extern",
        Kw,
        "extern NAME(+IN: TYPE, -OUT: TYPE, ...)",
        "A relation asked of the provider on demand, its `+` columns bound; nothing keeps its answers (`memo.first` does).",
        "extern dns.lookup(+name, -addr: string)",
    ),
    r(
        "input",
        Kw,
        "input NAME: TYPE (= DEFAULT)? (check BODY)? | input NAME(COLUMN: TYPE, ...) from SOURCE",
        "A typed input of the stack or a component; with columns, a relation read from a table or a fact file (`facts(PATH)`).",
        "input env: environment = \"staging\"",
    ),
    r(
        "output",
        Kw,
        "output NAME (: TYPE)? = TERM (where BODY)?",
        "A component's or stack's output, its type and its value in one statement.",
        "output vpc: net.vpc = vpc",
    ),
    r(
        "let",
        Kw,
        "let NAME = TERM (where BODY)?",
        "A value, read by name; one that holds a reference (a resource) is read through with a dot.",
        "let db = db.postgres[\"main\"]",
    ),
    r(
        "set",
        Kw,
        "set REFERENCE.PATH (= | +=) TERM @RANK? (where BODY)?",
        "A contribution to a block declared elsewhere, or to an input (under a `where`).",
        "set r.tags.team = \"platform\" @default where r in resource",
    ),
    r(
        "component",
        Kw,
        "component NAME { STATEMENTS }",
        "A component, an item of a module: a type the program defines, made of resources, with inputs and outputs; `resource C NAME { .. }` makes one, its predicates private to it, its outputs its attributes.",
        "component network { input vpc_net: inet }",
    ),
    r(
        "use",
        Kw,
        "use PATH (as NAME)? { INPUT = TERM, ... }? (where BODY)?",
        "Import a module, a file by its path from the project root, once under NAME: its items read as `NAME.x`, its rules and denies run over what this scope sees, its inputs bound by the block or their defaults, its resources stamped once as `NAME.x`. `use stacks.NAME` binds a stack's deployments, read as `NAME[k=v].output`; `use PROVIDER { SETTING = TERM, ... }` imports and configures a provider.",
        "use baseline",
    ),
    r(
        "resource",
        Kw,
        "resource TYPE NAME @RANK? { PATH = TERM, ... } (where BODY)?",
        "A resource the program wants, one per answer of its clause, its fields contributions; `x in resource` is any resource. TYPE is a provider's type or a component, whose resource's fields are its inputs.",
        "resource net.vpc vpc { cidr = vpc_net }",
    ),
    r(
        "settings",
        Kw,
        "set { PATH = TERM, ... } @RANK? (where BODY)?",
        "Gone (R-38): an input is given by `set`, several under one clause by `set { .. } where ..`, a document's leaves by `set from DOC`.",
        "set { db.multi_az = true } where env == \"prod\"",
    ),
    r(
        "deny",
        Kw,
        "deny \"MESSAGE\" {FIELDS}? where BODY",
        "A check: plan fails with the message when the body holds.",
        "deny \"prod needs multi_az\" where env == \"prod\", pg in db.postgres, not pg.multi_az",
    ),
    r(
        "warn",
        Kw,
        "warn \"MESSAGE\" {FIELDS}? where BODY",
        "A check: plan warns with the message when the body holds.",
        "warn \"no owner tag\" where r in net.vpc, not has r.tags.owner",
    ),
    r(
        "not",
        Kw,
        "not LITERAL | not { BODY }",
        "Negation: holds when the literal, or the whole body, has no match.",
        "unused(s) where s in net.subnet, not attached(s)",
    ),
    r(
        "in",
        Kw,
        "TERM in TYPE | TERM in resource | TERM in world.TYPE | TERM in LIST",
        "Membership: a resource of a type, any resource, a live object, or an element of a list.",
        "vpc_peer(a, b) where a in net.vpc, b in net.vpc",
    ),
    r(
        "has",
        Kw,
        "has REFERENCE.PATH",
        "The attribute has a value.",
        "tagged(r) where r in net.vpc, has r.tags.owner",
    ),
    r(
        "where",
        Kw,
        "HEAD where BODY | BLOCK where BODY",
        "The clause of a rule, check, `let`, `set`, output or block, after it: a query; a block is one resource (row, instance) per answer.",
        "vpc_peer(a, b) where vpc_peer_pair(_, _, a, b)",
    ),
    r(
        "check",
        Kw,
        "input NAME: TYPE (= DEFAULT)? check BODY | PATH: TYPE FLAG* check BODY",
        "A refinement of an input's or a `type` block attribute's type, the value named by its own name.",
        "input replicas: int = 2 check replicas >= 1",
    ),
    r("true", Kw, "true", "The boolean true.", "multi_az = true"),
    r(
        "false",
        Kw,
        "false",
        "The boolean false.",
        "private_api = false",
    ),
];

/// The language's reference: every function a program may call, from
/// `std/*.df`, then [`REFERENCE`]'s aggregates, externs and keywords.
pub fn references() -> &'static [Reference] {
    static ALL: std::sync::LazyLock<Vec<Reference>> = std::sync::LazyLock::new(|| {
        crate::functions::registry()
            .functions()
            .filter(|f| !f.internal)
            .map(|f| Reference {
                name: &f.name,
                kind: RefKind::Function,
                signature: &f.signature,
                summary: &f.summary,
                example: &f.example,
            })
            .chain(REFERENCE.iter().copied())
            .collect()
    });
    &ALL
}

/// The reference entry of `name`: a call's (function, aggregate or
/// extern) when `call`, else a keyword's before a builtin's.
pub fn reference(name: &str, call: bool) -> Option<&'static Reference> {
    let all = references();
    let is = |e: &&Reference| e.name == name;
    if call {
        all.iter().filter(is).find(|e| e.kind != RefKind::Keyword)
    } else {
        all.iter()
            .filter(is)
            .find(|e| e.kind == RefKind::Keyword)
            .or_else(|| all.iter().find(is))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hover, completion and signature help read `references()`: every
    /// function a program may call (from `std/*.df`), aggregate and
    /// keyword has its entry.
    #[test]
    fn every_builtin_and_keyword_has_a_reference() {
        let functions = crate::functions::registry()
            .functions()
            .filter(|f| !f.internal)
            .map(|f| f.name.as_str());
        let calls = functions
            .chain(crate::partition::AGGREGATES.iter().copied())
            .chain([crate::syntax::resolve::ENV_VAR]);
        for name in calls {
            let e = reference(name, true).unwrap_or_else(|| panic!("no reference for {name}"));
            assert!(e.signature.starts_with(&format!("{name}(")), "{e:?}");
        }
        for (name, _) in crate::lexer::KEYWORDS {
            let e = reference(name, false).unwrap_or_else(|| panic!("no reference for {name}"));
            assert_eq!(e.kind, RefKind::Keyword, "{e:?}");
        }
        for e in references() {
            assert!(!e.summary.is_empty() && !e.example.is_empty(), "{e:?}");
        }
    }
}
