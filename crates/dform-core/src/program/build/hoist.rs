//! Reads in term position (R-211 steps 6b, 7c). The resolver hoists a
//! read out of the term that makes it, a literal before the one holding
//! the term, and builds the read as its node where its term is
//! ([`Builder::read_into`]): the term holds the read, [`Expr::hoisted`]
//! into the variable it binds, and `lower` writes it back as the literal
//! it was, before the literal that holds it.
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
//! | `V = t`                                   | `t`'s node                      |
//!
//! The resolver keeps each read's node by the variable it binds
//! ([`Reads`]): a builder given a literal the resolver lowered (a
//! comprehension's body outside a statement) takes the node of each read
//! it holds rather than remaking it ([`Builder::reading`]), and the
//! literals the resolver made before a goal it built that no node of the
//! goal holds are made of the nodes and the tests it kept
//! ([`Builder::made_goal`]).

use super::Builder;
use crate::ast::{Lit, Term};
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
            .map(|l| Pending {
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

    /// The variable a hoisted read binds: the variable of the node the
    /// resolver kept of it (whose own literal it is, the last its lowering
    /// writes), the left of `V = t`.
    fn binder(&self, l: &Lit) -> Option<String> {
        let key = written(l);
        let kept = self.reads.nodes.values().flatten().find(|&&id| {
            let (_, reads) = crate::program::lower_expr(self.program, id);
            reads.last().is_some_and(|r| written(r) == key)
        });
        if let Some(v) = kept.and_then(|&id| self.program.exprs[id].hoisted) {
            return Some(self.program.vars[v].lowered.clone());
        }
        match l {
            Lit::Eq(Term::Var(v), _) => Some(v.clone()),
            _ => None,
        }
    }

    /// The node of the use of `x`: the read that binds it when one is
    /// pending, taken.
    pub(super) fn pending_read(&mut self, x: &str) -> Option<ExprId> {
        let i = self
            .pending
            .iter()
            .position(|p| p.binds.as_deref() == Some(x))?;
        let Pending { lit, .. } = self.pending.remove(i);
        if let Some(id) = self.built_read(x, &lit) {
            self.took(id);
            return Some(id);
        }
        let Lit::Eq(_, t) = &lit else {
            unreachable!("a read the resolver kept, or `V = t`: {lit:?}")
        };
        let id = self.expr(t);
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
            Some(b.bind_goal(span, pat, value))
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
                let Some(at) = self
                    .pending
                    .iter()
                    .position(|p| p.binds.as_deref() == Some(v.as_str()))
                else {
                    continue;
                };
                if seen.insert(at) {
                    first = first.min(at);
                    walk.extend(self.pending[at].lit.terms().cloned());
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
