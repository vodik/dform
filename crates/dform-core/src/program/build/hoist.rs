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
//! | any other `p(.., V)`                      | `Lookup`, `V` its last column   |
//! | `V = t`                                   | `t`'s node                      |
//!
//! A read whose variable the terms do not use is given back: a literal's
//! builder makes it a goal before the literal's own.
//!
//! The resolver builds a read where its term is, from what it reads
//! ([`Builder::read_into`], R-211 step 7), and keeps the node by the
//! variable it binds: a builder given the literal that read lowered to
//! takes that node, with the reads it holds, rather than remaking it.

use super::Builder;
use crate::ast::{Atom, Lit, Term};
use crate::program::Read;
use crate::program::node::*;
use crate::program::scope::{DeclRef, Reach};
use crate::value::Value;
use std::collections::BTreeMap;

/// What a read reads, as the front end holds it, its parts their nodes:
/// its node is built from it ([`Builder::read_into`]).
pub enum ReadOf<'t> {
    /// `k(V)`.
    Value(DeclRef, Reach),
    /// `attr(T, A, "p", V)`.
    Attr {
        typ: ExprId,
        addr: ExprId,
        attr: &'t str,
    },
    /// `output(C, "k", V)`; `of`, the component and the scope of the test
    /// that `C` exists, made before it when it is named by its key.
    Output {
        copy: ExprId,
        key: &'t str,
        of: Option<(Name, ExprId)>,
    },
    /// `cloud_attr("T", A, "p", V)`.
    World {
        typ: &'t str,
        addr: ExprId,
        path: &'t str,
    },
    /// `p(a, .., V, ..)`, `V` the `out`th column.
    Lookup {
        rel: &'t str,
        args: Vec<ExprId>,
        out: usize,
    },
}

/// A keyed list's element by its key (R-35): `key`, the type `typ` whose
/// list `list` declares the key's field, the variables `keys` and
/// `field` it is read into.
pub struct Keyed<'t> {
    pub key: ExprId,
    pub typ: ExprId,
    pub list: String,
    pub keys: &'t str,
    pub field: &'t str,
}

/// A read hoisted before what is being built that no node holds yet:
/// where it was hoisted among the others, the variable it binds.
#[derive(Debug, Clone)]
pub(super) struct Pending {
    at: usize,
    binds: Option<String>,
    lit: Lit,
}

impl super::Reads {
    /// The resolver made the test `lit` (an `instance_of` before a copy's
    /// read by its key) as the goal `g`.
    pub fn test(&mut self, lit: &Lit, g: GoalId) {
        self.tests.push((written(lit), g));
    }
}

