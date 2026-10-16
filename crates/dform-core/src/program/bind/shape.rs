//! What each kind of goal binds and needs: its ways (`=` binds either
//! side that is a pattern), the bodies inside it, and the names it waits
//! for.

use super::{Binder, DENSE, Inner, Mode, Shape, Use, Way, written};
use crate::ast::Span;
use crate::program::node::*;
use crate::value::Value;
use std::collections::BTreeSet;

/// A side of `=`, or a membership's element: a pattern, or a term in a
/// binding position.
#[derive(Clone, Copy)]
pub(super) enum Side {
    Pattern(PatternId),
    Expr(ExprId),
}

impl Binder<'_> {
    /// What goal `g` binds and needs.
    pub(super) fn shape(&mut self, g: GoalId) -> Shape {
        let goal = &self.program.goals[g];
        let mut shape = Shape::default();
        let mut way = Way::default();
        let mut bodies = Vec::new();
        match &goal.kind {
            GoalKind::Rel { rel, args } => self.rel_shape(rel, args, &mut way, &mut bodies),
            GoalKind::Truth(e) => self.reads(*e, "in this test", &mut way, &mut bodies),
            GoalKind::Has(h) => self.has_reads(h, &mut way, &mut bodies),
            GoalKind::Compare { lhs, ops } => {
                let first = ops.first().map_or(CmpOp::Eq, |(op, _)| *op);
                self.reads(*lhs, &cmp_by(first), &mut way, &mut bodies);
                for (op, x) in ops {
                    self.reads(*x, &cmp_by(*op), &mut way, &mut bodies);
                }
            }
            GoalKind::Bind { pat, value, eq } => return self.bind_shape(*pat, *value, *eq),
            GoalKind::Fold { var, eq, .. } => {
                self.binds(*var, &mut way);
                shape.eq = *eq;
            }
            GoalKind::Member { pat, coll } => self.member_shape(*pat, coll, &mut way, &mut bodies),
            GoalKind::Not {
                clause, written, ..
            } => return self.not_shape(*clause, *written),
            GoalKind::Marked { goal, .. } => return self.shape(*goal),
            GoalKind::Group { goals, after } => {
                for &g in goals.iter().chain(after) {
                    let s = self.shape(g);
                    shape.inner.extend(s.inner);
                    shape.waits.extend(s.waits);
                    if let Some(w) = s.ways.into_iter().next() {
                        way.binds.extend(w.binds);
                        way.keys.extend(w.keys);
                        way.needs.extend(w.needs);
                    }
                }
            }
        }
        self.shared(&bodies, &mut shape.waits);
        shape.inner.extend(bodies);
        shape.ways.push(way);
        shape
    }

    /// `not L`: what `L` reads is bound outside it, what it binds its own;
    /// `not { B }`: a clause inside this one; `x not in e`: every term read.
    fn not_shape(&mut self, clause: ClauseId, written: Negated) -> Shape {
        let mut shape = Shape::default();
        let goals = self.program.clauses[clause].goals.clone();
        let mut way = Way::default();
        match written {
            Negated::NotIn => {
                let mut bodies = Vec::new();
                for g in goals {
                    self.not_in_reads(g, &mut way, &mut bodies);
                }
                self.shared(&bodies, &mut shape.waits);
                shape.inner.extend(bodies);
                shape.ways.push(way);
                return shape;
            }
            Negated::Block => shape.inner.push(Inner::Body(clause, Vec::new())),
            Negated::Literal => {
                if let Some(&g) = goals.last() {
                    let s = self.shape(g);
                    let own: BTreeSet<&Name> = s.ways[0].bound().collect();
                    let needs = s.ways[0]
                        .needs
                        .iter()
                        .filter(|u| !own.contains(&u.name))
                        .map(|u| Use {
                            by: "at this `not`".to_string(),
                            ..u.clone()
                        })
                        .collect();
                    shape.inner.push(Inner::Not(needs));
                    shape.inner.extend(s.inner);
                }
            }
        }
        self.clause_names(clause, &mut shape.waits);
        shape.ways.push(way);
        shape
    }

    /// `x not in e`'s membership, each of its terms read.
    fn not_in_reads(&mut self, g: GoalId, way: &mut Way, bodies: &mut Vec<Inner>) {
        let by = "at this `not in`";
        match &self.program.goals[g].kind {
            GoalKind::Member { pat, coll } => {
                self.side_reads(element(*pat, coll), by, way, bodies);
                if let Some(e) = coll_expr(coll) {
                    self.reads(e, by, way, bodies);
                }
            }
            GoalKind::Group { goals, .. } => {
                for g in goals.clone() {
                    self.not_in_reads(g, way, bodies);
                }
            }
            GoalKind::Marked { goal, .. } => self.not_in_reads(*goal, way, bodies),
            GoalKind::Not { clause, .. } => {
                for g in self.program.clauses[*clause].goals.clone() {
                    self.not_in_reads(g, way, bodies);
                }
            }
            _ => {}
        }
    }

    /// `a = b`: a side that is a pattern binds, given what the other side
    /// reads, either side in the order written; with neither, both read.
    fn bind_shape(&mut self, pat: PatternId, value: ExprId, eq: Option<Span>) -> Shape {
        let mut shape = Shape::default();
        let mut bodies = Vec::new();
        let ps = self.program.patterns[pat].span;
        let vs = self.program.exprs[value].span;
        let sides = match ps.start <= vs.start {
            true => [Side::Pattern(pat), Side::Expr(value)],
            false => [Side::Expr(value), Side::Pattern(pat)],
        };
        for (p, v) in [(sides[0], sides[1]), (sides[1], sides[0])] {
            if !self.is_pattern(p) {
                continue;
            }
            let mut way = Way::default();
            self.side_binds(p, &mut way, &mut bodies);
            self.side_reads(v, "at this `=`", &mut way, &mut bodies);
            shape.ways.push(way);
        }
        if shape.ways.is_empty() {
            let mut way = Way::default();
            for s in sides {
                self.side_reads(s, "at this `=`", &mut way, &mut bodies);
            }
            shape.ways.push(way);
        }
        // `=` binds a name the other goals do not; a tuple or object
        // pattern may also compare (R-58).
        if !sides.iter().any(|s| self.is_structure(*s)) {
            shape.eq = eq;
        }
        self.shared(&bodies, &mut shape.waits);
        shape.inner.extend(bodies);
        shape
    }

    /// A relation's columns its mode leaves free bind; those it reads are
    /// read.
    fn rel_shape(&mut self, rel: &RelRef, args: &RelArgs, way: &mut Way, bodies: &mut Vec<Inner>) {
        let by = format!("in this call of `{}`", rel.name);
        let mode = Mode::of(&self.program.scopes, self.scope, rel);
        let pats: Vec<PatternId> = match args {
            RelArgs::Positional(ps) => ps.clone(),
            RelArgs::Record(fs) => fs.iter().map(|(_, p)| *p).collect(),
        };
        for (i, p) in pats.into_iter().enumerate() {
            match mode.reads(i) {
                true => self.pattern_reads(p, &by, way, bodies),
                false => self.pattern_binds(p, way, bodies),
            }
        }
    }

    /// `x in e`: its element binds, unless `e` is a range of a dense
    /// type, which has no next member (R-180); what `e` reads is read.
    fn member_shape(
        &mut self,
        pat: PatternId,
        coll: &Coll,
        way: &mut Way,
        bodies: &mut Vec<Inner>,
    ) {
        let elem = element(pat, coll);
        match coll_expr(coll).and_then(|e| self.dense(e)) {
            Some(of) => self.side_reads(elem, &format!("{DENSE} {of}"), way, bodies),
            None => self.side_binds(elem, way, bodies),
        }
        if let Some(e) = coll_expr(coll) {
            self.reads(e, "at this `in`", way, bodies);
        }
    }

    fn has_reads(&mut self, h: &Has, way: &mut Way, bodies: &mut Vec<Inner>) {
        let by = "at this `has`";
        match h {
            Has::Resource { typ, addr } => {
                self.reads(*typ, by, way, bodies);
                self.reads(*addr, by, way, bodies);
            }
            Has::Read(e) | Has::Walk { value: e, .. } => self.reads(*e, by, way, bodies),
        }
    }

    /// What a range literal is of when its type has no next member (a
    /// quantity, a float, a version, a time): `bytes`, `float`, with its
    /// ends' texts; none for an int's, an ip's, or what is computed.
    fn dense(&self, e: ExprId) -> Option<String> {
        let x = &self.program.exprs[e];
        let (ends, inclusive) = match &x.kind {
            ExprKind::Lit(Value::Range(r)) => ([end_of(&r.start)?, end_of(&r.end)?], r.inclusive),
            ExprKind::Range { lo, hi, inclusive } => {
                ([self.end_expr(*lo)?, self.end_expr(*hi)?], *inclusive)
            }
            _ => return None,
        };
        let op = if inclusive { "..=" } else { ".." };
        let text = written(x.span)?;
        let (a, b) = text.split_once(op)?;
        let texts = [a.trim(), b.trim()];
        let kinds = [ends[0].kind(texts[0])?, ends[1].kind(texts[1])?];
        let quantity = kinds.iter().find(|k| **k != "float").unwrap_or(&kinds[0]);
        Some(format!("{quantity} {} {op} {}", texts[0], texts[1]))
    }

    /// A range's end written as a literal: a quantity's or a string's.
    fn end_expr(&self, x: ExprId) -> Option<End> {
        match &self.program.exprs[x].kind {
            ExprKind::Quantity { .. } => Some(End::Quantity),
            ExprKind::Lit(v) => end_of(v),
            _ => None,
        }
    }

    /// Whether side `s` of `=` is a pattern: a tuple, an object, a
    /// literal, a name alone (a list is a value).
    fn is_pattern(&self, s: Side) -> bool {
        let e = match s {
            Side::Pattern(p) => {
                let pattern = &self.program.patterns[p];
                match &pattern.kind {
                    PatternKind::Hole | PatternKind::Bind(_) | PatternKind::Object { .. } => {
                        return true;
                    }
                    PatternKind::Tuple { .. } => return pattern.written == Written::Tuple,
                    PatternKind::Expr(e) => *e,
                }
            }
            Side::Expr(e) => e,
        };
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Lit(_)
            | ExprKind::Quantity { .. }
            | ExprKind::Interp { .. }
            | ExprKind::Object { .. } => true,
            _ => x.written != Written::Term,
        }
    }

    /// Whether side `s` is a tuple or an object.
    fn is_structure(&self, s: Side) -> bool {
        match s {
            Side::Pattern(p) => {
                let pattern = &self.program.patterns[p];
                match &pattern.kind {
                    PatternKind::Object { .. } => true,
                    PatternKind::Expr(e) => self.is_structure(Side::Expr(*e)),
                    _ => pattern.written == Written::Tuple,
                }
            }
            Side::Expr(e) => {
                let x = &self.program.exprs[e];
                matches!(x.kind, ExprKind::Object { .. }) || x.written == Written::Tuple
            }
        }
    }

    pub(super) fn side_binds(&mut self, s: Side, way: &mut Way, bodies: &mut Vec<Inner>) {
        match s {
            Side::Pattern(p) => self.pattern_binds(p, way, bodies),
            Side::Expr(e) => self.expr_binds(e, way, bodies),
        }
    }

    pub(super) fn side_reads(&mut self, s: Side, by: &str, way: &mut Way, bodies: &mut Vec<Inner>) {
        match s {
            Side::Pattern(p) => self.pattern_reads(p, by, way, bodies),
            Side::Expr(e) => self.reads(e, by, way, bodies),
        }
    }

    /// The names pattern `p` binds into `way`, and what its non-pattern
    /// parts read.
    fn pattern_binds(&mut self, p: PatternId, way: &mut Way, bodies: &mut Vec<Inner>) {
        let (parts, rest) = match &self.program.patterns[p].kind {
            PatternKind::Hole => return,
            PatternKind::Bind(v) => return self.binds(*v, way),
            PatternKind::Expr(e) => return self.expr_binds(*e, way, bodies),
            PatternKind::Tuple { elems, rest } => (elems.clone(), *rest),
            PatternKind::Object { fields, rest } => {
                (fields.iter().map(|(_, _, x)| *x).collect(), *rest)
            }
        };
        for x in parts {
            self.pattern_binds(x, way, bodies);
        }
        if let Some(v) = rest {
            self.binds(v, way);
        }
    }

    /// A term in a binding position: a name alone binds, a tuple or an
    /// object binds its parts, a literal nothing; anything else is read.
    fn expr_binds(&mut self, e: ExprId, way: &mut Way, bodies: &mut Vec<Inner>) {
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Var(v) if x.hoisted.is_none() => self.binds(*v, way),
            ExprKind::List { .. } | ExprKind::Object { .. } => {
                for c in x.kind.children() {
                    self.expr_binds(c, way, bodies);
                }
            }
            ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Hole => {}
            // A name alone read as more than a variable (a reference
            // column's `ref(T, R, "")`): its variable binds.
            _ => match &x.written {
                Written::Name(name) => {
                    if let Some(v) = self.named(e, name) {
                        self.binds(v, way);
                    }
                }
                _ => self.reads(e, "here", way, bodies),
            },
        }
    }

    /// A pattern read whole (a column its relation's mode reads, the
    /// element of a dense range): what each of its names reads.
    pub(super) fn pattern_reads(
        &mut self,
        p: PatternId,
        by: &str,
        way: &mut Way,
        bodies: &mut Vec<Inner>,
    ) {
        let pattern = &self.program.patterns[p];
        let span = pattern.span;
        let (parts, rest) = match &pattern.kind {
            PatternKind::Hole => return,
            PatternKind::Bind(v) => return self.read_var(*v, span, by, way),
            PatternKind::Expr(e) => return self.reads(*e, by, way, bodies),
            PatternKind::Tuple { elems, rest } => (elems.clone(), *rest),
            PatternKind::Object { fields, rest } => {
                (fields.iter().map(|(_, _, x)| *x).collect(), *rest)
            }
        };
        for x in parts {
            self.pattern_reads(x, by, way, bodies);
        }
        if let Some(v) = rest {
            self.read_var(v, span, by, way);
        }
    }
}

