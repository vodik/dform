//! The operator IR rules compile to (E §2.6, DR-20): every body literal is
//! one operator with a DBSP counterpart, and a stratum is a `Fix` over its
//! rules. `engine::eval` interprets it semi-naively: per stratum, each rule
//! joins the tuples the last round derived (the delta) against the full
//! relations, once per body position, until a round derives nothing.
//!
//! Index selection is here too: a relation read whose columns are bound by
//! the literals before it (a constant, or a variable an earlier literal
//! binds) reads a hash index on those columns, chosen at compile time from
//! the body's binding pattern. The engine keeps every index up to date as
//! tuples are inserted.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::lattice::nulls_in;
use std::collections::{BTreeMap, BTreeSet};

/// A relation: a predicate at one arity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rel {
    pub pred: String,
    pub arity: usize,
}

impl Rel {
    pub fn of(a: &Atom) -> Rel {
        Rel {
            pred: a.pred.clone(),
            arity: a.args.len(),
        }
    }
}

/// The columns of a relation an index is keyed on, ascending.
pub type Key = Vec<usize>;

/// A relation read: the literal it is, and how its tuples are found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    /// Index of the literal in the rule body.
    pub lit: usize,
    pub rel: Rel,
    /// Columns bound before the literal, looked up in an index; empty for a
    /// scan of the whole relation.
    pub key: Key,
    /// The literal compares a column outside `key` (a function, a list or
    /// object pattern, a repeated variable): a tuple outside the index
    /// bucket can still make that comparison undecided (Rule 2), so tuples
    /// holding an open or secret null are read as well.
    pub loose: bool,
    /// A constant in the literal holds a null: no index can decide which
    /// tuples it compares undecided, so every tuple is read.
    pub scan_all: bool,
}

/// One operator of a rule body, in body order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Every tuple of a relation: the body's relation read with no column
    /// bound.
    Scan(Read),
    /// Extend each row with the tuples of a relation that agree with it on
    /// the bound columns (a hash-index lookup).
    Join(Read),
    /// A read of an `extern` relation: joined like any other, complete
    /// before the stratum that reads it (its edge is negative).
    Extern(Read),
    /// `not p(...)`: keep the rows no tuple matches. The whole tuple is
    /// bound, so it is an index lookup on every column.
    AntiJoin { lit: usize, rel: Rel },
    /// A builtin predicate, `!=` or an ordering comparison. Three outputs:
    /// the rows that pass, the rows that fail, and `Stuck`, the rows that
    /// need a null's content (Rule 2).
    Filter { lit: usize },
    /// A binding: `X = expr`, or `member`/`enumerate` expanding a list.
    Map { lit: usize },
}

impl Op {
    pub fn lit(&self) -> usize {
        match self {
            Op::Scan(r) | Op::Join(r) | Op::Extern(r) => r.lit,
            Op::AntiJoin { lit, .. } | Op::Filter { lit } | Op::Map { lit } => *lit,
        }
    }

    /// The relation read positively, if this operator reads one.
    pub fn read(&self) -> Option<&Read> {
        match self {
            Op::Scan(r) | Op::Join(r) | Op::Extern(r) => Some(r),
            _ => None,
        }
    }
}

/// What a rule does with a row that satisfies its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// Instantiate the head and insert it; a tuple already there is not new.
    Distinct,
    /// An aggregate head: group the rows by the other columns and fold
    /// column `col`.
    Agg { col: usize, kind: AggKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Set,
    List,
    Count,
}

/// A body compiled to operators.
#[derive(Debug, Clone)]
pub struct Body {
    pub ops: Vec<Op>,
}

impl Body {
    /// The positive relation reads, in body order: the positions a delta
    /// can enter at.
    pub fn reads(&self) -> impl Iterator<Item = &Read> {
        self.ops.iter().filter_map(Op::read)
    }

    /// The operator for body literal `lit`.
    pub fn op(&self, lit: usize) -> &Op {
        &self.ops[lit]
    }
}

