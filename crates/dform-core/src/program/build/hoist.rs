//! Reads in term position (R-211 step 6b). The resolver hoists a read
//! out of the term that makes it, a literal before the one holding the
//! term, and the term holds the variable it binds: `k(V)`, `attr(T, A,
//! "p", V)`, `member(L, i, V)`, `V = f(x)`. A builder of a term or a
//! literal is given the reads its terms hoisted ([`Builder::reading`]),
//! and the first use of a read's variable is the read itself, its node
//! [`Expr::hoisted`] into that variable; `lower` writes it back as the
//! literal it was, before the literal that holds it.
//!
//! | hoisted                                   | node, `hoisted` into `V`        |
//! |-------------------------------------------|---------------------------------|
//! | `k(V)`                                    | `Value`                         |
//! | `attr("T", A, "p", V)`                    | `Field` of a `Resource`         |
//! | `output(C, "k", V)`, after `instance_of(c, S, C)` | `Output`, `of` the test |
//! | `cloud_attr("T", A, "p", V)`              | `World`                         |
//! | `p(a, .., V, ..)` by its others           | `Lookup`                        |
//! | `member(L, i, V)`                         | `Field` of an `Index` step      |
//! | `member(L, V), type_list_key(..), ..`     | `Field` of a `Keyed` step       |
//! | `k(__ref("T", __scope(V), ""))`           | `Address` of the read           |
//! | any other `p(.., V)`                      | `Read`, `V` its last column     |
//! | `V = t`                                   | `t`'s node                      |
//!
//! A read whose variable the terms do not use is given back: a literal's
//! builder makes it a goal before the literal's own.

use super::Builder;
use crate::ast::{Atom, Lit, Term};
use crate::program::Read;
use crate::program::node::*;
use crate::value::Value;
use std::collections::BTreeMap;

/// A read hoisted before what is being built that no node holds yet:
/// where it was hoisted among the others, the variable it binds.
#[derive(Debug, Clone)]
pub(super) struct Pending {
    at: usize,
    binds: Option<String>,
    lit: Lit,
}

