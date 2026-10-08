//! The partition graph of proposal E §2.6 / DR-12 as revised by F, run over
//! the program as evaluation runs it ([`compile`]: lowered, computed refs
//! rewritten to `attr` reads, the computed prelude expanded). This is the evaluator's stratifier: `engine::eval`
//! evaluates strata in the order computed here and assigns every rule to
//! the stratum of its head node, and `dform dev strata` prints the same graph.
//!
//! Nodes:
//!   * `arg(T, A, P, V, Rank)` heads (lowering turns `arg`, `arg_add`,
//!     `setting`, `setting_add` and `output` into this core form, with the
//!     pseudo-types `settings` and `output`, E §2.5) are *contributions* to
//!     the attribute aggregate and get a node `(arg, T, P)`; the aggregate
//!     they feed gets a node `(attr, T, P)`. `T` and `P` are the head's
//!     constant type and normalized path, or `*` when not constant.
//!   * Body reads of `attr`/`attr_stuck`/`attr_conflict` are reads of the
//!     aggregate: they connect to every `(attr, T, P)` node they unify with,
//!     with a NEGATIVE edge (every reader of an aggregate is above its group).
//!   * Every other type-keyed core predicate (`want`, `adopt`, ...; F's
//!     DR-12 revised) is one node per constant type, `(want, T)`, or
//!     `(want, *)` when the type is not constant.
//!   * Every other predicate is one node.
//!
//! Edges run body -> head. Negative when: the literal is under `not`; the
//! head is an aggregate (`collect*`); the literal reads the attribute
//! aggregate; the literal is an extern.
//!
//! The computed-attribute prelude rule of E §2.5 is expanded per schema row:
//! `(arg, T, P) :- (want, T)` for every computed `P` of `T` (E §4.3).
//!
//! Output: strata, or the negative SCC with every rule on it. The AST carries
//! no spans (DESIGN.org "No source locations"), so a rule is identified by its
//! index in the lowered program and its pretty-printed text.

use crate::ast::{Atom, Extern, Lit, Program, RuleStmt, Stmt, Term};
use crate::schema::Schema;
use crate::spell;
use crate::transform;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Node {
    pub pred: String,
    pub typ: Option<String>,
    pub path: Option<String>,
    /// The resources of the type this node is about, when the type is
    /// partitioned by address too ([`Options::split`]) and the rule's text
    /// fixes them; `None` is every resource of the type.
    pub addr: Option<Addr>,
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.typ.as_deref().unwrap_or("*");
        let t = match &self.addr {
            Some(a) => format!("{t}[{a}]"),
            None => t.to_string(),
        };
        match (&self.typ, &self.path) {
            (None, None) => write!(f, "{}", self.pred),
            (_, None) => write!(f, "({}, {t})", self.pred),
            (_, p) => write!(f, "({}, {t}, {})", self.pred, p.as_deref().unwrap_or("*")),
        }
    }
}

/// The addresses a rule's text fixes for a resource (R-107): a union of
/// patterns, each the literal pieces of an address with any text between
/// two pieces. One piece is a literal address (`"k3s.server"`); the agents
/// `"${name}-agent-${i}"` of the copy `k3s` are `"k3s.*-agent-*"` (or
/// that name quoted, R-112, should a gap hold a dot).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr(Vec<Vec<String>>);

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alts: Vec<String> = self.0.iter().map(|g| spell::quote(&g.join("*"))).collect();
        write!(f, "{}", alts.join("|"))
    }
}

impl Addr {
    /// One literal address.
    pub fn exact(a: &str) -> Addr {
        Addr(vec![vec![a.to_string()]])
    }

    /// Whether some address is in both.
    pub fn overlaps(&self, other: &Addr) -> bool {
        self.0
            .iter()
            .any(|a| other.0.iter().any(|b| globs_meet(a, b)))
    }

    /// The addresses `t` can be in a rule with `body`: a literal; a
    /// variable the body equates to one of these; `format(..)` with its
    /// literal text, an argument that is not a literal any text;
    /// `scoped(S, N)` with its scope. `None` when the text does not fix
    /// it, or fixes nothing (`str.format("%s", N)`).
    pub fn of(t: &Term, body: &[Lit]) -> Option<Addr> {
        let globs = addr_globs(t, body, 4)?;
        let any = |g: &Vec<String>| g.len() > 1 && g.iter().all(String::is_empty);
        (!globs.iter().any(any)).then_some(Addr(globs))
    }
}

