//! Reads in term position (R-211 steps 6b, 7c). The resolver builds a
//! read as its node where its term is ([`Builder::read_into`]): the term
//! holds the read, [`Expr::hoisted`] into the variable it binds, and
//! `lower` writes it as a literal before the one that holds the term.
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
//! | `k(__ref("T", V, ""))`                    | `Address` of the read           |
//! | `V = t`                                   | `t`'s node                      |
//!
//! A test the resolver makes before a read (`instance_of`) is the
//! read's when the read holds it ([`Reads::holds`]).

use super::Builder;
use crate::program::node::*;
use crate::program::scope::{DeclRef, Reach};
use crate::value::Value;

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

impl super::Reads {
    /// Whether a read built so far tests the copy in the scope `scope`.
    pub fn holds(&self, scope: ExprId) -> bool {
        self.held.contains(&scope)
    }
}

impl Builder<'_> {
    /// The read `read` in term position, read into the variable `v`: its
    /// node, built from what it reads (step 7), its parts the nodes of
    /// what resolved them.
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
                    rel: RelRef::own("attr", span),
                    args: vec![typ, addr, self.lit(Value::Str(attr.to_string()))],
                    out: 3,
                    path: Vec::new(),
                },
            },
            ReadOf::Output { copy, key, of } => {
                if let Some((_, scope)) = &of {
                    self.reads.held.insert(*scope);
                }
                ExprKind::Output {
                    of,
                    copy,
                    key: key.to_string(),
                }
            }
            ReadOf::World { typ, addr, path } => ExprKind::World {
                typ: TypeRef {
                    name: typ.to_string(),
                    span,
                },
                addr,
                path: self.fields(path),
            },
            ReadOf::Lookup { rel, args, out } => ExprKind::Lookup {
                rel: RelRef::own(rel, span),
                args,
                out,
                path: Vec::new(),
            },
        };
        self.expr_node(kind)
    }

    /// The address of the resource of type `typ` the input `rel`, read
    /// `via`, typed `ref(T)`, holds (R-101), read into `v`: `rel(__ref("T",
    /// V, ""))`, the address its user's.
    pub fn address_into(&mut self, (rel, via): (&str, Reach), typ: &str, v: &str) -> ExprId {
        let span = self.span;
        let of = self.expr_node(ExprKind::Lookup {
            rel: RelRef::reached(rel, via, span),
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

    /// A call of a let with parameters (or a relation read as one), `f(
    /// args..)` of the relation `rel` read `via`, read into `v`: the
    /// relation `f(args.., V)` (R-187).
    pub fn call_into(&mut self, (rel, via): (&str, Reach), args: Vec<ExprId>, v: &str) -> ExprId {
        let rel = RelRef::reached(rel, via, self.span);
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

    /// The read `id` in term position, read into `v`.
    fn hoist_into(&mut self, id: ExprId, v: &str) -> ExprId {
        let var = self.var(v);
        self.program.exprs[id].hoisted = Some(var);
        id
    }
}