impl Builder<'_> {
    /// `f`, the reads `reads` hoisted before what it builds, each the node
    /// of the first use of the variable it binds; the reads whose
    /// variables `f` does not use, given back in order.
    pub(super) fn reading<T>(
        &mut self,
        reads: &[Lit],
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, Vec<Lit>) {
        let pending = reads
            .iter()
            .enumerate()
            .map(|(at, l)| Pending {
                at,
                binds: self.binder(l),
                lit: l.clone(),
            })
            .collect();
        let outer = std::mem::replace(&mut self.pending, pending);
        let out = f(self);
        let left = std::mem::replace(&mut self.pending, outer);
        (out, left.into_iter().map(|p| p.lit).collect())
    }

    /// `f` with no read pending: a comprehension's body, whose reads are
    /// its own.
    pub(super) fn sealed<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::take(&mut self.pending);
        let out = f(self);
        let inner = std::mem::replace(&mut self.pending, outer);
        debug_assert!(inner.is_empty(), "a sealed build leaves nothing pending");
        out
    }

    /// The variable a hoisted read binds: the column of a read the front
    /// end recorded, the address in a reference read, the last column of
    /// any other relation, the left of `V = t`.
    fn binder(&self, l: &Lit) -> Option<String> {
        match l {
            Lit::Pos(a) => {
                if let Some((at, _)) = self.recorded(a) {
                    return match &a.args[at] {
                        Term::Var(v) => Some(v.clone()),
                        _ => None,
                    };
                }
                match a.args.last() {
                    Some(Term::Var(v)) if a.record.is_none() => Some(v.clone()),
                    Some(t) => address(t).map(|(_, v)| v.to_string()),
                    None => None,
                }
            }
            Lit::Eq(Term::Var(v), _) => Some(v.clone()),
            _ => None,
        }
    }

    /// The read the front end recorded `a` as (`Program::reads`), and the
    /// column its variable is in.
    pub(super) fn recorded(&self, a: &Atom) -> Option<(usize, Read)> {
        a.args.iter().enumerate().find_map(|(i, t)| {
            let Term::Var(v) = t else { return None };
            let key = crate::program::read_key(a.span, &a.pred, v);
            Some((i, self.program.reads.get(&key)?.clone()))
        })
    }

    /// The node of the use of `x`: the read that binds it when one is
    /// pending, taken.
    pub(super) fn pending_read(&mut self, x: &str) -> Option<ExprId> {
        let i = self
            .pending
            .iter()
            .position(|p| p.binds.as_deref() == Some(x))?;
        let Pending { at, lit, .. } = self.pending.remove(i);
        let id = match &lit {
            Lit::Eq(_, t) => self.expr(t),
            Lit::Pos(a) => self.at(a.span, |b| b.read_in_term(a, at, x)),
            l => unreachable!("a read binds by an atom or `=`: {l:?}"),
        };
        // A read read again into `x`: its own node, of the read.
        let id = match self.program.exprs[id].hoisted {
            Some(_) => self.expr_node(ExprKind::Alias(id)),
            None => id,
        };
        let var = self.var(x);
        self.program.exprs[id].hoisted = Some(var);
        Some(id)
    }

    /// The read `a`, hoisted `at`, its variable `x`, as the node in term
    /// position.
    fn read_in_term(&mut self, a: &Atom, at: usize, x: &str) -> ExprId {
        if let Some((col, read)) = self.recorded(a)
            && let Some(kind) = self.read_kind(a, col, read)
        {
            return self.expr_node(kind);
        }
        if let Some(t) = a.args.last()
            && let Some((typ, _)) = address(t)
        {
            let typ = TypeRef {
                name: typ.to_string(),
                span: a.span,
            };
            let of = self.read(a, a.args.len() - 1);
            return self.expr_node(ExprKind::Address { of, typ });
        }
        let column = a
            .args
            .iter()
            .rposition(|t| matches!(t, Term::Var(v) if v == x))
            .expect("a read's variable is a column of it");
        match (a.pred.as_str(), a.args.as_slice(), column) {
            ("member", [list, i, _], 2) => {
                let base = self.expr(list);
                let path = vec![Step::Index(self.expr(i))];
                self.expr_node(ExprKind::Field { base, path })
            }
            ("member", [list, _], 1)
                if let Some((key, typ, list_path, keys, field)) = self.keyed(at, x) =>
            {
                let base = self.expr(list);
                let step = Step::Keyed {
                    key: self.expr(&key),
                    typ: self.expr(&typ),
                    list: list_path,
                    keys: self.var(&keys),
                    field: self.var(&field),
                };
                self.expr_node(ExprKind::Field {
                    base,
                    path: vec![step],
                })
            }
            _ => self.read(a, column),
        }
    }

    /// The element `x` of a keyed list by its key (R-35): the reads
    /// hoisted after `member(L, x)` at `at`, `type_list_key(T, "l", Keys),
    /// Keys = [Key], __path(x, Key) == k`, taken: `k`, `T`, `"l"`, `Keys`
    /// and `Key`.
    fn keyed(&mut self, at: usize, x: &str) -> Option<(Term, Term, String, String, String)> {
        let next = |b: &Self, n: usize| b.pending.iter().position(|p| p.at == at + n);
        let (i, j, k) = (next(self, 1)?, next(self, 2)?, next(self, 3)?);
        let (Lit::Pos(tk), Lit::Eq(Term::Var(keys), Term::List(one)), Lit::Eq(path, key)) = (
            &self.pending[i].lit,
            &self.pending[j].lit,
            &self.pending[k].lit,
        ) else {
            return None;
        };
        let ("type_list_key", [typ, Term::Val(Value::Str(list)), Term::Var(ks)]) =
            (tk.pred.as_str(), tk.args.as_slice())
        else {
            return None;
        };
        let [Term::Var(field)] = one.as_slice() else {
            return None;
        };
        let Term::Func { name, args } = path else {
            return None;
        };
        let [Term::Var(item), Term::Var(f)] = args.as_slice() else {
            return None;
        };
        if ks != keys || name != "__path" || item != x || f != field {
            return None;
        }
        let out = (
            key.clone(),
            typ.clone(),
            list.clone(),
            keys.clone(),
            field.clone(),
        );
        let mut taken = [i, j, k];
        taken.sort_unstable();
        for n in taken.into_iter().rev() {
            self.pending.remove(n);
        }
        Some(out)
    }

    /// The node of the read the front end recorded `a` as, its variable in
    /// column `at`; `None` for an attribute of a resource whose type is a
    /// variable (`x in T` over several types), which stays its relation.
    pub(super) fn read_kind(&mut self, a: &Atom, at: usize, read: Read) -> Option<ExprKind> {
        let str_at = |i: usize| match a.args.get(i) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        Some(match read {
            Read::Value(decl, via) if at == 0 => ExprKind::Value { decl, via },
            Read::Attr if at == 3 => {
                let (typ, p) = (str_at(0)?, str_at(2)?);
                let typ = TypeRef {
                    name: typ,
                    span: a.span,
                };
                let addr = self.expr(&a.args[1]);
                let base = self.expr_node(ExprKind::Resource { typ, addr });
                let path = vec![Step::Field(p, a.span)];
                ExprKind::Field { base, path }
            }
            Read::Output if at == 2 => ExprKind::Output {
                of: self.instance_test(&a.args[0]),
                copy: self.expr(&a.args[0]),
                key: str_at(1)?,
            },
            Read::World if at == 3 => {
                let (typ, p) = (str_at(0)?, str_at(2)?);
                ExprKind::World {
                    typ: TypeRef {
                        name: typ,
                        span: a.span,
                    },
                    addr: self.expr(&a.args[1]),
                    path: self.fields(&p),
                }
            }
            Read::Lookup { out } if out == at => {
                let args = a.args.iter().enumerate().filter(|(i, _)| *i != at);
                let args = args.map(|(_, t)| self.expr(t)).collect();
                let rel = RelRef {
                    name: a.pred.clone(),
                    span: a.span,
                };
                ExprKind::Lookup {
                    rel,
                    args,
                    out,
                    path: Vec::new(),
                }
            }
            _ => return None,
        })
    }

    /// The test that the copy `copy` exists, hoisted before its output is
    /// read by its key (`instance_of(C, S, copy)`, nothing bound): taken,
    /// the component and the scope it names.
    pub(super) fn instance_test(&mut self, copy: &Term) -> Option<(Name, ExprId)> {
        let at = self.pending.iter().rposition(|p| {
            p.binds.is_none()
                && matches!(&p.lit, Lit::Pos(t) if t.pred == crate::modules::INSTANCE_OF
                    && t.args.get(2) == Some(copy))
        })?;
        let Lit::Pos(t) = self.pending.remove(at).lit else {
            unreachable!("an atom")
        };
        let [Term::Val(Value::Str(of)), scope, _] = t.args.as_slice() else {
            return None;
        };
        Some((of.clone(), self.at(t.span, |b| b.expr(scope))))
    }

    /// `m`'s entries in the order the resolver read them: by the first
    /// read each one's term holds (a written object's fields hoist in the
    /// order written; the lowered term keeps them by name).
    pub(super) fn in_read_order<'t>(
        &self,
        m: &'t BTreeMap<String, Term>,
    ) -> Vec<(&'t String, &'t Term)> {
        let mut entries: Vec<(&String, &Term)> = m.iter().collect();
        if !self.pending.is_empty() {
            entries.sort_by_key(|(_, t)| self.first_read(t));
        }
        entries
    }

    /// The first pending read `t` holds, through the reads it holds: where
    /// it was hoisted, or none.
    fn first_read(&self, t: &Term) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        let mut walk = vec![t.clone()];
        let mut first = usize::MAX;
        while let Some(t) = walk.pop() {
            let mut vars = std::collections::BTreeSet::new();
            crate::syntax::resolve::lit_vars(&Lit::Eq(t.clone(), Term::Wildcard), &mut vars);
            for v in vars {
                let Some(p) = self
                    .pending
                    .iter()
                    .find(|p| p.binds.as_deref() == Some(v.as_str()))
                else {
                    continue;
                };
                if seen.insert(p.at) {
                    first = first.min(p.at);
                    walk.extend(p.lit.terms().cloned());
                }
            }
        }
        first
    }
}

/// `__ref("T", __scope(V), "")`: the reference a read of an input typed
/// `ref(T)` matches, binding its address (R-101): `T` and `V`.
fn address(t: &Term) -> Option<(&str, &str)> {
    let Term::Func { name, args } = t else {
        return None;
    };
    let [
        Term::Val(Value::Str(typ)),
        Term::Func {
            name: mark,
            args: v,
        },
        Term::Val(Value::Str(p)),
    ] = args.as_slice()
    else {
        return None;
    };
    let [Term::Var(v)] = v.as_slice() else {
        return None;
    };
    (name == crate::address::REF && mark == crate::modules::ABSOLUTE && p.is_empty())
        .then_some((typ.as_str(), v.as_str()))
}