fn addr_globs(t: &Term, body: &[Lit], depth: usize) -> Option<Vec<Vec<String>>> {
    match t {
        Term::Val(Value::Str(s)) => Some(vec![vec![s.clone()]]),
        Term::Var(x) if depth > 0 => body.iter().find_map(|l| match l {
            Lit::Eq(Term::Var(y), u) | Lit::Eq(u, Term::Var(y))
                if y == x && !matches!(u, Term::Var(z) if z == x) =>
            {
                addr_globs(u, body, depth - 1)
            }
            _ => None,
        }),
        Term::Func { name, args } if name == crate::ir::FORMAT => {
            let fmt = args.first()?.as_str()?;
            let mut glob = vec![String::new()];
            for (i, part) in fmt.split("%s").enumerate() {
                if i > 0 {
                    match args.get(i)? {
                        Term::Val(v @ (Value::Str(_) | Value::Int(_))) => glob
                            .last_mut()
                            .unwrap()
                            .push_str(&crate::functions::value_to_string(v)),
                        _ => glob.push(String::new()),
                    }
                }
                glob.last_mut().unwrap().push_str(part);
            }
            Some(vec![glob])
        }
        // A header name's segment (R-112): itself, or quoted when it
        // holds a dot, which a literal piece may say and a gap may hold.
        Term::Func { name, args } if name == crate::ir::NAME_SEGMENT && args.len() == 1 => {
            let mut out = Vec::new();
            for g in addr_globs(&args[0], body, depth)? {
                let must = g.iter().any(|p| crate::ir::name_segment(p) != p.as_str());
                if !must {
                    out.push(g.clone());
                }
                if must || g.len() > 1 {
                    let mut q: Vec<String> = g
                        .iter()
                        .map(|p| {
                            let l = crate::ir::string_literal(p);
                            l[1..l.len() - 1].to_string()
                        })
                        .collect();
                    q[0].insert(0, '"');
                    q.last_mut().unwrap().push('"');
                    out.push(q);
                }
            }
            Some(out)
        }
        Term::Func { name, args } if name == crate::ir::SCOPED && args.len() == 2 => {
            let scope = args[0].as_str()?;
            let prefix = crate::ir::scoped(scope, "");
            let mut out = Vec::new();
            for g in addr_globs(&args[1], body, depth)? {
                // A name that is already an address is itself (R-65).
                let scoped = g.iter().any(|p| crate::ir::is_scoped(p));
                if !scoped {
                    let mut s = g.clone();
                    s[0] = format!("{prefix}{}", s[0]);
                    out.push(s);
                }
                if scoped || g.len() > 1 {
                    out.push(g);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Whether two patterns ([`Addr`]) match a common address: a search over
/// pairs of positions, a gap taking a character or a gap of the other.
fn globs_meet(a: &[String], b: &[String]) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Ch(char),
        Gap,
    }
    let toks = |g: &[String]| -> Vec<Tok> {
        let mut out = Vec::new();
        for (i, p) in g.iter().enumerate() {
            if i > 0 {
                out.push(Tok::Gap);
            }
            out.extend(p.chars().map(Tok::Ch));
        }
        out
    };
    let (a, b) = (toks(a), toks(b));
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![(0usize, 0usize)];
    while let Some((i, j)) = todo.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        if i == a.len() && j == b.len() {
            return true;
        }
        if a.get(i) == Some(&Tok::Gap) {
            todo.push((i + 1, j));
            if j < b.len() {
                todo.push((i, j + 1));
            }
        }
        if b.get(j) == Some(&Tok::Gap) {
            todo.push((i, j + 1));
            if i < a.len() {
                todo.push((i + 1, j));
            }
        }
        if let (Some(Tok::Ch(x)), Some(Tok::Ch(y))) = (a.get(i), b.get(j))
            && x == y
        {
            todo.push((i + 1, j + 1));
        }
    }
    false
}

impl Node {
    pub fn plain(pred: &str) -> Node {
        Node {
            pred: pred.into(),
            typ: None,
            path: None,
            addr: None,
        }
    }
    pub fn unifies(&self, other: &Node) -> bool {
        if self.pred != other.pred {
            return false;
        }
        let ok = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        };
        let addrs = match (&self.addr, &other.addr) {
            (Some(x), Some(y)) => x.overlaps(y),
            _ => true,
        };
        ok(&self.typ, &other.typ) && addrs && self.paths_unify(other)
    }

    /// A scoped cell's path is `scope::k` ([`type_path_node`]), `k` in
    /// the stack's own scope and `*::k` in a scope that is not constant,
    /// which is `k` in every scope; so is a path whose type is not
    /// constant, since its type may be a scoped cell's.
    fn paths_unify(&self, other: &Node) -> bool {
        let (Some(x), Some(y)) = (&self.path, &other.path) else {
            return true;
        };
        if x == y {
            return true;
        }
        // A value body's write of the keys its type does not declare
        // (`per_path`) is every top-level path but the declared ones.
        let top = |p: &str| p.strip_suffix(transform::ELEM).unwrap_or(p).to_string();
        match (other_keys(x), other_keys(y)) {
            (Some(_), Some(_)) => return true,
            (Some(ks), None) => return !ks.contains(&top(y)),
            (None, Some(ks)) => return !ks.contains(&top(x)),
            (None, None) => {}
        }
        let split = |p: &str| match p.rsplit_once("::") {
            Some((s, k)) => (Some(s.to_string()), k.to_string()),
            None => (None, p.to_string()),
        };
        let ((sx, kx), (sy, ky)) = (split(x), split(y));
        let any = |n: &Node, s: &Option<String>| match s {
            Some(s) => s == "*",
            None => n.typ.is_none(),
        };
        kx == ky && (any(self, &sx) || any(other, &sy))
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: Node,
    pub to: Node,
    pub negative: bool,
    /// Index into `Graph::rules`; `None` for a prelude edge.
    pub rule: Option<usize>,
    pub why: String,
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub nodes: BTreeSet<Node>,
    pub edges: Vec<Edge>,
    pub rules: Vec<RuleStmt>,
    /// The nodes each rule defines, by its index in `rules`: the strata
    /// it runs in. One, but for a rule that takes its head's address from
    /// a read of a type split by address: one per address it reads.
    pub heads: Vec<Vec<Node>>,
    /// The types partitioned by address too ([`Options::split`]).
    pub split: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Predicates that are externs (negative edges into their readers).
    pub externs: BTreeSet<String>,
    /// Resource types partitioned by address as well as by path (R-107):
    /// a `want` or an aggregate of one is a node per address its rule's
    /// text fixes, so an agent's `want` reads its server's `attr` without
    /// reading its own. [`compile`] splits a type when the graph by path
    /// has a negative cycle through a rule that reads its own type.
    pub split: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    Stratified {
        strata: BTreeMap<Node, usize>,
    },
    Rejected {
        scc: BTreeSet<Node>,
        negative_edges: Vec<Edge>,
    },
}

/// The text of a string literal term, owned ([`Term::as_str`]).
pub(crate) fn const_str(t: &Term) -> Option<String> {
    t.as_str().map(str::to_string)
}

/// A's path normalization, approximated without a schema: a resource
/// attribute path is normalized to its first segment (the fake provider's
/// attributes are all top-level keys); an output key is one declared leaf
/// per full key. `transform::normalize_contribution` is the same rule
/// applied to a contribution's value.
pub fn normalize_path(typ: &Option<String>, path: &str) -> String {
    match typ.as_deref() {
        Some(transform::OUTPUT) => path.to_string(),
        // An element write (`transform::ELEM`) is its own node, `P[]`.
        _ if path.ends_with(transform::ELEM) => {
            let first = crate::ir::path_segments(path)[0];
            let top = crate::ir::segment_parts(first).0;
            format!("{}{}", crate::ir::path_key(&top), transform::ELEM)
        }
        _ => crate::ir::path_segments(path)[0].to_string(),
    }
}

/// The `(pred, T, P)` node of an atom whose type is column 0 and path is
/// column 2. An input, `let` or output cell is partitioned by its scope
/// too (column 1): a module instance's input `k` is the path `m.i::k`,
/// distinct from the stack's own input `k` (scope `""`), so `resource m i
/// { k = k }` passes one cell to another instead of reading its own
/// aggregate, and `output k = c.k` reads the copy's output `k` (R-100). A
/// scope that is not constant is any scope, `*::k`.
fn type_path_node(pred: &str, atom: &Atom) -> Node {
    let typ = const_str(&atom.args[0]);
    let path = const_str(&atom.args[2]).map(|p| normalize_path(&typ, &p));
    let path = match typ.as_deref() {
        Some(t) if scoped_cell(t) => match const_str(&atom.args[1]) {
            Some(scope) if scope.is_empty() => path,
            Some(scope) => path.map(|p| format!("{scope}::{p}")),
            None => path.map(|p| format!("*::{p}")),
        },
        _ => path,
    };
    Node {
        pred: pred.into(),
        typ,
        path,
        addr: None,
    }
}

/// Whether a cell of the pseudo-type `typ` is partitioned by its scope
/// ([`type_path_node`]): an input's, a `let`'s, an output's.
pub(crate) fn scoped_cell(typ: &str) -> bool {
    matches!(
        typ,
        crate::modules::INPUT | crate::modules::LET | transform::OUTPUT
    )
}

/// A contribution head `arg(T, A, P, V, Rank)` (the lowered core form):
/// its `(arg, T, P)` node and the `(attr, T, P)` aggregate it feeds.
fn contrib_node(atom: &Atom) -> Option<(Node, Node)> {
    if atom.pred != "arg" || atom.args.len() != 5 {
        return None;
    }
    let arg = type_path_node("arg", atom);
    Some((arg.clone(), aggregate_of(&arg)))
}

/// The `(attr, T, P)` aggregate an `(arg, T, P)` node feeds: an element
/// write's `P[]` feeds `P`.
fn aggregate_of(arg: &Node) -> Node {
    Node {
        pred: "attr".into(),
        typ: arg.typ.clone(),
        path: arg
            .path
            .as_ref()
            .map(|p| p.strip_suffix(transform::ELEM).unwrap_or(p).to_string()),
        addr: arg.addr.clone(),
    }
}

/// A read of the aggregate's outputs: `attr/4`, `attr_stuck/4`,
/// `attr_conflict/5` all read the `(attr, T, P)` node.
fn aggregate_read(atom: &Atom) -> Option<Node> {
    match (atom.pred.as_str(), atom.args.len()) {
        ("attr", 4) | ("attr_stuck", 4) | ("attr_conflict", 5) => {
            Some(type_path_node("attr", atom))
        }
        (transform::ATTR_BASE, 4) => Some(type_path_node(transform::ATTR_BASE, atom)),
        _ => None,
    }
}

/// Compiler-owned predicates whose first column is a resource type (F's
/// DR-12 revised), or a reference to a resource of it (R-42). Each is one
/// node per constant type.
fn type_keyed(pred: &str) -> bool {
    matches!(
        pred,
        "want"
            | "adopt"
            | "identity"
            | "lifecycle"
            | "ignore_changes"
            | "moved"
            | "desired"
            | "world"
    )
}

/// The constant type of a type-keyed column: a type, or a reference's.
fn key_type(t: &Term) -> Option<String> {
    match t {
        Term::Val(Value::Ref { typ, .. }) => Some(typ.clone()),
        Term::Func { name, args } if name == crate::ir::REF && args.len() == 3 => {
            const_str(&args[0])
        }
        t => const_str(t),
    }
}

/// The node a rule with this head defines.
pub fn head_node(atom: &Atom) -> Node {
    if let Some((arg, _)) = contrib_node(atom) {
        return arg;
    }
    if type_keyed(&atom.pred) && !atom.args.is_empty() {
        return Node {
            pred: atom.pred.clone(),
            typ: key_type(&atom.args[0]),
            path: None,
            addr: None,
        };
    }
    Node::plain(&atom.pred)
}

/// The node rule `r` defines: its head's ([`head_node`]); a value body's
/// write of the keys its type does not declare ([`per_path`]) is the path
/// [`other_keys`] reads, every top-level path but those.
fn head_of(r: &RuleStmt) -> Node {
    let mut n = head_node(&r.head);
    if let Some(p) = other_path(r) {
        n.path = Some(p);
    }
    n
}

/// The path of [`per_path`]'s catch-all: `!{K1,K2,..}`, the segments of
/// the keys it leaves out, when `r` is one (`arg(T, A, P, V, R) :- ..,
/// not member([K1, ..], K), P = __segment(K)`).
fn other_path(r: &RuleStmt) -> Option<String> {
    if r.head.pred != "arg" || r.head.args.len() != 5 {
        return None;
    }
    let Term::Var(p) = &r.head.args[2] else {
        return None;
    };
    let k = r.body.iter().find_map(|l| match l {
        Lit::Eq(Term::Var(x), Term::Func { name, args })
            if x == p && name == crate::ir::NAME_SEGMENT =>
        {
            match args.as_slice() {
                [Term::Var(k)] => Some(k),
                _ => None,
            }
        }
        _ => None,
    })?;
    r.body.iter().find_map(|l| match l {
        Lit::Not(a) if a.pred == "member" => match a.args.as_slice() {
            [Term::Val(Value::List(keys)), Term::Var(x)] if x == k => {
                let segs: Vec<String> = keys
                    .iter()
                    .map(|v| crate::ir::name_segment(v.as_str().unwrap_or_default()).into_owned())
                    .collect();
                Some(format!("{OTHER_OPEN}{}}}", segs.join(",")))
            }
            _ => None,
        },
        _ => None,
    })
}

/// How [`other_path`] opens its path: no attribute's path starts so.
const OTHER_OPEN: &str = "!{";

/// The keys an [`other_path`] leaves out.
fn other_keys(p: &str) -> Option<BTreeSet<String>> {
    let inner = p.strip_prefix(OTHER_OPEN)?.strip_suffix('}')?;
    Some(inner.split(',').map(str::to_string).collect())
}

/// The pattern a body literal reads.
fn body_pattern(atom: &Atom) -> Node {
    aggregate_read(atom).unwrap_or_else(|| head_node(atom))
}

/// `n` with the addresses its atom's column 1 fixes in a rule with
/// `body`, when `n` is a resource's (`want`, a contribution, the
/// aggregate) and its type is partitioned by address (R-107).
fn addressed(mut n: Node, atom: &Atom, body: &[Lit], split: &BTreeSet<String>) -> Node {
    if is_resource_node(&n)
        && n.typ.as_ref().is_some_and(|t| split.contains(t))
        && let Some(a) = atom.args.get(1)
    {
        n.addr = Addr::of(a, body);
    }
    n
}

fn is_builtin_or_edb(pred: &str) -> bool {
    matches!(pred, "member" | "enumerate") || crate::functions::is_predicate(pred)
}

/// The aggregates (docs/grammar.md "Aggregates"): written `n = count(x)` in
/// a body, lowered to a rule head that applies one.
pub const AGGREGATES: &[&str] = &[
    "collect_set",
    "collect_list",
    "count",
    "sum",
    "min",
    "max",
    "any",
    "all",
];

pub fn is_aggregate_head(head: &Atom) -> bool {
    head.args.iter().any(|t| match t {
        Term::Func { name, .. } => AGGREGATES.contains(&name.as_str()),
        _ => false,
    })
}

/// Build the graph over a program: lower it, then `build_lowered` with the
/// program's `extern` declarations added to `opts`.
pub fn build(program: &Program, schema: &Schema) -> Result<Graph> {
    Ok(compile(program, &schema.facts)?.graph)
}

/// A program compiled for evaluation, with the partition graph it is
/// stratified by.
#[derive(Debug, Clone)]
pub struct Compiled {
    /// The lowered program's statements, before the ref rewrite (a source
    /// fact's statement index is its provenance until the AST has spans).
    pub statements: Vec<Stmt>,
    pub rules: Vec<RuleStmt>,
    pub facts: Vec<Atom>,
    pub externs: BTreeSet<Extern>,
    pub schema: Schema,
    /// `rules`, in that order.
    pub graph: Graph,
    /// The stack's and every module instance's typed inputs (`dform dev
    /// effects`'s reads, by name, per scope).
    pub inputs: Vec<crate::inputs::Declared>,
}

/// Compile `program` as `engine::eval` runs it and build its partition
/// graph: lower; read the provider schema from `given` (the facts given to
/// the run) and the program's own schema facts; reject a write to a
/// computed path; rewrite each `ref` to a computed path into an `attr` read
/// (with its dangling-ref deny); expand the computed prelude per schema row
/// (E §2.5, §4.3). The one place the graph is built: `dform dev strata`
/// and `dform dev graph --strata` print exactly what evaluation stratifies with.
pub fn compile(program: &Program, given: &[Atom]) -> Result<Compiled> {
    let lowered = transform::lower(program)?;
    let mut rules: Vec<RuleStmt> = Vec::new();
    let mut facts: Vec<Atom> = Vec::new();
    for s in &lowered.program.statements {
        match s {
            Stmt::Rule(r) => rules.push(r.clone()),
            Stmt::Fact(a) => facts.push(a.clone()),
            _ => {}
        }
    }
    let schema = schema_of(given, &facts)?;
    let (rules, facts) = answer_has(rules, facts, &schema);
    let rules = answer_identity(rules, &schema);
    transform::check_computed_writes(&rules, &facts, &schema)?;
    let (mut rules, facts) = transform::rewrite_computed_refs(rules, facts, &schema);
    rules.extend(transform::computed_prelude(&schema));
    let externs: BTreeSet<String> = lowered.externs.iter().map(|e| e.pred.clone()).collect();
    let mut graph = stratified(&rules, &facts, &schema, &externs);
    // R-116: a write whose type is a variable (`set r.metadata.labels.owner
    // = .. where r in k8s`) is one node `(arg, *, P)`, which every reader
    // of `P` of any type reads. On a negative cycle, each such rule is a
    // rule per type it can be, kept when that stratifies.
    // A value body known only at run time (R-126) writes a path per key
    // of its document, one node `(arg, T, *)` too; on a negative cycle,
    // each is a rule per attribute its type declares and one for every
    // other key (`per_path`), kept when that stratifies, alone or with
    // R-116's.
    if let Verdict::Rejected { .. } = stratify(&graph) {
        let typed = per_type(&rules, &facts, &graph);
        let pathed = per_path(&rules, &schema);
        let both = typed.as_deref().and_then(|t| per_path(t, &schema));
        for expanded in [typed, pathed, both].into_iter().flatten() {
            let finer = stratified(&expanded, &facts, &schema, &externs);
            if let Verdict::Stratified { .. } = stratify(&finer) {
                (rules, graph) = (expanded, finer);
                break;
            }
        }
    }
    Ok(Compiled {
        statements: lowered.program.statements,
        rules,
        facts,
        externs: lowered.externs,
        schema,
        graph,
        inputs: lowered.inputs,
    })
}

/// The graph of `rules`, partitioned by address too where that stratifies
/// a negative cycle (R-107).
fn stratified(
    rules: &[RuleStmt],
    facts: &[Atom],
    schema: &Schema,
    externs: &BTreeSet<String>,
) -> Graph {
    let mut opts = Options {
        externs: externs.clone(),
        split: BTreeSet::new(),
    };
    let rules = rules.to_vec();
    let facts = facts.to_vec();
    let mut graph = build_lowered(rules.clone(), &facts, schema, &opts);
    // R-107: a negative cycle through a type's resources and an attribute
    // of them (an agent reading its server's address) is partitioned by
    // address too, kept when that stratifies it or the cycle is one between
    // addresses (a resource reading itself, two reading each other).
    while let Verdict::Rejected { scc, .. } = stratify(&graph) {
        let more = own_type_reads(&scc, &opts.split);
        if more.is_empty() {
            break;
        }
        opts.split.extend(more);
        let finer = build_lowered(rules.clone(), &facts, schema, &opts);
        match stratify(&finer) {
            Verdict::Rejected { negative_edges, .. }
                if !negative_edges
                    .iter()
                    .any(|e| e.rule.is_some() && e.from.addr.is_some() && e.to.addr.is_some()) =>
            {
                break;
            }
            _ => graph = finer,
        }
    }
    graph
}

/// The namespace test `r in NS` lowers to (R-49, the resolver's
/// `NAMESPACE`): `__namespace(NS, T)`, a fact per type of the namespace.
const NAMESPACE: &str = "__namespace";

/// `rules` with each contribution whose type is a variable written once
/// per concrete type it can be (R-116): the type substituted through the
/// whole rule, so `(arg, k8s.deployment, metadata)` and `(arg,
/// k8s.namespace, metadata)` are apart and each copy reads its own type's
/// resources. The types a variable can be are what its positive body
/// literals allow: `__namespace(NS, T)` the namespace's types; `want(T,
/// A)` (or another type-keyed relation) the types `graph` defines it for,
/// when no rule defines it for a type that is not constant. `None` when no
/// rule is expanded.
fn per_type(rules: &[RuleStmt], facts: &[Atom], graph: &Graph) -> Option<Vec<RuleStmt>> {
    let mut out = Vec::with_capacity(rules.len());
    let mut any = false;
    for r in rules {
        let types = match (contrib_node(&r.head), r.head.args.first()) {
            (Some(_), Some(Term::Var(t))) => types_of(t, r, facts, graph),
            _ => None,
        };
        let (Some(types), Some(Term::Var(t))) = (types, r.head.args.first()) else {
            out.push(r.clone());
            continue;
        };
        any = true;
        for typ in types {
            let env = BTreeMap::from([(t.clone(), Value::Str(typ))]);
            out.push(RuleStmt {
                head: Atom {
                    args: r.head.args.iter().map(|a| a.subst(&env)).collect(),
                    ..r.head.clone()
                },
                body: r.body.iter().map(|l| l.subst(&env)).collect(),
            });
        }
    }
    any.then_some(out)
}

/// `rules` with each value body known only at run time (R-126: `arg(T, A,
/// P, V, R) :- .., X = __body(VALUE), member(X, K, V), P = __segment(K)`)
/// written once per key it can have, so a rule reading one attribute of
/// `T` and writing another is no cycle through it: the keys of VALUE when
/// it is an object written out; else one rule per top-level attribute `T`
/// declares (`member(X, "spec", V)`, the path `spec`) and one for every
/// other key (`not member(["metadata", "spec"], K)`), whose node is every
/// path but those ([`other_path`]): a document's `apiVersion` and `kind`,
/// which the provider checks. A type the schema declares no attribute of
/// is left as it is. `None` when no rule is expanded.
fn per_path(rules: &[RuleStmt], schema: &Schema) -> Option<Vec<RuleStmt>> {
    let mut out = Vec::with_capacity(rules.len());
    let mut any = false;
    for r in rules {
        match value_body(r).and_then(|vb| per_key(r, &vb, schema)) {
            Some(rs) => {
                any = true;
                out.extend(rs);
            }
            None => out.push(r.clone()),
        }
    }
    any.then_some(out)
}

/// A value body's rule, by its parts ([`per_path`]).
struct ValueBody<'a> {
    typ: &'a str,
    value: &'a Term,
    /// The body's `member(X, K, V)` and `P = __segment(K)`, by index.
    member: usize,
    segment: usize,
}

fn value_body(r: &RuleStmt) -> Option<ValueBody<'_>> {
    if r.head.pred != "arg" || r.head.args.len() != 5 {
        return None;
    }
    let (Some(typ), Term::Var(p)) = (r.head.args[0].as_str(), &r.head.args[2]) else {
        return None;
    };
    let (x, value) = r.body.iter().find_map(|l| match l {
        Lit::Eq(Term::Var(x), Term::Func { name, args })
            if name == crate::ir::RESOURCE_BODY && args.len() == 1 =>
        {
            Some((x, &args[0]))
        }
        _ => None,
    })?;
    let member = r.body.iter().position(|l| {
        matches!(l, Lit::Pos(a) if a.pred == "member"
            && matches!(a.args.as_slice(), [Term::Var(y), Term::Var(_), _] if y == x))
    })?;
    let Lit::Pos(m) = &r.body[member] else {
        return None;
    };
    let Term::Var(k) = &m.args[1] else {
        return None;
    };
    let segment = r.body.iter().position(|l| {
        matches!(l, Lit::Eq(Term::Var(y), Term::Func { name, args })
            if y == p && name == crate::ir::NAME_SEGMENT
                && matches!(args.as_slice(), [Term::Var(z)] if z == k))
    })?;
    Some(ValueBody {
        typ,
        value,
        member,
        segment,
    })
}

