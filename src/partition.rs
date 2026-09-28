//! The partition graph of proposal E §2.6 / DR-12 as revised by F, run over
//! the program as evaluation runs it ([`compile`]: lowered, computed refs
//! rewritten to `attr` reads, the computed prelude expanded, constraints as
//! `deny` rules). This is the evaluator's stratifier: `engine::eval`
//! evaluates strata in the order computed here and assigns every rule to
//! the stratum of its head node, and `dform strata` prints the same graph.
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

use crate::ast::{Atom, Constraint, Extern, Lit, Program, RuleStmt, Stmt, Term};
use crate::schema::Schema;
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
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.typ, &self.path) {
            (None, None) => write!(f, "{}", self.pred),
            (t, None) => write!(f, "({}, {})", self.pred, t.as_deref().unwrap_or("*")),
            (t, p) => write!(
                f,
                "({}, {}, {})",
                self.pred,
                t.as_deref().unwrap_or("*"),
                p.as_deref().unwrap_or("*")
            ),
        }
    }
}

impl Node {
    fn plain(pred: &str) -> Node {
        Node {
            pred: pred.into(),
            typ: None,
            path: None,
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
        ok(&self.typ, &other.typ) && ok(&self.path, &other.path)
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
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Predicates that are externs (negative edges into their readers).
    pub externs: BTreeSet<String>,
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

fn const_str(t: &Term) -> Option<String> {
    match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A's path normalization, approximated without a schema: a resource
/// attribute path is normalized to its first segment (the fake provider's
/// attributes are all top-level keys); a settings/output key is one declared
/// leaf per full key (E §7.1 declares `db.backup_days`, `audit.sinks` as
/// separate leaves of `type settings`). `transform::normalize_contribution`
/// is the same rule applied to a contribution's value.
pub fn normalize_path(typ: &Option<String>, path: &str) -> String {
    match typ.as_deref() {
        Some(transform::SETTINGS) | Some(transform::OUTPUT) => path.to_string(),
        _ => path.split('.').next().unwrap_or(path).to_string(),
    }
}

/// The `(pred, T, P)` node of an atom whose type is column 0 and path is
/// column 2.
fn type_path_node(pred: &str, atom: &Atom) -> Node {
    let typ = const_str(&atom.args[0]);
    let path = const_str(&atom.args[2]).map(|p| normalize_path(&typ, &p));
    Node {
        pred: pred.into(),
        typ,
        path,
    }
}

/// A contribution head `arg(T, A, P, V, Rank)` (the lowered core form):
/// its `(arg, T, P)` node and the `(attr, T, P)` aggregate it feeds.
fn contrib_node(atom: &Atom) -> Option<(Node, Node)> {
    if atom.pred != "arg" || atom.args.len() != 5 {
        return None;
    }
    Some((type_path_node("arg", atom), type_path_node("attr", atom)))
}

/// A read of the aggregate's outputs: `attr/4`, `attr_stuck/4`,
/// `attr_conflict/5` all read the `(attr, T, P)` node.
fn aggregate_read(atom: &Atom) -> Option<Node> {
    match (atom.pred.as_str(), atom.args.len()) {
        ("attr", 4) | ("attr_stuck", 4) | ("attr_conflict", 5) => {
            Some(type_path_node("attr", atom))
        }
        _ => None,
    }
}

/// Compiler-owned predicates whose first column is a resource type (F's
/// DR-12 revised). Each is one node per constant type.
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

/// The node a rule with this head defines.
pub fn head_node(atom: &Atom) -> Node {
    if let Some((arg, _)) = contrib_node(atom) {
        return arg;
    }
    if type_keyed(&atom.pred) && !atom.args.is_empty() {
        return Node {
            pred: atom.pred.clone(),
            typ: const_str(&atom.args[0]),
            path: None,
        };
    }
    Node::plain(&atom.pred)
}

/// The pattern a body literal reads.
fn body_pattern(atom: &Atom) -> Node {
    aggregate_read(atom).unwrap_or_else(|| head_node(atom))
}

fn is_builtin_or_edb(pred: &str) -> bool {
    matches!(
        pred,
        "member" | "enumerate" | "inet_overlaps" | "inet_contains" | "ip_unspecified"
    )
}

pub fn is_aggregate_head(head: &Atom) -> bool {
    head.args.iter().any(|t| match t {
        Term::Func { name, .. } => matches!(
            name.as_str(),
            "collect" | "collect_set" | "collect_list" | "count" | "sum" | "min" | "max"
        ),
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
    pub constraints: Vec<Constraint>,
    pub facts: Vec<Atom>,
    pub externs: BTreeSet<Extern>,
    pub schema: Schema,
    /// `rules` then `constraints` as `deny` rules, in that order.
    pub graph: Graph,
}

/// Compile `program` as `engine::eval` runs it and build its partition
/// graph: lower; read the provider schema from `given` (the facts given to
/// the run) and the program's own schema facts; reject a write to a
/// computed path; rewrite each `ref` to a computed path into an `attr` read
/// (with its dangling-ref deny); expand the computed prelude per schema row
/// (E §2.5, §4.3). The one place the graph is built: `dform strata` and
/// `dform graph strata` print exactly what evaluation stratifies with.
pub fn compile(program: &Program, given: &[Atom]) -> Result<Compiled> {
    let lowered = transform::lower(program)?;
    let mut rules: Vec<RuleStmt> = Vec::new();
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut facts: Vec<Atom> = Vec::new();
    for s in &lowered.program.statements {
        match s {
            Stmt::Rule(r) => rules.push(r.clone()),
            Stmt::Fact(a) => facts.push(a.clone()),
            Stmt::Constraint(c) => constraints.push(c.clone()),
            _ => {}
        }
    }
    let schema = schema_of(given, &facts)?;
    transform::check_computed_writes(&rules, &facts, &schema)?;
    let (mut rules, facts, constraints) =
        transform::rewrite_computed_refs(rules, facts, constraints, &schema);
    rules.extend(transform::computed_prelude(&schema));
    let opts = Options {
        externs: lowered.externs.iter().map(|e| e.pred.clone()).collect(),
    };
    let mut graph_rules = rules.clone();
    graph_rules.extend(constraints.iter().map(constraint_rule));
    let graph = build_lowered(graph_rules, &facts, &schema, &opts);
    Ok(Compiled {
        statements: lowered.program.statements,
        rules,
        constraints,
        facts,
        externs: lowered.externs,
        schema,
        graph,
    })
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

/// A constraint is a `deny` rule with a message head.
pub fn constraint_rule(c: &Constraint) -> RuleStmt {
    RuleStmt {
        head: Atom {
            pred: "deny".into(),
            args: vec![Term::Val(Value::Str(c.message.clone()))],
            record: None,
        },
        body: c.body.clone(),
    }
}

/// Build the graph over lowered rules and facts plus the schema's prelude.
/// `Graph::rules` is `rules` in the given order, so a rule's index in the
/// graph is its index in the caller's list.
pub fn build_lowered(
    rules: Vec<RuleStmt>,
    fact_atoms: &[Atom],
    schema: &Schema,
    opts: &Options,
) -> Graph {
    let mut nodes: BTreeSet<Node> = BTreeSet::new();
    let mut edges: Vec<Edge> = Vec::new();

    // Definition nodes: every head, every fact's predicate, every aggregate
    // node behind a contribution, and the prelude's computed contributions.
    let mut defs: BTreeSet<Node> = BTreeSet::new();
    for a in fact_atoms {
        if let Some((arg, attr)) = contrib_node(a) {
            defs.insert(arg);
            defs.insert(attr);
        } else {
            defs.insert(head_node(a));
        }
    }
    for r in &rules {
        let h = head_node(&r.head);
        defs.insert(h.clone());
        if let Some((_, attr)) = contrib_node(&r.head) {
            defs.insert(attr);
        }
    }
    // Prelude: (arg, T, P) :- (want, T) per minted schema row (computed and
    // optional_computed, E §2.5); expanded per row, P normalized.
    let mut prelude: Vec<(Node, Node)> = Vec::new();
    for (t, attr_name, _, _) in transform::minted_paths(schema) {
        let attr_name = normalize_path(&Some(t.clone()), &attr_name);
        {
            let arg = Node {
                pred: "arg".into(),
                typ: Some(t.clone()),
                path: Some(attr_name.clone()),
            };
            let attr = Node {
                pred: "attr".into(),
                typ: Some(t.clone()),
                path: Some(attr_name),
            };
            let want = Node {
                pred: "want".into(),
                typ: Some(t.clone()),
                path: None,
            };
            defs.insert(arg.clone());
            defs.insert(attr.clone());
            defs.insert(want.clone());
            prelude.push((want, arg));
        }
    }
    nodes.extend(defs.iter().cloned());

    // Aggregate edges: every (arg, T, P) definition feeds every (attr, T', P')
    // definition it unifies with, negatively.
    let args: Vec<Node> = defs.iter().filter(|n| n.pred == "arg").cloned().collect();
    let attrs: Vec<Node> = defs.iter().filter(|n| n.pred == "attr").cloned().collect();
    for a in &args {
        for t in &attrs {
            let a_as_attr = Node {
                pred: "attr".into(),
                typ: a.typ.clone(),
                path: a.path.clone(),
            };
            if a_as_attr.unifies(t) {
                edges.push(Edge {
                    from: a.clone(),
                    to: t.clone(),
                    negative: true,
                    rule: None,
                    why: "attribute aggregate (lub_ranked)".into(),
                });
            }
        }
    }
    for (want, arg) in prelude {
        edges.push(Edge {
            from: want,
            to: arg,
            negative: false,
            rule: None,
            why: "prelude: computed attribute null".into(),
        });
    }

    // Rule edges.
    for (i, r) in rules.iter().enumerate() {
        let head = head_node(&r.head);
        let agg_head = is_aggregate_head(&r.head);
        for lit in &r.body {
            let (atom, negated) = match lit {
                Lit::Pos(a) => (a, false),
                Lit::Not(a) => (a, true),
                _ => continue,
            };
            if is_builtin_or_edb(&atom.pred) {
                continue;
            }
            let pat = body_pattern(atom);
            let reads_aggregate = pat.pred == "attr";
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
            // Connect from every definition node the pattern unifies with.
            let mut matched = false;
            for d in defs.iter() {
                if d.unifies(&pat) {
                    matched = true;
                    edges.push(Edge {
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
                nodes.insert(pat.clone());
                edges.push(Edge {
                    from: pat.clone(),
                    to: head.clone(),
                    negative,
                    rule: Some(i),
                    why: format!("{why} (undefined)"),
                });
            }
        }
    }

    Graph {
        nodes,
        edges,
        rules,
    }
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

// ---------------------------------------------------------------------------
// Pretty printing (no spans exist; rule text is the diagnostic)
// ---------------------------------------------------------------------------

pub fn fmt_term(t: &Term) -> String {
    match t {
        Term::Val(v) => fmt_value(v),
        Term::Var(v) => v.clone(),
        Term::Wildcard => "_".into(),
        Term::Func { name, args } => format!(
            "{name}({})",
            args.iter().map(fmt_term).collect::<Vec<_>>().join(", ")
        ),
        Term::List(xs) => format!(
            "[{}]",
            xs.iter().map(fmt_term).collect::<Vec<_>>().join(", ")
        ),
        Term::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", fmt_term(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Term::ListComp { item, .. } => format!("[{} | ...]", fmt_term(item)),
    }
}

pub fn fmt_value(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("{s:?}"),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => format!(
            "[{}]",
            xs.iter().map(fmt_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", fmt_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Ip(n) => crate::value::u32_to_ipv4(*n),
        Value::IpNet { addr, prefix } => crate::value::ipnet_to_string(*addr, *prefix),
        Value::IpRange { start, end } => format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        ),
        Value::Ref { typ, name, attr } => format!("ref({typ}, {name}, {attr})"),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ}, {name}, {attr})"),
        Value::Null { label, class, .. } => format!("?{label}:{class:?}"),
    }
}

pub fn fmt_atom(a: &Atom) -> String {
    format!(
        "{}({})",
        a.pred,
        a.args.iter().map(fmt_term).collect::<Vec<_>>().join(", ")
    )
}

pub fn fmt_lit(l: &Lit) -> String {
    match l {
        Lit::Pos(a) => fmt_atom(a),
        Lit::Not(a) => format!("not {}", fmt_atom(a)),
        Lit::Eq(a, b) => format!("{} = {}", fmt_term(a), fmt_term(b)),
        Lit::Neq(a, b) => format!("{} != {}", fmt_term(a), fmt_term(b)),
        Lit::Gt(a, b) => format!("{} > {}", fmt_term(a), fmt_term(b)),
        Lit::Ge(a, b) => format!("{} >= {}", fmt_term(a), fmt_term(b)),
        Lit::Lt(a, b) => format!("{} < {}", fmt_term(a), fmt_term(b)),
        Lit::Le(a, b) => format!("{} <= {}", fmt_term(a), fmt_term(b)),
    }
}

pub fn fmt_rule(r: &RuleStmt) -> String {
    let body = r.body.iter().map(fmt_lit).collect::<Vec<_>>().join(", ");
    if body.is_empty() {
        fmt_atom(&r.head)
    } else {
        format!("{} :- {}", fmt_atom(&r.head), body)
    }
}

/// A short, cropped rule text for reports.
pub fn rule_short(r: &RuleStmt) -> String {
    let s = fmt_rule(r);
    if s.len() > 140 {
        format!("{}...", &s[..140])
    } else {
        s
    }
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
                    Some(i) => format!("rule #{i}: {}", rule_short(&g.rules[i])),
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
            Some(i) => fmt_rule(&g.rules[i]),
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
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn examples() -> Vec<(&'static str, PathBuf, Schema)> {
        vec![
            ("dform.df", root().join("dform.df"), crate::schema::fake()),
            (
                "dform-advanced.df",
                root().join("dform-advanced.df"),
                crate::schema::fake(),
            ),
            ("pngu.df", root().join("pngu.df"), crate::schema::gke()),
            (
                "examples/decl_demo.df",
                root().join("examples/decl_demo.df"),
                crate::schema::fake(),
            ),
            (
                "examples/adopt_demo.df",
                root().join("examples/adopt_demo.df"),
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
        // below): 13.
        let (v, _) = run_file(
            "dform.df",
            &[root().join("dform.df")],
            &crate::schema::fake(),
        )
        .unwrap();
        let Verdict::Stratified { strata } = v else {
            panic!()
        };
        assert_eq!(strata.values().max().copied().unwrap() + 1, 13);
    }

    /// The graph `dform strata` prints is the one evaluation runs with: a
    /// contribution that holds `ref(net.vpc, A, id)` reads `(attr, net.vpc,
    /// id)` after the ref rewrite, so it sits in a higher stratum. The
    /// graph `dform strata` built from the program before the rewrite put
    /// the contribution in stratum 0 and the attribute in stratum 3.
    #[test]
    fn a_ref_holding_contribution_sits_above_the_attribute_it_reads() {
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        let g = build(&program, &crate::schema::fake()).unwrap();
        let Verdict::Stratified { strata } = stratify(&g) else {
            panic!()
        };
        let node = |pred: &str, t: &str, p: &str| Node {
            pred: pred.into(),
            typ: Some(t.into()),
            path: Some(p.into()),
        };
        let peering = strata[&node("arg", "net.vpc_peering", "requester_vpc_id")];
        let vpc_id = strata[&node("attr", "net.vpc", "id")];
        assert!(peering > vpc_id, "{peering} <= {vpc_id}");
    }

    /// Adversarial programs, in the current syntax so they lower today.
    #[test]
    fn adversarial_programs_under_revised_dr12() {
        let dir = root().join("examples/adversarial");
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