/// One rule compiled.
#[derive(Debug, Clone)]
pub struct Rule {
    pub body: Body,
    pub head: Head,
}

/// A stratum: a `Fix` over its rules. `recursive` when a rule reads a
/// relation a rule of the same stratum derives, so a round can feed the
/// next.
#[derive(Debug, Clone, Default)]
pub struct Fix {
    pub rules: Vec<usize>,
    pub recursive: bool,
}

/// Compile a rule body. `externs` are the extern predicates.
pub fn compile_body(body: &[Lit], externs: &BTreeSet<String>) -> Body {
    let mut bound: BTreeSet<String> = BTreeSet::new();
    let mut ops = Vec::with_capacity(body.len());
    for (lit, l) in body.iter().enumerate() {
        let op = match l {
            Lit::Pos(a) if a.pred == "member" || a.pred == "enumerate" => {
                for t in a.args.iter().skip(1) {
                    bind_vars(t, &mut bound);
                }
                Op::Map { lit }
            }
            Lit::Pos(a) if is_builtin_pred(&a.pred) => Op::Filter { lit },
            Lit::Pos(a) => {
                let read = compile_read(lit, a, &mut bound);
                if externs.contains(&a.pred) {
                    Op::Extern(read)
                } else if read.key.is_empty() {
                    Op::Scan(read)
                } else {
                    Op::Join(read)
                }
            }
            Lit::Not(a) if a.pred == "member" || is_builtin_pred(&a.pred) => Op::Filter { lit },
            Lit::Not(a) => Op::AntiJoin {
                lit,
                rel: Rel::of(a),
            },
            Lit::Eq(a, b) => {
                // `=` binds a variable side; the other side must be ground.
                for t in [a, b] {
                    if let Term::Var(x) = t {
                        bound.insert(x.clone());
                    }
                }
                Op::Map { lit }
            }
            Lit::Neq(..) | Lit::Gt(..) | Lit::Ge(..) | Lit::Lt(..) | Lit::Le(..) => {
                Op::Filter { lit }
            }
        };
        ops.push(op);
    }
    Body { ops }
}

/// A positive relation read: the columns bound before it are its key.
fn compile_read(lit: usize, a: &Atom, bound: &mut BTreeSet<String>) -> Read {
    let mut key = Vec::new();
    let mut loose = false;
    let mut scan_all = false;
    // Variables this literal binds: a later occurrence in the same literal
    // compares against the tuple's own column.
    let mut here: BTreeSet<String> = BTreeSet::new();
    for (c, t) in a.args.iter().enumerate() {
        match t {
            Term::Val(v) => {
                if nulls_in(v).is_empty() {
                    key.push(c);
                } else {
                    scan_all = true;
                }
            }
            Term::Var(x) if bound.contains(x) => key.push(c),
            Term::Var(x) => {
                if !here.insert(x.clone()) {
                    loose = true;
                }
            }
            Term::Wildcard => {}
            other => {
                loose = true;
                bind_vars(other, &mut here);
            }
        }
    }
    bound.extend(here);
    Read {
        lit,
        rel: Rel::of(a),
        key,
        loose,
        scan_all,
    }
}

/// Every variable a pattern term binds when it unifies.
fn bind_vars(t: &Term, out: &mut BTreeSet<String>) {
    match t {
        Term::Var(x) => {
            out.insert(x.clone());
        }
        Term::List(xs) => xs.iter().for_each(|x| bind_vars(x, out)),
        Term::Obj(m) => m.values().for_each(|x| bind_vars(x, out)),
        // `scoped(Scope, Local)` as a pattern binds `Local`.
        Term::Func { name, args } if name == "scoped" && args.len() == 2 => {
            bind_vars(&args[1], out)
        }
        _ => {}
    }
}

/// Builtin predicates: functions to Bool, never relations.
pub fn is_builtin_pred(pred: &str) -> bool {
    matches!(pred, "inet_overlaps" | "inet_contains" | "ip_unspecified")
}