/// `r` as one rule per key ([`per_path`]); `None` when its keys are not
/// known.
fn per_key(r: &RuleStmt, vb: &ValueBody, schema: &Schema) -> Option<Vec<RuleStmt>> {
    let written: Option<Vec<String>> = match vb.value {
        Term::Obj(m) => Some(m.keys().cloned().collect()),
        Term::Val(Value::Obj(m)) => Some(m.keys().cloned().collect()),
        _ => None,
    };
    let (keys, rest) = match written {
        Some(keys) => (keys, false),
        None => {
            let declared: BTreeSet<String> = schema
                .attrs
                .keys()
                .filter(|(t, _)| t == vb.typ)
                .map(|(_, p)| crate::ir::segment_key(crate::ir::path_segments(p)[0]).into_owned())
                .collect();
            if declared.is_empty() {
                return None;
            }
            (declared.into_iter().collect(), true)
        }
    };
    let Lit::Pos(member) = &r.body[vb.member] else {
        return None;
    };
    let mut out = Vec::new();
    for key in &keys {
        let body: Vec<Lit> = r
            .body
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != vb.segment)
            .map(|(i, l)| match i == vb.member {
                true => {
                    let mut m = member.clone();
                    m.args[1] = Term::Val(Value::Str(key.clone()));
                    Lit::Pos(m)
                }
                false => l.clone(),
            })
            .collect();
        let mut head = r.head.clone();
        head.args[2] = Term::Val(Value::Str(crate::ir::name_segment(key).into_owned()));
        out.push(RuleStmt { head, body });
    }
    if rest {
        let mut body = r.body.clone();
        body.insert(
            vb.member + 1,
            Lit::Not(Atom {
                pred: "member".into(),
                args: vec![
                    Term::Val(Value::List(keys.into_iter().map(Value::Str).collect())),
                    member.args[1].clone(),
                ],
                record: None,
                span: member.span,
            }),
        );
        out.push(RuleStmt {
            head: r.head.clone(),
            body,
        });
    }
    Some(out)
}

