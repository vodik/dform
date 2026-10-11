//! A resolved chain as its node (R-211 step 7): what it reads and the
//! path into it, planned by the resolver as it names each read's
//! variable ([`Plan`]), then built as the read's node and the path's
//! steps over the nodes of what resolved it.

use crate::program::scope::{DeclRef, Reach};
use crate::program::{Builder, ExprId, build::Keyed, build::ReadOf};

/// A resolved chain, its reads' variables named.
pub(super) enum Plan {
    /// What resolved it: a variable, a constant, a read made already.
    Node(ExprId),
    /// A read in term position into the new variable `var`.
    Read { read: PlanRead, var: String },
    /// A typed output given as a value: the reference to the resource of
    /// type `typ` whose address it holds, `ref(T, V, "")` (R-43).
    RefTo { typ: String, addr: Box<Plan> },
    /// Fields and indexes into `base`.
    Path {
        base: Box<Plan>,
        steps: Vec<PlanStep>,
    },
}

/// What a read reads, its parts as resolved.
pub(super) enum PlanRead {
    Value(DeclRef, Reach),
    Attr {
        typ: ExprId,
        addr: ExprId,
        attr: String,
    },
    Output {
        copy: ExprId,
        key: String,
        of: Option<(String, ExprId)>,
    },
    World {
        typ: String,
        addr: ExprId,
        path: String,
    },
    Lookup {
        rel: String,
        args: Vec<ExprId>,
        out: usize,
    },
}

/// One step of a path into a value.
pub(super) enum PlanStep {
    /// `.a.b`: the stored path.
    Fields(String),
    /// `[i]`, the element read into `item`.
    Element { i: ExprId, item: String },
    /// `["k"]` of a keyed list (R-35), the element read into `item`.
    Keyed {
        key: ExprId,
        typ: ExprId,
        list: String,
        item: String,
        keys: String,
        field: String,
    },
}

impl Plan {
    /// The node the chain is, built by `b`.
    pub(super) fn build(&self, b: &mut Builder) -> ExprId {
        match self {
            Plan::Node(id) => *id,
            Plan::Read { read, var } => b.read_into(read.of(), var),
            Plan::RefTo { typ, addr } => {
                let addr = addr.build(b);
                b.reference(typ, addr)
            }
            Plan::Path { base, steps } => {
                let mut id = base.build(b);
                for s in steps {
                    id = match s {
                        PlanStep::Fields(p) => b.path(id, p),
                        PlanStep::Element { i, item } => b.element_at(id, *i, item),
                        PlanStep::Keyed {
                            key,
                            typ,
                            list,
                            item,
                            keys,
                            field,
                        } => {
                            let k = Keyed {
                                key: *key,
                                typ: *typ,
                                list: list.clone(),
                                keys,
                                field,
                            };
                            b.keyed_element(id, k, item)
                        }
                    };
                }
                id
            }
        }
    }
}

impl PlanRead {
    /// What it reads, as the builder takes it.
    pub(super) fn of(&self) -> ReadOf<'_> {
        match self {
            PlanRead::Value(decl, via) => ReadOf::Value(decl.clone(), via.clone()),
            PlanRead::Attr { typ, addr, attr } => ReadOf::Attr {
                typ: *typ,
                addr: *addr,
                attr,
            },
            PlanRead::Output { copy, key, of } => ReadOf::Output {
                copy: *copy,
                key,
                of: of.clone(),
            },
            PlanRead::World { typ, addr, path } => ReadOf::World {
                typ,
                addr: *addr,
                path,
            },
            PlanRead::Lookup { rel, args, out } => ReadOf::Lookup {
                rel,
                args: args.clone(),
                out: *out,
            },
        }
    }
}
