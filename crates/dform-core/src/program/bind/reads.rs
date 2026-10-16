//! What a term reads: each variable with the operator that reads it, a
//! name alone in an index the key of the row it reads (`T[e]`, `p[k]`,
//! `xs[i]`), a comprehension a clause inside the one around it.

use super::{Binder, Inner, Use, Way, written};
use crate::ast::Span;
use crate::program::node::*;

impl Binder<'_> {
    /// The variables term `e` reads into `way`, each with what reads it
    /// (`by`, for one written alone).
    pub(super) fn reads(&mut self, e: ExprId, by: &str, way: &mut Way, bodies: &mut Vec<Inner>) {
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Var(v) => self.read_var(*v, x.span, by, way),
            ExprKind::Interp { .. } => {
                for h in x.kind.children() {
                    self.reads(h, "in this interpolation", way, bodies);
                }
            }
            // What an aggregate folds is the clause's.
            ExprKind::Call {
                callee: Callee::Std(f),
                ..
            } if crate::partition::AGGREGATES.contains(&f.name.as_str()) => {}
            ExprKind::Call { callee, .. } => {
                let by = format!("in this call of `{}`", self.callee(callee));
                for a in x.kind.children() {
                    self.reads(a, &by, way, bodies);
                }
            }
            ExprKind::Aggregate { .. } => {}
            ExprKind::Binary { op, .. } => {
                let by = format!("at this `{}`", bin_text(*op));
                for a in x.kind.children() {
                    self.reads(a, &by, way, bodies);
                }
            }
            ExprKind::Neg(a) => self.reads(*a, "at this `-`", way, bodies),
            ExprKind::Comprehension { item, clause } => {
                bodies.push(Inner::Body(*clause, vec![*item]));
            }
            ExprKind::Object { parts } => {
                for p in parts.clone() {
                    match p {
                        ObjPart::Field { value: a, .. }
                        | ObjPart::Computed { key: a, .. }
                        | ObjPart::Spread(a) => self.reads(a, by, way, bodies),
                    }
                }
            }
            ExprKind::List { .. }
            | ExprKind::Range { .. }
            | ExprKind::As { .. }
            | ExprKind::Alias(_)
            | ExprKind::RefOf(_)
            | ExprKind::Address { .. }
            | ExprKind::Scoped { .. } => {
                for a in x.kind.children() {
                    self.reads(a, by, way, bodies);
                }
            }
            ExprKind::Missing
            | ExprKind::Hole
            | ExprKind::Lit(_)
            | ExprKind::Quantity { .. }
            | ExprKind::Value { .. }
            | ExprKind::Setting { .. }
            | ExprKind::Type(_) => {}
            ExprKind::Field { .. }
            | ExprKind::Resource { .. }
            | ExprKind::Lookup { .. }
            | ExprKind::Output { .. }
            | ExprKind::Deployed { .. }
            | ExprKind::World { .. } => self.chain_reads(e, by, way, bodies),
        }
    }

    /// A read of a chain: its head by what reads the chain, its indexes
    /// as indexes.
    fn chain_reads(&mut self, e: ExprId, by: &str, way: &mut Way, bodies: &mut Vec<Inner>) {
        match &self.program.exprs[e].kind {
            ExprKind::Field { base, path } => {
                let (base, path) = (*base, path.clone());
                self.reads(base, by, way, bodies);
                self.steps(&path, way, bodies);
            }
            // `x` of a typed variable, or `T[e]`.
            ExprKind::Resource { typ, addr } => {
                let addr = *addr;
                match typ.span.start == self.program.exprs[addr].span.start {
                    true => self.reads(addr, by, way, bodies),
                    false => self.index(addr, way, bodies),
                }
            }
            ExprKind::Lookup { args, path, .. } => {
                let (args, path) = (args.clone(), path.clone());
                for a in args {
                    self.index(a, way, bodies);
                }
                self.steps(&path, way, bodies);
            }
            // `n.k`, or `c[e].k`: a copy named by its key, an index.
            ExprKind::Output { copy, of, .. } => match of {
                Some(_) => self.index(*copy, way, bodies),
                None => self.reads(*copy, by, way, bodies),
            },
            ExprKind::Deployed { keys, .. } => {
                for (_, x, _) in keys.clone() {
                    self.index(x, way, bodies);
                }
            }
            ExprKind::World { addr, path, .. } => {
                let (addr, path) = (*addr, path.clone());
                self.index(addr, way, bodies);
                self.steps(&path, way, bodies);
            }
            _ => unreachable!("a chain"),
        }
    }

    /// An index: a name alone binds, as the key of the row it reads;
    /// anything else is read.
    fn index(&mut self, x: ExprId, way: &mut Way, bodies: &mut Vec<Inner>) {
        match &self.program.exprs[x].kind {
            ExprKind::Var(v) => self.binds_key(*v, way),
            _ => self.reads(x, "in this index", way, bodies),
        }
    }

    /// A path's indexes.
    fn steps(&mut self, path: &[Step], way: &mut Way, bodies: &mut Vec<Inner>) {
        for s in path {
            match s {
                Step::Index(x) | Step::Key(x) | Step::Keyed { key: x, .. } => {
                    self.index(*x, way, bodies)
                }
                Step::Field(..) | Step::Each(..) | Step::Len => {}
            }
        }
    }

    /// A read of `v` written at `at`: by what reads it when it stands
    /// alone, else by the read it heads (`x.p`, `x[i]`).
    pub(super) fn read_var(&self, v: VarId, at: Span, by: &str, way: &mut Way) {
        let v = &self.program.vars[v];
        if !self.counts(v) {
            return;
        }
        let by = match written(at) {
            Some(t) if t != v.name => format!("at this read `{t}`"),
            _ => by.to_string(),
        };
        let at = Span {
            end: at.start + v.name.len() as u32,
            ..at
        };
        let name = v.name.clone();
        way.needs.push(Use { name, at, by });
    }

    /// `v` bound by the goal.
    pub(super) fn binds(&self, v: VarId, way: &mut Way) {
        let v = &self.program.vars[v];
        if self.counts(v) {
            way.binds.push(v.name.clone());
        }
    }

    /// `v` bound by an index of the goal, before it.
    fn binds_key(&self, v: VarId, way: &mut Way) {
        let v = &self.program.vars[v];
        if self.counts(v) {
            way.keys.push(v.name.clone());
        }
    }

    /// The variable `name` a term written as that name alone stands for.
    pub(super) fn named(&self, e: ExprId, name: &str) -> Option<VarId> {
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Var(v) if self.program.vars[*v].name == name => Some(*v),
            _ => x
                .kind
                .children()
                .into_iter()
                .find_map(|c| self.named(c, name)),
        }
    }

    /// What a call is written as calling: `inet.subnet`, `f`.
    fn callee(&self, c: &Callee) -> String {
        match c {
            Callee::Std(f) => f.name.clone(),
            Callee::Let(item) => match &self.program.items[*item].kind {
                ItemKind::LetFn { name, .. } => name.clone(),
                _ => String::new(),
            },
            Callee::Extern(rel) => rel.name.clone(),
            Callee::Loader(name) | Callee::Data(name) => name.clone(),
        }
    }
}

fn bin_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}