/// The concrete types the variable `t` can be in `r` ([`per_type`]).
fn types_of(t: &str, r: &RuleStmt, facts: &[Atom], graph: &Graph) -> Option<BTreeSet<String>> {
    let is_t = |x: &Term| matches!(x, Term::Var(v) if v == t);
    let mut out: Option<BTreeSet<String>> = None;
    for l in &r.body {
        let Lit::Pos(a) = l else { continue };
        let allowed: Option<BTreeSet<String>> = match a.args.as_slice() {
            [Term::Val(Value::Str(ns)), x] if a.pred == NAMESPACE && is_t(x) => Some(
                facts
                    .iter()
                    .filter(|f| f.pred == NAMESPACE)
                    .filter(|f| f.args[0].as_str() == Some(ns))
                    .filter_map(|f| const_str(&f.args[1]))
                    .collect(),
            ),
            [x, ..] if type_keyed(&a.pred) && is_t(x) => {
                let defs = graph.nodes.iter().filter(|n| n.pred == a.pred);
                let mut types = BTreeSet::new();
                let mut constant = true;
                for n in defs {
                    match &n.typ {
                        Some(typ) => {
                            types.insert(typ.clone());
                        }
                        None => constant = false,
                    }
                }
                constant.then_some(types)
            }
            _ => None,
        };
        if let Some(allowed) = allowed {
            out = Some(match out {
                Some(prev) => prev.intersection(&allowed).cloned().collect(),
                None => allowed,
            });
        }
    }
    out
}

/// Insert a head node and the aggregate nodes behind it: a contribution's
/// `attr`, and an element write's base.
fn define(defs: &mut BTreeSet<Node>, h: &Node) {
    defs.insert(h.clone());
    if h.pred != "arg" {
        return;
    }
    let attr = aggregate_of(h);
    if h.path
        .as_ref()
        .is_some_and(|p| p.ends_with(transform::ELEM))
    {
        defs.insert(Node {
            pred: transform::ATTR_BASE.into(),
            ..attr.clone()
        });
    }
    defs.insert(attr);
}

/// The prelude's contribution to the computed path `p` of `t`.
fn prelude_arg(t: &str, p: &str, addr: Option<Addr>) -> Node {
    Node {
        pred: "arg".into(),
        typ: Some(t.to_string()),
        path: Some(p.to_string()),
        addr,
    }
}

/// The addresses `want` of `t` is defined at; every address when none is.
fn want_addrs(defs: &BTreeSet<Node>, t: &str) -> Vec<Option<Addr>> {
    let mut out: Vec<Option<Addr>> = defs
        .iter()
        .filter(|n| n.pred == "want" && n.typ.as_deref() == Some(t))
        .map(|n| n.addr.clone())
        .collect();
    if out.is_empty() {
        out.push(None);
    }
    out
}

fn is_resource_node(n: &Node) -> bool {
    matches!(n.pred.as_str(), "want" | "arg" | "attr") || n.pred == transform::ATTR_BASE
}

/// The body literal a rule takes its head's address from (R-107): the
/// head is a resource's of a type split by address, its address a
/// variable the text does not fix, and a positive literal reads a resource
/// of the same type at that variable (`arg(T, A, P, ..) :- want(T, A)`).
/// The rule then derives at every address that literal reads.
fn address_source(r: &RuleStmt, split: &BTreeSet<String>) -> Option<usize> {
    let head = addressed(head_of(r), &r.head, &r.body, split);
    let typ = head.typ.as_ref().filter(|t| split.contains(*t))?;
    if !is_resource_node(&head) || head.addr.is_some() || is_aggregate_head(&r.head) {
        return None;
    }
    let a = match r.head.args.get(1)? {
        Term::Var(a) => a,
        _ => return None,
    };
    r.body.iter().position(|l| match l {
        Lit::Pos(b) => {
            let pat = body_pattern(b);
            is_resource_node(&pat)
                && pat.typ.as_ref() == Some(typ)
                && matches!(b.args.get(1), Some(Term::Var(v)) if v == a)
        }
        _ => false,
    })
}

