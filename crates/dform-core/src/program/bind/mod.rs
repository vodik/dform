//! Binding order (R-10, docs/grammar.md "Bodies"), a pass over a clause's
//! goals. What binds a variable is a pattern (the left of `=` with its
//! names unbound, the element of `in`, a relation's column its mode leaves
//! free, `mode.rs`) and an aggregate's `n = count(x)`; every other operand
//! (a comparison's, an arithmetic operator's, a function's argument, a
//! column its relation's mode reads, an attribute read, a `has` or a
//! `not`) needs its variables bound by some other goal of the clause that
//! does not itself depend on them, so the order the goals are written in
//! is irrelevant. The pass says the order the goals hold in, each after
//! what binds what it reads, otherwise as written, and what is wrong: an
//! unbound operand at it, `=` with both sides bound by other goals (`==`
//! is meant).
//!
//! The front end builds a statement's goals once, as written, asks the
//! pass, and puts the clause in the order it says
//! ([`crate::program::build::Gather::order`]); `lower` writes it so.
//!
//! The nodes keep what a term reads and how it is written where that
//! binds ([`Written`](super::node::Written): a name alone, a tuple; where
//! a binding's `=` stands); a message quotes the program's text at a
//! node's span (`at this read `x.p``, a range's ends).

mod mode;
mod names;
mod reads;
mod shape;

pub use mode::Mode;

use super::node::*;
use super::{Program, ScopeId};
use crate::ast::Span;
use crate::diag::Diagnostic;
use std::collections::BTreeSet;

/// What the pass reads besides the clause: the names bound around it,
/// and the scope its statement is written in.
pub struct Context<'a> {
    /// The source names an enclosing statement binds.
    pub outer: &'a BTreeSet<Name>,
    pub scope: ScopeId,
}

/// The order a clause's goals hold in, what it binds, and what is wrong
/// with it.
pub struct Order {
    /// Each goal's index in the clause, in the order they hold: each
    /// after what binds what it reads, the rest as written after them.
    pub order: Vec<usize>,
    /// The names some goal of the clause binds, and those each clause
    /// inside it binds (a comprehension's, a `not { }`'s): what a name
    /// the statement writes elsewhere is bound by.
    pub binds: BTreeSet<Name>,
    pub diags: Vec<Diagnostic>,
}

/// The order of `clause`'s goals.
pub fn order(program: &Program, clause: ClauseId, cx: &Context) -> Order {
    let mut b = Binder::new(program, cx.scope);
    let (_, order) = b.order(clause, cx.outer);
    Order {
        order,
        binds: b.binds,
        diags: b.diags,
    }
}

/// What a term outside the statement's clause reads and binds: a head's
/// column, a field's value, an aggregate's item.
#[derive(Default)]
pub struct TermReads {
    /// The names it reads.
    pub reads: Vec<Name>,
    /// The names it binds itself: an index's (`T[e]`, the key of the row
    /// it reads) and those a comprehension in it binds.
    pub binds: BTreeSet<Name>,
}

/// What the term `t` reads and binds, in `scope`.
pub fn term(program: &Program, scope: ScopeId, t: Term) -> TermReads {
    let mut b = Binder::new(program, scope);
    let mut way = Way::default();
    let mut bodies = Vec::new();
    match t {
        Term::Expr(e) => b.reads(e, "", &mut way, &mut bodies),
        Term::Pattern(p) => b.pattern_reads(p, "", &mut way, &mut bodies),
    }
    b.inside(&bodies);
    let mut binds = b.binds;
    binds.extend(way.keys);
    if let Term::Expr(e) = t {
        hoisted(program, e, &mut binds);
    }
    TermReads {
        reads: way.needs.into_iter().map(|u| u.name).collect(),
        binds,
    }
}

/// The names the reads in term `e` are read into that the program
/// wrote: a `set` target's `[_]`, each value its read gives.
fn hoisted(program: &Program, e: ExprId, out: &mut BTreeSet<Name>) {
    let x = &program.exprs[e];
    if let Some(v) = x.hoisted.map(|v| &program.vars[v])
        && !v.implicit
    {
        out.insert(v.name.clone());
    }
    for c in x.kind.children() {
        hoisted(program, c, out);
    }
}

/// The names goal `g` binds, written in `scope`, the compiler's
/// variables' too: what any way it holds binds (an index's key among
/// them), not what the clauses inside it bind.
pub fn binds(program: &Program, scope: ScopeId, g: GoalId) -> BTreeSet<Name> {
    let shape = Binder::with_implicit(program, scope).shape(g);
    shape.ways.iter().flat_map(Way::bound).cloned().collect()
}