impl Builder<'_> {
    /// `f`, the reads `reads` hoisted before what it builds, each the node
    /// of the first use of the variable it binds; the reads whose
    /// variables `f` does not use, given back in order.
    pub fn reading<T>(&mut self, reads: &[Lit], f: impl FnOnce(&mut Self) -> T) -> (T, Vec<Lit>) {
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

    /// What the front end said the read `a` it hoisted reads, and the
    /// column its variable is in.
    pub(super) fn recorded(&self, a: &Atom) -> Option<(usize, Read)> {
        a.args.iter().enumerate().find_map(|(i, t)| {
            let Term::Var(v) = t else { return None };
            Some((
                i,
                self.reads.kinds.get(&(a.pred.clone(), v.clone()))?.clone(),
            ))
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
        if let Some(id) = self.built_read(x, &lit) {
            self.took(id);
            return Some(id);
        }
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

    /// The node built where its term is (step 7) of the read `lit` into
    /// `x`: one of those read into `x` that writes `lit`.
    fn built_read(&self, x: &str, lit: &Lit) -> Option<ExprId> {
        let lit = written(lit);
        self.reads.nodes.get(x)?.iter().copied().find(|&id| {
            let (_, reads) = crate::program::lower_expr(self.program, id);
            reads.iter().any(|r| written(r) == lit)
        })
    }

    /// The goal of `lit`, a literal the resolver made before the one that
    /// holds it, no term of that literal's holding it: a test it made
    /// ([`Reads::test`]), or a read whose node it kept, bound in place
    /// to its variable.
    pub(super) fn made_goal(&mut self, lit: &Lit) -> Option<GoalId> {
        let key = written(lit);
        if let Some(&(_, g)) = self.reads.tests.iter().find(|(k, _)| *k == key) {
            return Some(g);
        }
        let x = self.binder(lit)?;
        let id = self.built_read(&x, lit)?;
        let (_, reads) = crate::program::lower_expr(self.program, id);
        if reads.last().map(written) != Some(key) {
            return None;
        }
        let span = self.program.exprs[id].span;
        self.at(span, |b| {
            let value = b.copy_settled(id);
            let var = b.var(&x);
            let pat = b.pattern_node(PatternKind::Bind(var));
            Some(b.goal_node(span, GoalKind::Bind { pat, value }))
        })
    }

    /// The node `id`, built where its term is (step 7), holds the reads
    /// its lowering writes: taken from those pending.
    pub fn took(&mut self, id: ExprId) {
        let (_, reads) = crate::program::lower_expr(self.program, id);
        for r in reads {
            let r = written(&r);
            if let Some(i) = self.pending.iter().position(|p| written(&p.lit) == r) {
                self.pending.remove(i);
            }
        }
    }

    /// The read `read` in term position, read into the variable `v`: its
    /// node, built from what it reads (step 7), its parts the nodes of
    /// what resolved them. Kept for the literals that hold its read
    /// ([`Builder::pending_read`]).
    pub fn read_into(&mut self, read: ReadOf, v: &str) -> ExprId {
        let id = self.read_node(read);
        self.hoist_into(id, v)
    }

    /// The read `read` in place: what a literal tests or binds, its value
    /// column the literal's (`R.p == c`, `not R.ready`, `has n.k`).
    pub fn read_node(&mut self, read: ReadOf) -> ExprId {
        let span = self.span;
        let kind = match read {
            ReadOf::Value(decl, via) => ExprKind::Value { decl, via },
            ReadOf::Attr { typ, addr, attr } => match self.constant(typ).cloned() {
                Some(Value::Str(t)) => {
                    let typ = TypeRef {
                        name: t.clone(),
                        span,
                    };
                    let base = self.expr_node(ExprKind::Resource { typ, addr });
                    let path = vec![Step::Field(attr.to_string(), span)];
                    ExprKind::Field { base, path }
                }
                // A resource whose type is a variable: its relation's.
                _ => ExprKind::Lookup {
                    rel: RelRef {
                        name: "attr".to_string(),
                        span,
                    },
                    args: vec![typ, addr, self.lit(Value::Str(attr.to_string()))],
                    out: 3,
                    path: Vec::new(),
                },
            },
            ReadOf::Output { copy, key, of } => ExprKind::Output {
                of,
                copy,
                key: key.to_string(),
            },
            ReadOf::World { typ, addr, path } => ExprKind::World {
                typ: TypeRef {
                    name: typ.to_string(),
                    span,
                },
                addr,
                path: self.fields(path),
            },
            ReadOf::Lookup { rel, args, out } => ExprKind::Lookup {
                rel: RelRef {
                    name: rel.to_string(),
                    span,
                },
                args,
                out,
                path: Vec::new(),
            },
        };
        self.expr_node(kind)
    }

    /// The address of the resource of type `typ` the input `rel`, typed
    /// `ref(T)`, holds (R-101), read into `v`: `rel(__ref("T",
    /// __scope(V), ""))`.
    pub fn address_into(&mut self, rel: &str, typ: &str, v: &str) -> ExprId {
        let span = self.span;
        let of = self.expr_node(ExprKind::Lookup {
            rel: RelRef {
                name: rel.to_string(),
                span,
            },
            args: Vec::new(),
            out: 0,
            path: Vec::new(),
        });
        let typ = TypeRef {
            name: typ.to_string(),
            span,
        };
        let id = self.expr_node(ExprKind::Address { of, typ });
        self.hoist_into(id, v)
    }

    /// The term `id` read into a variable of its own, `v = t`: `id` itself,
    /// or, a read already, its own node of the read.
    pub fn bound_into(&mut self, id: ExprId, v: &str) -> ExprId {
        let id = match self.program.exprs[id].hoisted {
            Some(_) => self.expr_node(ExprKind::Alias(id)),
            None => id,
        };
        self.hoist_into(id, v)
    }

    /// The variable `v`'s node.
    pub fn variable(&mut self, v: &str) -> ExprId {
        let v = self.var(v);
        self.expr_node(ExprKind::Var(v))
    }

    /// The constant `v`'s node.
    pub fn lit(&mut self, v: Value) -> ExprId {
        self.expr_node(ExprKind::Lit(v))
    }

    /// A call of a let with parameters, `f(args..)`, read into `v`: the
    /// relation `f(args.., V)` (R-187).
    pub fn call_into(&mut self, rel: &str, args: Vec<ExprId>, v: &str) -> ExprId {
        let rel = RelRef {
            name: rel.to_string(),
            span: self.span,
        };
        let out = args.len();
        let kind = ExprKind::Lookup {
            rel,
            args,
            out,
            path: Vec::new(),
        };
        let id = self.expr_node(kind);
        self.hoist_into(id, v)
    }

    /// The element `i` of the list `base`, read into `v`: `member(L, i,
    /// V)`.
    pub fn element_at(&mut self, base: ExprId, i: ExprId, v: &str) -> ExprId {
        let path = vec![Step::Index(i)];
        let id = self.expr_node(ExprKind::Field { base, path });
        self.hoist_into(id, v)
    }

    /// The element `i` of the list `base`, in place: `member(L, i, P)`.
    pub fn element_in_place(&mut self, base: ExprId, i: ExprId) -> ExprId {
        let path = vec![Step::Index(i)];
        self.expr_node(ExprKind::Field { base, path })
    }

    /// The element of the keyed list `base` whose key is `key` (R-35), read
    /// into `v`: its key's field read off the type `typ`'s list `list`
    /// into `keys`, `[field]`.
    pub fn keyed_element(&mut self, base: ExprId, k: Keyed, v: &str) -> ExprId {
        let step = Step::Keyed {
            key: k.key,
            typ: k.typ,
            list: k.list,
            keys: self.var(k.keys),
            field: self.var(k.field),
        };
        let path = vec![step];
        let id = self.expr_node(ExprKind::Field { base, path });
        self.hoist_into(id, v)
    }

    /// `base.a.b`, the stored path `p` into it.
    pub fn path(&mut self, base: ExprId, p: &str) -> ExprId {
        let path = self.fields(p);
        self.expr_node(ExprKind::Field { base, path })
    }

    /// The reference to the resource of type `typ` at `addr`: `ref(T, A,
    /// "")`.
    pub fn reference(&mut self, typ: &str, addr: ExprId) -> ExprId {
        let typ = TypeRef {
            name: typ.to_string(),
            span: self.span,
        };
        let res = self.expr_node(ExprKind::Resource { typ, addr });
        self.expr_node(ExprKind::RefOf(res))
    }

    /// The read `id` in term position, read into `v`, kept for the
    /// literals that hold its read.
    fn hoist_into(&mut self, id: ExprId, v: &str) -> ExprId {
        let var = self.var(v);
        self.program.exprs[id].hoisted = Some(var);
        self.reads.nodes.entry(v.to_string()).or_default().push(id);
        id
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
            let out = a.args.len() - 1;
            let of = self.read(a, out, &Read::Lookup { out });
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
            _ => self.read(a, column, &Read::Lookup { out: column }),
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

/// A literal as written, at its place: two reads of one relation written
/// in two places are two.
fn written(l: &Lit) -> String {
    match l {
        Lit::Pos(a) | Lit::Not(a) => format!("{l:?}@{:?}", a.span),
        l => format!("{l:?}"),
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