/// The pattern of the literal a rule takes its head's address from.
fn source_pattern(r: &RuleStmt, at: usize, split: &BTreeSet<String>) -> Node {
    let (Lit::Pos(a) | Lit::Not(a)) = &r.body[at] else {
        unreachable!("an address source is a positive literal")
    };
    addressed(body_pattern(a), a, &r.body, split)
}

/// `has r.PATH` of a resource's attribute, as the resolver marks it:
/// `__has(T, A, "PATH", N)` before the N literals that test it by value.
pub const HAS: &str = "__has";
/// `has r` of a resource (R-152), as the resolver writes it:
/// `__identity(T, A)`.
pub const IDENTITY: &str = "__identity";

/// Answer each `has r` of a resource (R-152): it holds once the resource's
/// identity is known. `__identity(T, A)` becomes the read a reference to
/// it joins (`transform::identity_read`: its `id`, else the top of its
/// first computed attribute) with `__known` over the value, a content
/// position, so before the resource exists the literal is stuck and what
/// it gates waits on the resource, as a read of any computed value of it
/// does; a type with no computed attribute has an identity once it is
/// wanted. Under `not` the resolver puts it in a helper, so `not has r`
/// waits too.
fn answer_identity(rules: Vec<RuleStmt>, schema: &Schema) -> Vec<RuleStmt> {
    let mut n = 0usize;
    rules
        .into_iter()
        .map(|mut r| {
            if !r
                .body
                .iter()
                .any(|l| matches!(l, Lit::Pos(a) if a.pred == IDENTITY))
            {
                return r;
            }
            let mut body = Vec::with_capacity(r.body.len() + 1);
            for l in std::mem::take(&mut r.body) {
                let Lit::Pos(a) = &l else {
                    body.push(l);
                    continue;
                };
                let [typ, addr] = a.args.as_slice() else {
                    body.push(l);
                    continue;
                };
                if a.pred != IDENTITY {
                    body.push(l);
                    continue;
                }
                let mut read = transform::identity_read(typ.clone(), addr.clone(), schema);
                read.span = a.span;
                if read.pred == "attr" {
                    let mut v = Term::Var(format!("__Identity{n}"));
                    read.args[3] = v.clone();
                    // An identity below the cell (`metadata.uid` of a
                    // Kubernetes object): its field, so what waits on it
                    // waits on the identity, not every computed field the
                    // cell holds.
                    let below = id_below(typ, &read.args[2], schema);
                    body.push(Lit::Pos(read));
                    if let Some(rest) = below {
                        let w = Term::Var(format!("__IdentityField{n}"));
                        body.push(Lit::Eq(
                            w.clone(),
                            Term::Func {
                                name: "__path".into(),
                                args: vec![v, Term::Val(Value::Str(rest))],
                            },
                        ));
                        v = w;
                    }
                    n += 1;
                    body.push(Lit::Pos(Atom {
                        pred: "__known".into(),
                        args: vec![v],
                        record: None,
                        span: a.span,
                    }));
                } else {
                    body.push(Lit::Pos(read));
                }
            }
            r.body = body;
            r
        })
        .collect()
}

/// The path below the identity read's cell `cell` of the identity of
/// `typ`, when its schema flags one there (`metadata.uid` of a
/// Kubernetes object: `uid`).
fn id_below(typ: &Term, cell: &Term, schema: &Schema) -> Option<String> {
    let (t, cell) = (key_type(typ)?, cell.as_str()?);
    schema.attrs.iter().find_map(|((at, p), spec)| {
        let rest = p.strip_prefix(cell)?.strip_prefix('.')?;
        (at == &t && spec.has("id") && spec.has("computed")).then(|| rest.to_string())
    })
}

/// The schema's answer to `has`: `__type_has(T, P)` for every path `P` of
/// a configurable attribute of `T` and every object above one.
pub const TYPE_HAS: &str = "__type_has";