/// The aggregate term of a head, if it has one.
pub fn find_agg(head: &Atom) -> Option<(usize, AggKind)> {
    for (i, t) in head.args.iter().enumerate() {
        let Term::Func { name, args } = t else {
            continue;
        };
        if args.len() != 1 {
            continue;
        }
        match name.as_str() {
            // Back-compat: `collect(X)` is set-like.
            "collect" | "collect_set" => return Some((i, AggKind::Set)),
            "collect_list" => return Some((i, AggKind::List)),
            "count" => return Some((i, AggKind::Count)),
            _ => {}
        }
    }
    None
}

/// Compile a rule.
pub fn compile_rule(r: &RuleStmt, externs: &BTreeSet<String>) -> Rule {
    Rule {
        body: compile_body(&r.body, externs),
        head: match find_agg(&r.head) {
            Some((col, kind)) => Head::Agg { col, kind },
            None => Head::Distinct,
        },
    }
}

/// Group rules by stratum into `Fix` operators, in stratum order.
pub fn fixes(rules: &[RuleStmt], stratum_of: &[usize], compiled: &[Rule]) -> Vec<Fix> {
    let max = stratum_of.iter().copied().max().unwrap_or(0) + 1;
    let mut out: Vec<Fix> = vec![Fix::default(); max];
    for (i, s) in stratum_of.iter().enumerate() {
        out[*s].rules.push(i);
    }
    for fix in &mut out {
        let heads: BTreeSet<Rel> = fix.rules.iter().map(|i| Rel::of(&rules[*i].head)).collect();
        fix.recursive = fix
            .rules
            .iter()
            .any(|i| compiled[*i].body.reads().any(|r| heads.contains(&r.rel)));
    }
    out
}

/// Every index a set of bodies reads through, by relation.
pub fn indexes<'a>(bodies: impl IntoIterator<Item = &'a Body>) -> BTreeMap<Rel, BTreeSet<Key>> {
    let mut out: BTreeMap<Rel, BTreeSet<Key>> = BTreeMap::new();
    for b in bodies {
        for op in &b.ops {
            match op {
                Op::Scan(r) | Op::Join(r) | Op::Extern(r) if !r.key.is_empty() && !r.scan_all => {
                    out.entry(r.rel.clone()).or_default().insert(r.key.clone());
                }
                Op::AntiJoin { rel, .. } => {
                    out.entry(rel.clone())
                        .or_default()
                        .insert((0..rel.arity).collect());
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(src: &str) -> Body {
        let p = crate::parser::parse_program(src).unwrap();
        let r = p
            .statements
            .iter()
            .find_map(|s| match s {
                crate::ast::Stmt::Rule(r) => Some(r.clone()),
                _ => None,
            })
            .unwrap();
        compile_body(&r.body, &BTreeSet::new())
    }

    #[test]
    fn a_join_is_keyed_on_the_columns_bound_before_it() {
        let b = body("h(X, Z) :- p(X, Y), q(Y, \"k\", Z), not r(Z).");
        let reads: Vec<&Read> = b.reads().collect();
        assert_eq!(reads[0].key, Vec::<usize>::new());
        assert!(matches!(b.ops[0], Op::Scan(_)));
        assert_eq!(reads[1].key, vec![0, 1]);
        assert!(matches!(b.ops[1], Op::Join(_)));
        assert!(matches!(b.ops[2], Op::AntiJoin { .. }));
        let idx = indexes([&b]);
        assert_eq!(
            idx[&Rel {
                pred: "r".into(),
                arity: 1
            }],
            BTreeSet::from([vec![0]])
        );
    }

    #[test]
    fn an_equality_binds_and_a_repeated_variable_is_loose() {
        let b = body("h(X) :- Y = 1, p(Y, X, X).");
        assert!(matches!(b.ops[0], Op::Map { .. }));
        let r = b.reads().next().unwrap();
        assert_eq!(r.key, vec![0]);
        assert!(r.loose);
    }
}