/// The clauses inside goal `g` other than a negation's own: a
/// comprehension's body, in any of its terms.
pub fn bodies(program: &Program, scope: ScopeId, g: GoalId) -> Vec<ClauseId> {
    let shape = Binder::new(program, scope).shape(g);
    shape
        .inner
        .iter()
        .filter_map(|i| match i {
            Inner::Body(c, items) if !items.is_empty() => Some(*c),
            _ => None,
        })
        .collect()
}

/// The source names goal `g` writes, those of the clauses inside it
/// included.
pub fn goal_names(program: &Program, g: GoalId) -> BTreeSet<Name> {
    let b = Binder::new(program, ScopeId::default());
    let mut vars = BTreeSet::new();
    b.goal_vars(g, &mut vars);
    b.names(&vars).into_iter().collect()
}

/// The source names term `t` writes, a comprehension's included.
pub fn term_names(program: &Program, t: Term) -> BTreeSet<Name> {
    let b = Binder::new(program, ScopeId::default());
    let mut vars = BTreeSet::new();
    match t {
        Term::Expr(e) => b.expr_vars(e, &mut vars),
        Term::Pattern(p) => b.pattern_vars(p, &mut vars),
    }
    b.names(&vars).into_iter().collect()
}

/// The variables of goal `g`, the compiler's too, those of the clauses
/// inside it included, into `out`.
pub fn goal_vars(program: &Program, g: GoalId, out: &mut BTreeSet<VarId>) {
    Binder::with_implicit(program, ScopeId::default()).goal_vars(g, out);
}

/// The variables of clause `c`, the compiler's too, those of the
/// clauses inside it included.
pub fn clause_vars(program: &Program, c: ClauseId) -> BTreeSet<VarId> {
    let mut vars = BTreeSet::new();
    Binder::with_implicit(program, ScopeId::default()).clause_vars(c, &mut vars);
    vars
}

/// The variable term `t` is, written alone (none for a read of one).
pub fn alone(program: &Program, t: Term) -> Option<VarId> {
    let e = match t {
        Term::Pattern(p) => match &program.patterns[p].kind {
            PatternKind::Bind(v) => return Some(*v),
            PatternKind::Expr(e) => *e,
            _ => return None,
        },
        Term::Expr(e) => e,
    };
    let x = &program.exprs[e];
    match x.kind {
        ExprKind::Var(v) if x.hoisted.is_none() => Some(v),
        _ => None,
    }
}

/// A term of the program: a node of a value, or a pattern.
#[derive(Clone, Copy)]
pub enum Term {
    Expr(ExprId),
    Pattern(PatternId),
}

/// How a read under `in` over a dense range is said, the range's element
/// type after it.
const DENSE: &str = "at this `in` over a range of";

/// Where a variable is read: its name, where, and the operator that reads
/// it (`<`, `has`, a call of `f`, ..).
#[derive(Clone)]
struct Use {
    name: Name,
    at: Span,
    by: String,
}

/// One way a goal can hold: the names it binds once `needs` are bound.
#[derive(Clone, Default)]
struct Way {
    binds: Vec<Name>,
    /// Names an index binds (`T[e].p`, `xs[i]`): the read is made before
    /// the goal, so the goal may read them too.
    keys: Vec<Name>,
    needs: Vec<Use>,
}

impl Way {
    fn holds(&self, bound: &BTreeSet<Name>) -> bool {
        self.missing(bound).is_none()
    }

    fn missing<'m>(&'m self, bound: &BTreeSet<Name>) -> Option<&'m Use> {
        self.needs
            .iter()
            .find(|u| !bound.contains(&u.name) && !self.keys.contains(&u.name))
    }

    /// What it binds, its keys with it.
    fn bound(&self) -> impl Iterator<Item = &Name> {
        self.binds.iter().chain(&self.keys)
    }
}

/// A goal of the clause: its ways, the bodies inside it checked against
/// what the clause binds (`not { }`, a comprehension, a negated goal),
/// and for `x = t` where its `=` stands, which must bind.
#[derive(Default)]
struct Shape {
    ways: Vec<Way>,
    inner: Vec<Inner>,
    eq: Option<Span>,
    /// The names a negation or a comprehension in it shares with the
    /// clause: it holds once the clause binds them, whatever their order.
    waits: BTreeSet<Name>,
}