/// Answer each `has r.PATH` (R-106). In a rule that writes under PATH
/// (`set r.metadata.labels.owner = .. where r in resource, has
/// r.metadata`), from the schema, when PATH is a configurable attribute of
/// the resource's type or an object holding one: whether the type has it,
/// so the rule does not read what it writes; for a type that is not
/// constant, when some type's schema has PATH, each type's schema answers.
/// Everywhere else the value answers, as it always did: whether the
/// resource sets it (`not has p.spec.podSelector.matchLabels`), and a
/// computed attribute, or one no schema declares, has a value once it is
/// known. The schema's answers are `__type_has` rows in place of the
/// marked literals; the value's are the literals, the mark dropped.
fn answer_has(
    rules: Vec<RuleStmt>,
    mut facts: Vec<Atom>,
    schema: &Schema,
) -> (Vec<RuleStmt>, Vec<Atom>) {
    let mark = |l: &Lit| match l {
        Lit::Pos(a) | Lit::Not(a) if a.pred == HAS && a.args.len() == 4 => Some(a.clone()),
        _ => None,
    };
    if !rules
        .iter()
        .flat_map(|r| &r.body)
        .any(|l| mark(l).is_some())
    {
        return (rules, facts);
    }
    // Every configurable path of each type and the objects above it.
    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();
    for ((t, p), spec) in &schema.attrs {
        if spec.has("computed") {
            continue;
        }
        let segs = crate::ir::path_segments(p);
        for n in 1..=segs.len() {
            declared.insert((t.clone(), segs[..n].join(".")));
        }
    }
    // Whether the schema answers `a` in a rule with `head`: the head
    // writes under its path, of a type that may be its, and the schema
    // has the path.
    let by_schema = |a: &Atom, head: &Atom| -> bool {
        let Some(p) = a.args[2].as_str() else {
            return false;
        };
        let under = |w: &str| {
            w == p
                || w.strip_prefix(p)
                    .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
        };
        let writes_under = head.pred == "arg"
            && head.args.len() == 5
            && head.args[2].as_str().is_some_and(under)
            && match (key_type(&head.args[0]), key_type(&a.args[0])) {
                (Some(x), Some(y)) => x == y,
                _ => true,
            };
        writes_under
            && match key_type(&a.args[0]) {
                Some(t) => declared.contains(&(t, p.to_string())),
                None => declared.iter().any(|(_, q)| q == p),
            }
    };
    // Whether the value answers `a` over a computed attribute (its first
    // segment computed in the type's schema, any type's when the type is
    // not constant): it has a value once the provider reports it, so the
    // test is undetermined while the value is unknown (`__known`).
    let computed = |a: &Atom| -> bool {
        let Some(top) = a.args[2]
            .as_str()
            .and_then(|p| crate::ir::path_segments(p).into_iter().next())
        else {
            return false;
        };
        let is = |t: &str| {
            schema
                .attrs
                .get(&(t.to_string(), top.to_string()))
                .is_some_and(|s| s.has("computed"))
        };
        match key_type(&a.args[0]) {
            Some(t) => is(&t),
            None => schema.attrs.keys().any(|(t, _)| is(t)),
        }
    };
    let mut fresh = 0usize;
    let mut helpers: Vec<RuleStmt> = Vec::new();
    let mut patch: BTreeSet<String> = BTreeSet::new();
    let mut asked: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<RuleStmt> = Vec::with_capacity(rules.len());
    for mut r in rules {
        let mut body = Vec::with_capacity(r.body.len());
        let mut lits = std::mem::take(&mut r.body).into_iter();
        while let Some(l) = lits.next() {
            let Some(a) = mark(&l) else {
                body.push(l);
                continue;
            };
            let n = match &a.args[3] {
                Term::Val(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            if !by_schema(&a, &r.head) {
                if computed(&a) {
                    let read: Vec<Lit> = lits.by_ref().take(n).collect();
                    match l {
                        Lit::Pos(_) => body.extend(known(read, &mut fresh)),
                        _ => body.extend(not_known(read, &mut fresh, &mut helpers, &mut patch)),
                    }
                }
                continue;
            }
            lits.by_ref().take(n).for_each(drop);
            asked.extend(a.args[2].as_str().map(str::to_string));
            let answer = Atom {
                pred: TYPE_HAS.into(),
                args: vec![a.args[0].clone(), a.args[2].clone()],
                ..a
            };
            body.push(match l {
                Lit::Not(_) => Lit::Not(answer),
                _ => Lit::Pos(answer),
            });
        }
        r.body = body;
        out.push(r);
    }
    // A `not has` the resolver put in a helper: the helper's read waits.
    for r in &mut out {
        if patch.contains(&r.head.pred) {
            r.body = known(std::mem::take(&mut r.body), &mut fresh);
        }
    }
    out.extend(helpers);
    for (t, p) in &declared {
        if asked.contains(p) {
            facts.push(Atom {
                pred: TYPE_HAS.into(),
                args: vec![
                    Term::Val(Value::Str(t.clone())),
                    Term::Val(Value::Str(p.clone())),
                ],
                record: None,
                span: Default::default(),
            });
        }
    }
    (out, facts)
}

/// The literals of a `has r.p` that test it by value, with `__known` over
/// the value they read: the attribute read's value, or the walk's end
/// (`Has = __path(..)`), the last literal that binds one.
fn known(mut read: Vec<Lit>, fresh: &mut usize) -> Vec<Lit> {
    let at = read.iter().rposition(|l| match l {
        Lit::Eq(Term::Var(_), _) => true,
        Lit::Pos(a) => a.pred == "attr" && a.args.len() == 4,
        _ => false,
    });
    let Some(at) = at else { return read };
    let v = match &mut read[at] {
        Lit::Eq(Term::Var(v), _) => Term::Var(v.clone()),
        Lit::Pos(a) => match &a.args[3] {
            Term::Var(v) => Term::Var(v.clone()),
            _ => {
                let v = Term::Var(format!("__Known{fresh}"));
                *fresh += 1;
                a.args[3] = v.clone();
                v
            }
        },
        _ => return read,
    };
    let span = match &read[at] {
        Lit::Pos(a) => a.span,
        _ => Default::default(),
    };
    read.insert(
        at + 1,
        Lit::Pos(Atom {
            pred: "__known".into(),
            args: vec![v],
            record: None,
            span,
        }),
    );
    read
}

/// The literals of a `not has r.p` that test it by value, waiting while
/// the value is unknown: `not attr(T, A, P, _)` becomes `not
/// __has_known_N(..)` over a helper that reads the value and holds when
/// it is known ([`known`]); a helper the resolver made (`not __neg_N(..)`)
/// is named in `patch`, its read made to wait in place.
fn not_known(
    read: Vec<Lit>,
    fresh: &mut usize,
    helpers: &mut Vec<RuleStmt>,
    patch: &mut BTreeSet<String>,
) -> Vec<Lit> {
    read.into_iter()
        .map(|l| match l {
            Lit::Not(a) if a.pred.starts_with("__neg_") => {
                patch.insert(a.pred.clone());
                Lit::Not(a)
            }
            Lit::Not(a) if a.pred == "attr" && a.args.len() == 4 => {
                let mut vars = BTreeSet::new();
                for t in &a.args {
                    t.for_each_var(&mut |v| {
                        vars.insert(v.to_string());
                    });
                }
                let args: Vec<Term> = vars.into_iter().map(Term::Var).collect();
                let head = Atom {
                    pred: format!("__has_known_{}", helpers.len()),
                    args,
                    record: None,
                    span: a.span,
                };
                helpers.push(RuleStmt {
                    head: head.clone(),
                    body: known(vec![Lit::Pos(a)], fresh),
                });
                Lit::Not(head)
            }
            l => l,
        })
        .collect()
}

/// The types whose resources a negative cycle runs through, not split by
/// address yet ([`Options::split`]): a type whose `want` and an attribute
/// of it are both on the cycle, so its resources' existence waits on one
/// of their own attributes.
fn own_type_reads(scc: &BTreeSet<Node>, split: &BTreeSet<String>) -> BTreeSet<String> {
    let of = |pred: &str| -> BTreeSet<&String> {
        scc.iter()
            .filter(|n| n.pred == pred)
            .filter_map(|n| n.typ.as_ref())
            .collect()
    };
    let attrs = of("attr");
    of("want")
        .into_iter()
        .filter(|t| attrs.contains(t) && !split.contains(*t))
        .cloned()
        .collect()
}

/// The provider schema among the facts given to a run (the catalog the
/// provider injects), plus any schema facts the program itself states.
fn schema_of(given: &[Atom], program_facts: &[Atom]) -> Result<Schema> {
    let is_schema =
        |a: &&Atom| matches!(a.pred.as_str(), "type_attr" | "type_provider" | "type_mint");
    let rows: Vec<Atom> = given
        .iter()
        .filter(is_schema)
        .chain(program_facts.iter().filter(is_schema))
        .cloned()
        .collect::<BTreeSet<Atom>>()
        .into_iter()
        .collect();
    Schema::from_facts(&rows)
}

/// Build the graph over lowered rules and facts plus the schema's prelude.
/// `Graph::rules` is `rules` in the given order, so a rule's index in the
/// graph is its index in the caller's list.
fn build_lowered(
    rules: Vec<RuleStmt>,
    fact_atoms: &[Atom],
    schema: &Schema,
    opts: &Options,
) -> Graph {
    let mut b = Builder {
        sources: rules
            .iter()
            .map(|r| address_source(r, &opts.split))
            .collect(),
        heads: vec![Vec::new(); rules.len()],
        read: vec![Vec::new(); rules.len()],
        rules,
        opts,
        defs: BTreeSet::new(),
        nodes: BTreeSet::new(),
        edges: Vec::new(),
    };
    b.facts(fact_atoms);
    // The prelude's and the copies' nodes, to a fixpoint: a copy may read
    // what another copy or the prelude defines.
    let minted: Vec<(String, String)> = transform::minted_paths(schema)
        .into_iter()
        .map(|(t, p, _, _)| {
            let p = normalize_path(&Some(t.clone()), &p);
            (t, p)
        })
        .collect();
    b.heads(&minted);
    let prelude = b.prelude(&minted);
    // stuck/4 is a relation when a rule reads it: a node the evaluator
    // defines.
    let stuck_read = b.rules.iter().any(reads_stuck);
    if stuck_read {
        b.defs.insert(Node::plain(STUCK));
    }
    b.nodes.extend(b.defs.iter().cloned());
    let args = b.aggregate_edges();
    for (want, arg) in prelude {
        b.edges.push(Edge {
            from: want,
            to: arg,
            negative: false,
            rule: None,
            why: "prelude: computed attribute null".into(),
        });
    }
    b.rule_edges();
    if stuck_read {
        b.stuck_edges(args);
    }
    b.into_graph()
}

/// A partition graph as it is built: its definition nodes, its edges, and
/// each rule's heads, read sources and the nodes its body reads.
struct Builder<'a> {
    rules: Vec<RuleStmt>,
    opts: &'a Options,
    /// Definition nodes: every head, every fact's predicate, every
    /// aggregate node behind a contribution, and the prelude's computed
    /// contributions.
    defs: BTreeSet<Node>,
    nodes: BTreeSet<Node>,
    edges: Vec<Edge>,
    /// The nodes each rule defines.
    heads: Vec<Vec<Node>>,
    /// The read each rule takes its head's address from, by its index in
    /// the body ([`address_source`]).
    sources: Vec<Option<usize>>,
    /// The nodes each rule's body reads.
    read: Vec<Vec<Node>>,
}

impl Builder<'_> {
    fn into_graph(self) -> Graph {
        Graph {
            nodes: self.nodes,
            edges: self.edges,
            rules: self.rules,
            heads: self.heads,
            split: self.opts.split.clone(),
        }
    }

    /// The definition nodes of the facts.
    fn facts(&mut self, fact_atoms: &[Atom]) {
        let split = &self.opts.split;
        for a in fact_atoms {
            if let Some((arg, _)) = contrib_node(a) {
                let arg = addressed(arg, a, &[], split);
                self.defs.insert(aggregate_of(&arg));
                self.defs.insert(arg);
            } else {
                self.defs.insert(addressed(head_node(a), a, &[], split));
            }
        }
    }

    /// Each rule's head nodes; a rule that takes its head's address from a
    /// read of its own type, split by address (`arg(T, A, ..) :- want(T,
    /// A)`), has one per address it reads, and runs at each
    /// ([`address_source`]). The copies to a fixpoint, with the prelude's
    /// `minted` nodes.
    fn heads(&mut self, minted: &[(String, String)]) {
        let split = &self.opts.split;
        for (i, r) in self.rules.iter().enumerate() {
            if self.sources[i].is_none() {
                let h = addressed(head_of(r), &r.head, &r.body, split);
                define(&mut self.defs, &h);
                self.heads[i].push(h);
            }
        }
        loop {
            let before = self.defs.len();
            for (t, p) in minted.iter().filter(|(t, _)| split.contains(t)) {
                for addr in want_addrs(&self.defs, t) {
                    define(&mut self.defs, &prelude_arg(t, p, addr));
                }
            }
            for (i, r) in self.rules.iter().enumerate() {
                let Some(at) = self.sources[i] else { continue };
                let pat = source_pattern(r, at, split);
                let head = head_of(r);
                let found: BTreeSet<Option<Addr>> =
                    unifying(&self.defs, &pat).map(|d| d.addr.clone()).collect();
                for addr in found {
                    let h = Node {
                        addr,
                        ..head.clone()
                    };
                    if !self.heads[i].contains(&h) {
                        define(&mut self.defs, &h);
                        self.heads[i].push(h);
                    }
                }
            }
            if self.defs.len() == before {
                break;
            }
        }
        for (i, r) in self.rules.iter().enumerate() {
            if self.heads[i].is_empty() {
                let h = head_of(r);
                define(&mut self.defs, &h);
                self.heads[i].push(h);
            }
        }
    }

    /// Prelude: (arg, T, P) :- (want, T) per minted schema row (computed
    /// and optional_computed, E §2.5); expanded per row, P normalized. A
    /// type partitioned by address has it per address its `want` is
    /// defined at. Each (want, arg) pair.
    fn prelude(&mut self, minted: &[(String, String)]) -> Vec<(Node, Node)> {
        let split = &self.opts.split;
        let mut prelude: Vec<(Node, Node)> = Vec::new();
        for (t, p) in minted {
            let addrs = match split.contains(t) {
                true => want_addrs(&self.defs, t),
                false => vec![None],
            };
            for addr in addrs {
                let arg = prelude_arg(t, p, addr.clone());
                let want = Node {
                    pred: "want".into(),
                    typ: Some(t.clone()),
                    path: None,
                    addr,
                };
                define(&mut self.defs, &arg);
                self.defs.insert(want.clone());
                prelude.push((want, arg));
            }
        }
        prelude
    }

    /// Aggregate edges: every (arg, T, P) definition feeds every (attr, T',
    /// P') definition it unifies with, negatively. The arg nodes.
    fn aggregate_edges(&mut self) -> Vec<Node> {
        let args: Vec<Node> = unifying(&self.defs, &Node::plain("arg")).cloned().collect();
        for a in &args {
            let a_as_attr = aggregate_of(a);
            // The base is every contribution but the element writes.
            let base = (!a
                .path
                .as_ref()
                .is_some_and(|p| p.ends_with(transform::ELEM)))
            .then(|| Node {
                pred: transform::ATTR_BASE.into(),
                ..a_as_attr.clone()
            });
            let targets: Vec<Node> = unifying(&self.defs, &a_as_attr)
                .chain(base.iter().flat_map(|b| unifying(&self.defs, b)))
                .cloned()
                .collect();
            for t in &targets {
                self.edges.push(Edge {
                    from: a.clone(),
                    to: t.clone(),
                    negative: true,
                    rule: None,
                    why: "attribute aggregate (lub_ranked)".into(),
                });
            }
        }
        args
    }

    /// Rule edges: from every definition node a body's literal unifies
    /// with, to every head of its rule.
    fn rule_edges(&mut self) {
        let (opts, split) = (self.opts, &self.opts.split);
        for (i, r) in self.rules.iter().enumerate() {
            let agg_head = is_aggregate_head(&r.head);
            for (k, lit) in r.body.iter().enumerate() {
                let (atom, negated) = match lit {
                    Lit::Pos(a) => (a, false),
                    Lit::Not(a) => (a, true),
                    _ => continue,
                };
                if is_builtin_or_edb(&atom.pred) {
                    continue;
                }
                let pat = addressed(body_pattern(atom), atom, &r.body, split);
                let reads_aggregate = pat.pred == "attr" || pat.pred == transform::ATTR_BASE;
                let is_extern = opts.externs.contains(&atom.pred);
                let negative = negated || agg_head || reads_aggregate || is_extern;
                let why = if negated {
                    "not"
                } else if agg_head {
                    "aggregate head"
                } else if reads_aggregate {
                    "reads attr (aggregate)"
                } else if is_extern {
                    "extern"
                } else {
                    "positive"
                };
                // Connect from every definition node the pattern unifies with,
                // to every head; the read a head takes its address from, to
                // the head at that address.
                let mut matched = false;
                for d in unifying(&self.defs, &pat) {
                    matched = true;
                    self.read[i].push(d.clone());
                    for head in &self.heads[i] {
                        if self.sources[i] == Some(k) && head.addr != d.addr {
                            continue;
                        }
                        self.edges.push(Edge {
                            from: d.clone(),
                            to: head.clone(),
                            negative,
                            rule: Some(i),
                            why: why.into(),
                        });
                    }
                }
                if !matched {
                    // Undefined predicate (or EDB with no facts): a node with no
                    // definition. E makes this a compile error; we record it as a
                    // plain node so the graph still stratifies.
                    self.nodes.insert(pat.clone());
                    self.read[i].push(pat.clone());
                    for head in &self.heads[i] {
                        self.edges.push(Edge {
                            from: pat.clone(),
                            to: head.clone(),
                            negative,
                            rule: Some(i),
                            why: format!("{why} (undefined)"),
                        });
                    }
                }
            }
        }
    }

    /// stuck/4 is derived above every rule that can stick: its instances
    /// are those of the rule's stuck companion, which reads the rule's body
    /// (E §2.7), so an edge from every node such a body reads; and above
    /// every contribution partition (`args`), whose groups the aggregate
    /// can leave stuck. Negative: a reader sees every instance. A reader
    /// whose head feeds a rule that can stick is on a negative cycle, an
    /// error.
    fn stuck_edges(&mut self, args: Vec<Node>) {
        let stuck = Node::plain(STUCK);
        let aggregates: BTreeSet<String> = self
            .rules
            .iter()
            .filter(|r| is_aggregate_head(&r.head))
            .map(|r| r.head.pred.clone())
            .chain(["attr".to_string(), transform::ATTR_BASE.to_string()])
            .collect();
        for (i, r) in self.rules.iter().enumerate() {
            if !crate::stuck::can_stick(&r.head, &r.body, &aggregates) {
                continue;
            }
            let from: BTreeSet<&Node> = self.read[i].iter().collect();
            for d in from {
                self.edges.push(Edge {
                    from: d.clone(),
                    to: stuck.clone(),
                    negative: true,
                    rule: Some(i),
                    why: "can stick (stuck/4)".into(),
                });
            }
        }
        for a in args {
            self.edges.push(Edge {
                from: a,
                to: stuck.clone(),
                negative: true,
                rule: None,
                why: "attribute aggregate can stick (stuck/4)".into(),
            });
        }
    }
}