/// A range's end as its token is: a quantity (a number with a unit, a
/// decimal) or a string.
#[derive(Clone, Copy)]
enum End {
    Quantity,
    Str,
}

impl End {
    /// What a range of it is of, by its text: `bytes`, `float`, `semver`;
    /// none for a string no type orders densely.
    fn kind(self, text: &str) -> Option<&'static str> {
        match self {
            End::Quantity => Some(match crate::quantity::literal(text) {
                Ok(crate::quantity::Literal::Known(q)) => q.dim().name(),
                _ if text.contains('.') && !text.ends_with('m') => "float",
                _ => "quantity",
            }),
            End::Str => {
                let s = text.trim_matches('"');
                if crate::value::ipv4_to_u32(s).is_some() {
                    None
                } else if crate::value::Version::parse(s).is_ok() {
                    Some("semver")
                } else if crate::time::Time::parse(s).is_ok() {
                    Some("time")
                } else {
                    None
                }
            }
        }
    }
}

/// How a literal's value is written, for a range's end.
fn end_of(v: &Value) -> Option<End> {
    match v {
        Value::Quantity(_) | Value::Float(_) => Some(End::Quantity),
        Value::Str(_) => Some(End::Str),
        _ => None,
    }
}

/// A membership's element as written: a reference column's `r` of `r in
/// T`, else its pattern.
fn element(pat: PatternId, coll: &Coll) -> Side {
    match coll {
        Coll::TypeOf { of, .. } => Side::Expr(*of),
        _ => Side::Pattern(pat),
    }
}

/// The term a membership ranges over, when the program wrote one.
pub(super) fn coll_expr(c: &Coll) -> Option<ExprId> {
    match c {
        Coll::Expr(e) | Coll::Each(e) => Some(*e),
        Coll::Type(_)
        | Coll::Namespace { .. }
        | Coll::ProviderType { .. }
        | Coll::World(_)
        | Coll::Enum { .. }
        | Coll::Copies { .. }
        | Coll::TypeOf { .. } => None,
    }
}

/// What reads a comparison's operand: at this `<`.
fn cmp_by(op: CmpOp) -> String {
    let op = match op {
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    };
    format!("at this `{op}`")
}
