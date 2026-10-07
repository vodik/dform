//! `[_]` in a path (R-162): an anonymous binding that ranges over every
//! match, the placeholder `_` of an atom or a pattern written as a path
//! step.
//!
//! - In a read (a body, a value) `[_]` is already a step that enumerates:
//!   `T[_]` every resource of `T`, `l[_]` every element of a list, `o[_]`
//!   every value of an object (`has r.metadata.labels[_]`: some label).
//! - In a `set` target each `[_]` is a fresh variable and the statement is
//!   the clause form: `set k8s.stateful_set[_].spec.template.spec.
//!   containers[_].resources.limits = v` is `set c.resources.limits = v
//!   where w in k8s.stateful_set, c in w.spec.template.spec.containers`,
//!   so it fires per binding and over a list touches the elements there
//!   are. The variable of the `[_]` written at byte `at` is `Each_AT`,
//!   which no source name lowers to (`capitalise` drops an inner `_`), so
//!   `why` finds it by the step's place and prints it as `with
//!   containers[_] = {..}`.
//! - `v in PATH`, `PATH` holding a `[_]`, binds `v` to each value the path
//!   reaches: `img in k8s.deployment[_].spec.template.spec.containers[_].
//!   image` is each image (the `[_]` steps enumerate; the value at the end
//!   is not walked again).

use super::*;

/// The core variable of the `[_]` whose `[` is at byte `at` of its file.
pub fn each_var(at: u32) -> String {
    format!("Each_{at}")
}

/// Whether `op` is `[_]`.
pub(super) fn is_each(op: &Op) -> bool {
    matches!(op, Op::Index(ts, _) if ts.len() == 1
        && Chain::of(&ts[0]).is_some_and(|x| x.head == "_" && x.is_bare()))
}

impl Lowerer<'_> {
    /// `T[_]` on the right of `in` is `T`: every resource of it.
    pub(super) fn type_each(&self, rc: &Rc, mut c: Chain) -> Chain {
        if c.ops.last().is_some_and(is_each) {
            let mut t = c.clone();
            t.ops.pop();
            if self.chain_type(rc, &t).is_some() {
                c = t;
            }
        }
        c
    }

    /// `c` with every `[_]` a variable bound in `body` (the set target's
    /// form, above): the chain from the last `[_]` on, its head that
    /// variable. A `[_]` after a type binds a resource of it, one after a
    /// resource's list an element of the list, which `set` writes by its
    /// key (R-69).
    pub(super) fn each_target(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        body: &mut Vec<Lit>,
        span: Span,
    ) -> L<Chain> {
        let mut c = c.clone();
        while let Some(k) = c.ops.iter().position(is_each) {
            let Op::Index(_, r) = &c.ops[k] else {
                unreachable!("is_each")
            };
            let r = *r;
            let at = self.span_of(r);
            let prefix = Chain {
                ops: c.ops[..k].to_vec(),
                range: rowan::TextRange::new(c.range.start(), r.end()),
                ..c.clone()
            };
            let src = format!("_[_]@{}", u32::from(r.start()));
            let v = each_var(u32::from(r.start()));
            if let Some(t) = self.chain_type(rc, &prefix) {
                let typ = self.each_type(rc, &t, body, at);
                body.push(Lit::Pos(atom_at("want", vec![typ.clone(), var(&v)], at)));
                rc.types.insert(src.clone(), typ);
            } else {
                let res = self.resolve(rc, &prefix, body)?;
                let of = match &res {
                    Res::Ref { typ, addr, path } => path_string(path)
                        .filter(|p| !p.is_empty())
                        .map(|p| (typ.clone(), addr.clone(), p)),
                    _ => None,
                };
                let Some(of) = of else {
                    return self.error(
                        at,
                        "`[_]` in a `set` is every resource of a type (`T[_]`) or every element \
                         of a resource's list (`r.l[_]`), and the path before this one is neither",
                    );
                };
                let list = self.realize(rc, res, Pos::Content, body, span)?;
                body.push(Lit::Pos(atom_at("member", vec![list, var(&v)], at)));
                rc.elems.insert(src.clone(), of);
            }
            rc.reserved.insert(v.clone());
            rc.vars.insert(src.clone(), v);
            rc.first.insert(src.clone(), at);
            rc.binders.insert(src.clone());
            c = Chain {
                head: src,
                head_kind: IDENT,
                call: None,
                range: rowan::TextRange::new(r.start(), c.range.end()),
                ops: c.ops[k + 1..].to_vec(),
            };
        }
        Ok(c)
    }

    /// The type term of a resource of `t`: `t`, or with the provider also
    /// used under another name a variable over its names (R-115).
    fn each_type(&mut self, rc: &mut Rc, t: &str, body: &mut Vec<Lit>, span: Span) -> Term {
        let names = self.covering(t);
        if names.len() < 2 {
            return str_term(t);
        }
        let tv = var(&fresh(rc, "Type"));
        for each in names {
            self.helpers.push(Stmt::Fact(atom_at(
                membership::PROVIDER_TYPE,
                vec![str_term(t), str_term(&each)],
                span,
            )));
        }
        body.push(Lit::Pos(atom_at(
            membership::PROVIDER_TYPE,
            vec![str_term(t), tv.clone()],
            span,
        )));
        tv
    }
}