/// The evaluator's companion relation of stuck rule instances.
pub const STUCK: &str = "stuck";

/// Does the rule read `stuck/4`?
fn reads_stuck(r: &RuleStmt) -> bool {
    r.body.iter().any(|l| match l {
        Lit::Pos(a) | Lit::Not(a) => a.pred == STUCK,
        _ => false,
    })
}

/// The nodes of `defs` that unify with `pat`, in order. Nodes sort by
/// predicate, then type (`*` first), then path, so the candidates are the
/// `*`-typed run and the run of `pat`'s type.
fn unifying<'a>(defs: &'a BTreeSet<Node>, pat: &'a Node) -> impl Iterator<Item = &'a Node> {
    let run = move |typ: Option<String>| {
        let from = Node {
            pred: pat.pred.clone(),
            typ: typ.clone(),
            path: None,
            addr: None,
        };
        defs.range(from..)
            .take_while(move |n| n.pred == pat.pred && (typ.is_none() || n.typ == typ))
    };
    let typed: Box<dyn Iterator<Item = &'a Node>> = match &pat.typ {
        // `*`: every node of the predicate.
        None => Box::new(run(None)),
        Some(t) => Box::new(
            run(None)
                .take_while(|n| n.typ.is_none())
                .chain(run(Some(t.clone()))),
        ),
    };
    typed.filter(move |n| n.unifies(pat))
}

/// Tarjan's strongly connected components: `comp[v]` is v's component.
struct Tarjan<'a> {
    adj: &'a [Vec<usize>],
    index: usize,
    st: Vec<usize>,
    on: Vec<bool>,
    ix: Vec<Option<usize>>,
    low: Vec<usize>,
    comp: Vec<usize>,
    ncomp: usize,
}

impl Tarjan<'_> {
    fn sccs(adj: &[Vec<usize>]) -> Vec<usize> {
        let n = adj.len();
        let mut t = Tarjan {
            adj,
            index: 0,
            st: vec![],
            on: vec![false; n],
            ix: vec![None; n],
            low: vec![0; n],
            comp: vec![usize::MAX; n],
            ncomp: 0,
        };
        for v in 0..n {
            if t.ix[v].is_none() {
                t.dfs(v);
            }
        }
        t.comp
    }

    fn dfs(&mut self, v: usize) {
        self.ix[v] = Some(self.index);
        self.low[v] = self.index;
        self.index += 1;
        self.st.push(v);
        self.on[v] = true;
        for &w in &self.adj[v] {
            match self.ix[w] {
                None => {
                    self.dfs(w);
                    self.low[v] = self.low[v].min(self.low[w]);
                }
                Some(iw) if self.on[w] => self.low[v] = self.low[v].min(iw),
                Some(_) => {}
            }
        }
        if Some(self.low[v]) == self.ix[v] {
            loop {
                let w = self.st.pop().unwrap();
                self.on[w] = false;
                self.comp[w] = self.ncomp;
                if w == v {
                    break;
                }
            }
            self.ncomp += 1;
        }
    }
}