impl Shape {
    /// The way that holds once `bound` is, all of `waits` the clause binds
    /// (`known`) among it.
    fn holds(&self, bound: &BTreeSet<Name>, known: &BTreeSet<Name>) -> Option<&Way> {
        let m = self.ways.iter().find(|m| m.holds(bound))?;
        self.waits
            .iter()
            .all(|w| bound.contains(w) || !known.contains(w) || m.binds.contains(w))
            .then_some(m)
    }
}

enum Inner {
    /// A negated goal: what it reads must be bound outside it.
    Not(Vec<Use>),
    /// A clause of its own, with the item a comprehension builds of it.
    Body(ClauseId, Vec<ExprId>),
}

/// The pass over one statement's clauses.
struct Binder<'a> {
    program: &'a Program,
    /// The scope the statement is written in: where a relation's mode is
    /// looked up.
    scope: ScopeId,
    /// The names the clauses checked so far bind: what can bind them.
    binds: BTreeSet<Name>,
    /// The variables the compiler made count as the program's: what a
    /// negation's helper shares is any variable.
    implicit: bool,
    diags: Vec<Diagnostic>,
}

impl<'a> Binder<'a> {
    fn new(program: &'a Program, scope: ScopeId) -> Self {
        Binder {
            program,
            scope,
            binds: BTreeSet::new(),
            implicit: false,
            diags: Vec::new(),
        }
    }

    /// A pass that counts the variables the compiler made too.
    fn with_implicit(program: &'a Program, scope: ScopeId) -> Self {
        Binder {
            implicit: true,
            ..Binder::new(program, scope)
        }
    }

    /// Whether the pass counts `v`: a variable the program wrote, or any.
    fn counts(&self, v: &Var) -> bool {
        self.implicit || !v.implicit
    }
}

impl Binder<'_> {
    /// What `clause` binds from `outer`, and the order its goals hold in;
    /// what is wrong said, the clauses inside it checked after it.
    fn order(&mut self, clause: ClauseId, outer: &BTreeSet<Name>) -> (BTreeSet<Name>, Vec<usize>) {
        let goals = self.program.clauses[clause].goals.clone();
        let shapes: Vec<Shape> = goals.iter().map(|&g| self.shape(g)).collect();
        // What some goal can bind: a name nothing binds is unknown.
        let mut known = outer.clone();
        known.extend(
            shapes
                .iter()
                .flat_map(|s| &s.ways)
                .flat_map(Way::bound)
                .cloned(),
        );
        self.binds.extend(known.difference(outer).cloned());
        let (bound, order) = fixpoint(&shapes, outer, &known, None);
        let mut fired = vec![false; shapes.len()];
        for &i in &order {
            fired[i] = true;
        }
        let mut reported = BTreeSet::new();
        for (i, s) in shapes.iter().enumerate() {
            if !fired[i] {
                if let Some(u) = s.ways[0].missing(&bound)
                    && reported.insert(u.name.clone())
                {
                    self.unbound(u, &known);
                }
            } else if let Some(eq) = s.eq {
                self.both_bound(&shapes, i, eq, outer, &known);
            }
        }
        for inner in shapes.iter().flat_map(|s| &s.inner) {
            let missing = match inner {
                Inner::Not(needs) => needs.iter().find(|u| !bound.contains(&u.name)).cloned(),
                Inner::Body(clause, items) => self.inner(*clause, items, &bound),
            };
            if let Some(u) = missing
                && reported.insert(u.name.clone())
            {
                self.unbound(&u, &known);
            }
        }
        // The rest, as written, after what holds.
        let mut all = order;
        all.extend((0..shapes.len()).filter(|i| !fired[*i]));
        (bound, all)
    }

    /// The clauses among `bodies` checked on their own, outside any
    /// clause: what each binds.
    fn inside(&mut self, bodies: &[Inner]) {
        for inner in bodies {
            if let Inner::Body(clause, items) = inner {
                self.inner(*clause, items, &BTreeSet::new());
            }
        }
    }

    /// A clause inside the one being checked, under what it binds
    /// (`bound`): checked on its own, and what its items read that it
    /// leaves unbound.
    fn inner(&mut self, clause: ClauseId, items: &[ExprId], bound: &BTreeSet<Name>) -> Option<Use> {
        let (local, _) = self.order(clause, bound);
        let mut item = Way::default();
        for &t in items {
            self.reads(t, "in this item", &mut item, &mut Vec::new());
        }
        item.needs.into_iter().find(|u| !local.contains(&u.name))
    }

    /// `x = t` (the goal `i`, its `=` at `eq`) with both sides bound by
    /// the other goals: `==` is meant.
    fn both_bound(
        &mut self,
        shapes: &[Shape],
        i: usize,
        eq: Span,
        outer: &BTreeSet<Name>,
        known: &BTreeSet<Name>,
    ) {
        let s = &shapes[i];
        let (without, _) = fixpoint(shapes, outer, known, Some(i));
        if !s
            .ways
            .iter()
            .all(|m| m.binds.iter().all(|b| without.contains(b)))
        {
            return;
        }
        let names: BTreeSet<String> = s
            .ways
            .iter()
            .flat_map(|m| m.binds.iter().map(|b| format!("`{b}`")))
            .collect();
        let names: Vec<String> = names.into_iter().collect();
        let d = Diagnostic::error(eq, "both sides are bound; write `==`")
            .with_help(format!(
                "{} {} already: `==` compares; `=` gives a value to a name nothing else does",
                names.join(" and "),
                if names.len() == 1 {
                    "has its value"
                } else {
                    "have their values"
                }
            ))
            .with_fix("write `==`", vec![(eq, "==".to_string())]);
        self.diags.push(d);
    }

    /// The error at `u`, a read nothing binds before it; `known` what the
    /// clause binds somewhere, for a name it does not.
    fn unbound(&mut self, u: &Use, known: &BTreeSet<Name>) {
        let name = &u.name;
        let quoted = format!("\"{name}\"");
        let d = if let Some(of) = u.by.strip_prefix(DENSE) {
            dense_unbound(u.at, name, of.trim())
        } else if u.by == "at this `==`" {
            // `x == t` meant as a binding (R-10), or a string unquoted.
            Diagnostic::error(
                u.at,
                format!("`{name}` is unbound at this `==`; `=` binds, `==` compares"),
            )
            .with_help(format!(
                "bind it with `{name} = ..`, `in`, or a relation; a string is quoted: {quoted}"
            ))
        } else if !known.contains(name) {
            let near = known.iter().map(String::as_str);
            crate::syntax::resolve::unknown_name(u.at, name, &quoted, true, near)
        } else {
            Diagnostic::error(
                u.at,
                format!(
                    "`{name}` is unbound {}; bind it with `=`, `in`, or a relation first",
                    u.by
                ),
            )
        };
        self.diags.push(d);
    }
}