/// Tarjan SCC over the node graph; a negative edge inside an SCC is a
/// negative cycle.
pub fn stratify(g: &Graph) -> Verdict {
    let idx: BTreeMap<&Node, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let n = g.nodes.len();
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for e in &g.edges {
        let (Some(&a), Some(&b)) = (idx.get(&e.from), idx.get(&e.to)) else {
            continue;
        };
        adj[a].push(b);
    }
    let comp = Tarjan::sccs(&adj);
    let node_vec: Vec<&Node> = g.nodes.iter().collect();
    for e in &g.edges {
        if !e.negative {
            continue;
        }
        let (Some(&a), Some(&b)) = (idx.get(&e.from), idx.get(&e.to)) else {
            continue;
        };
        if comp[a] == comp[b] {
            let c = comp[a];
            let scc: BTreeSet<Node> = (0..n)
                .filter(|&i| comp[i] == c)
                .map(|i| node_vec[i].clone())
                .collect();
            let negative_edges: Vec<Edge> = g
                .edges
                .iter()
                .filter(|e| e.negative && scc.contains(&e.from) && scc.contains(&e.to))
                .cloned()
                .collect();
            return Verdict::Rejected {
                scc,
                negative_edges,
            };
        }
    }
    // Strata by relaxation (no negative cycle, so it converges).
    let mut strata: BTreeMap<Node, usize> = g.nodes.iter().map(|n| (n.clone(), 0)).collect();
    loop {
        let mut changed = false;
        for e in &g.edges {
            let req = strata.get(&e.from).copied().unwrap_or(0) + if e.negative { 1 } else { 0 };
            let cur = strata.get(&e.to).copied().unwrap_or(0);
            if cur < req {
                strata.insert(e.to.clone(), req);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Verdict::Stratified { strata }
}

pub fn report(name: &str, g: &Graph, v: &Verdict) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "== {name}: {} nodes, {} edges ({} negative)\n",
        g.nodes.len(),
        g.edges.len(),
        g.edges.iter().filter(|e| e.negative).count()
    ));
    match v {
        Verdict::Stratified { strata } => {
            let max = strata.values().copied().max().unwrap_or(0);
            out.push_str(&format!("   STRATIFIED, {} strata\n", max + 1));
            let mut by: BTreeMap<usize, Vec<String>> = BTreeMap::new();
            for (n, s) in strata {
                by.entry(*s).or_default().push(n.to_string());
            }
            for (s, ns) in by {
                out.push_str(&format!("   stratum {s}: {}\n", ns.join("  ")));
            }
        }
        Verdict::Rejected {
            scc,
            negative_edges,
        } => {
            out.push_str(&format!(
                "   REJECTED: negative cycle through {} nodes\n",
                scc.len()
            ));
            out.push_str(&format!(
                "   scc: {}\n",
                scc.iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("  ")
            ));
            for e in negative_edges {
                let rule = match e.rule {
                    Some(i) => format!("rule #{i}: {}", spell::rule_short(&g.rules[i])),
                    None => "prelude".into(),
                };
                out.push_str(&format!(
                    "   {} -> {}  [{}]  {}\n",
                    e.from, e.to, e.why, rule
                ));
            }
        }
    }
    out
}

/// The compile error for a negative cycle: every node on it and every
/// negative edge inside it, each with the full text of the rule that makes it.
pub fn cycle_error(g: &Graph, scc: &BTreeSet<Node>, negative_edges: &[Edge]) -> String {
    let mut out = format!(
        "program is not stratifiable: negative cycle through {}",
        scc.iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for e in negative_edges {
        let rule = match e.rule {
            Some(i) => {
                let r = &g.rules[i];
                match crate::diag::place(r.head.span) {
                    Some(at) => format!("{} (at {at})", spell::rule(r)),
                    None => spell::rule(r),
                }
            }
            None => "(compiler-generated)".into(),
        };
        out.push_str(&format!("\n  {} -> {} [{}]: {}", e.from, e.to, e.why, rule));
    }
    out
}

/// Convenience: load, build, stratify, report.
pub fn run_file(
    name: &str,
    files: &[std::path::PathBuf],
    schema: &Schema,
) -> Result<(Verdict, String)> {
    let program = crate::loader::load_program(files)?;
    let g = build(&program, schema)?;
    let v = stratify(&g);
    let r = report(name, &g, &v);
    Ok((v, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
    }

    fn examples() -> Vec<(&'static str, PathBuf, Schema)> {
        vec![
            (
                "examples/demo/stacks/dform.df",
                root().join("examples/demo/stacks/dform.df"),
                crate::schema::fake(),
            ),
            (
                "examples/advanced/stacks/dform-advanced.df",
                root().join("examples/advanced/stacks/dform-advanced.df"),
                crate::schema::fake(),
            ),
            (
                "examples/pngu/stacks/pngu.df",
                root().join("examples/pngu/stacks/pngu.df"),
                crate::schema::gke(),
            ),
            (
                "examples/decl/stacks/decl_demo.df",
                root().join("examples/decl/stacks/decl_demo.df"),
                crate::schema::fake(),
            ),
            (
                "examples/adopt/stacks/adopt_demo.df",
                root().join("examples/adopt/stacks/adopt_demo.df"),
                crate::schema::fake(),
            ),
        ]
    }

    /// F's DR-12 revised: `want` is partitioned by constant type, so the
    /// cross-module wiring through `output(private_subnet_ids)` stratifies.
    #[test]
    fn dr12_revised_partitions_want_by_type() {
        let mut results = Vec::new();
        for (name, path, schema) in examples() {
            let (v, r) = run_file(name, &[path], &schema).unwrap();
            println!("{r}");
            results.push((name, matches!(v, Verdict::Stratified { .. })));
        }
        println!("DR-12 revised: {:?}", results);
        for (name, ok) in &results {
            assert!(ok, "{name} rejected under revised DR-12");
        }
        // F section 4.1 counted 11 strata for dform.df over the program
        // before the ref rewrite; the graph evaluation runs with also orders
        // every ref-holding contribution above the attribute it reads (see
        // below), and module inputs are cells of the aggregate too (phase 6):
        // 17; the settings are the stack config's table, read in two more:
        // 19; the modules' `iam_need` reaches the stack through an output
        // (R-5), a cell of the aggregate too, read in two more: 21; `let
        // cfg` is a cell too (R-3), its contribution and its aggregate two
        // more: 23. The database's and the cluster's subnets are a relation
        // the network exports (R-55), not an output cell read through two
        // more: 15. The settings are contributions to the inputs (R-38),
        // read by their names, and `let cfg` is gone: 13.
        let (v, _) = run_file(
            "examples/demo/stacks/dform.df",
            &[root().join("examples/demo/stacks/dform.df")],
            &crate::schema::fake(),
        )
        .unwrap();
        let Verdict::Stratified { strata } = v else {
            panic!()
        };
        assert_eq!(strata.values().max().copied().unwrap() + 1, 13);
    }

    /// The graph `dform dev strata` prints is the one evaluation runs with: a
    /// contribution that holds a reference, `ref(net.vpc, A, "")`, joins
    /// `(attr, net.vpc, id)` after the ref rewrite, so it sits in a higher
    /// stratum. The
    /// graph `dform dev strata` built from the program before the rewrite put
    /// the contribution in stratum 0 and the attribute in stratum 3.
    #[test]
    fn a_ref_holding_contribution_sits_above_the_attribute_it_reads() {
        let program =
            crate::loader::load_program(&[root().join("examples/demo/stacks/dform.df")]).unwrap();
        let g = build(&program, &crate::schema::fake()).unwrap();
        let Verdict::Stratified { strata } = stratify(&g) else {
            panic!()
        };
        let node = |pred: &str, t: &str, p: &str| Node {
            pred: pred.into(),
            typ: Some(t.into()),
            path: Some(p.into()),
            addr: None,
        };
        let peering = strata[&node("arg", "net.vpc_peering", "requester_vpc")];
        let vpc_id = strata[&node("attr", "net.vpc", "id")];
        assert!(peering > vpc_id, "{peering} <= {vpc_id}");
    }

    /// Adversarial programs, in the current syntax so they lower today.
    #[test]
    fn adversarial_programs_under_revised_dr12() {
        let dir = root().join("tests/fixtures/adversarial");
        let cases: Vec<(&str, bool)> = vec![
            ("adv3_pack_reads_other_path.df", true),
            ("adv3b_pack_reads_same_path.df", false),
            ("adv4_variable_path_writer.df", false),
            ("adv4b_variable_path_writer_no_read.df", true),
            ("adv7_mutual_recursion_attr.df", false),
            ("adv7b_mutual_via_different_paths.df", true),
            ("adv9_default_tag_unless_present.df", false),
        ];
        for (file, expect_ok) in cases {
            let (v, r) = run_file(file, &[dir.join(file)], &crate::schema::fake()).unwrap();
            println!("{r}");
            assert_eq!(matches!(v, Verdict::Stratified { .. }), expect_ok, "{file}");
        }
    }
}