/// `n in 1Gi..=500Gi` with `n` unbound (R-180): the error, and the fix at
/// the site, ints scaled where the ends share a unit.
fn dense_unbound(span: Span, name: &str, of: &str) -> Diagnostic {
    let mut parts = of.split(' ');
    let mut part = || parts.next().unwrap_or_default();
    let (ty, a, op, b) = (part(), part(), part(), part());
    let unit = |t: &str| -> Option<(i64, String)> {
        let at = t.find(|c: char| !c.is_ascii_digit())?;
        Some((t[..at].parse().ok()?, t[at..].to_string()))
    };
    let i = if name == "i" { "k" } else { "i" };
    let help = match (unit(a), unit(b)) {
        (Some((x, u)), Some((y, w))) if u == w && !a.starts_with('"') => {
            format!("enumerate ints and scale them: `{i} in {x}{op}{y}, {name} = {i} * 1{u}`")
        }
        _ => format!("bind `{name}` first, and `{name} in {a}{op}{b}` tests it"),
    };
    Diagnostic::error(
        span,
        format!(
            "`{name}` is unbound at this `in`: a range of {ty} has no next member to give it, \
             so it tests a value and enumerates none"
        ),
    )
    .with_help(help)
}

/// What the clause binds from `outer`, and the goals that hold in the
/// order they hold: each time the first, as written, whose operands are
/// bound. `skip` leaves one goal out.
fn fixpoint(
    shapes: &[Shape],
    outer: &BTreeSet<Name>,
    known: &BTreeSet<Name>,
    skip: Option<usize>,
) -> (BTreeSet<Name>, Vec<usize>) {
    let mut bound = outer.clone();
    let mut fired = vec![false; shapes.len()];
    let mut order = Vec::new();
    loop {
        let next = shapes.iter().enumerate().find_map(|(i, s)| {
            if fired[i] || Some(i) == skip {
                return None;
            }
            s.holds(&bound, known).map(|m| (i, m))
        });
        let Some((i, m)) = next else {
            return (bound, order);
        };
        bound.extend(m.bound().cloned());
        fired[i] = true;
        order.push(i);
    }
}

/// The text `span` names in its source: what a message quotes.
fn written(span: Span) -> Option<String> {
    let (_, text) = crate::diag::source_of(span)?;
    text.get(span.start as usize..span.end as usize)
        .map(str::to_string)
}
