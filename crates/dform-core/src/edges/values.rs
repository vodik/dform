//! Where a value stands in the walk: a cell's values flow into it, a
//! function's arguments are its parameters', and a term is placed at its
//! node's path, a call by what it computes (a literal's node, a field
//! read, a typed value, a sum, a spread).

use super::shape::*;
use super::*;

impl Graph<'_> {
    /// `t` is one of the values of the cell `n` (a let's, an output's,
    /// an input's): a variable flows into it, one way; anything else is
    /// placed in it. A let's or an output's (`cell`) takes, besides, a
    /// variable's type one way (R-213) and a literal's kind (R-34).
    pub(super) fn give(&mut self, n: usize, t: &mut Term, at: &At, cell: bool) {
        let var = match t {
            Term::Var(v) => Some(v.clone()),
            Term::Func { name, args } if name == types::AS => match args.as_slice() {
                [Term::Var(v), _] => Some(v.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(v) = var {
            let x = self.var(&v);
            self.flows.push((x, n));
            if cell && matches!(t, Term::Var(_)) {
                self.typing.give(x, v, n, at.span);
            }
            return;
        }
        match cell {
            true => {
                self.held(n, t, at, at.span);
            }
            false => self.place(n, Vec::new(), t, at),
        }
    }

    /// A function's arguments: each its parameter's type (a `string`
    /// parameter takes a value as its text, R-133); a coeffect's none.
    /// A package function's typed parameter types its variable's column
    /// (R-34).
    pub(super) fn params(&mut self, f: &crate::functions::Function, args: &mut [Term], at: &At) {
        let package = f.name.contains('.') && !f.internal;
        for (i, t) in args.iter_mut().enumerate() {
            let p = f
                .params
                .get(i)
                .or_else(|| f.variadic.then(|| f.params.last()).flatten());
            let n = self.node();
            if let Some(p) = p {
                let ty = Ty::parse(&p.ty);
                // A coeffect's argument is the host's to read
                // (`oci.resolve`, `io.read`): its row is keyed by the
                // value as written.
                let reads = !f.coeffect && readable(&ty);
                let types = package && matches!(ty, Ty::Scalar(_)) && matches!(t, Term::Var(_));
                let role = match (reads, types) {
                    (true, true) => Some(Role::Both),
                    (true, false) => Some(Role::Reads),
                    (false, true) => Some(Role::Types {
                        prints: p.ty == "string",
                    }),
                    (false, false) => None,
                };
                if let Some(role) = role {
                    let what = format!("`{}`'s argument `{}`", f.name, p.name);
                    self.edge(n, Shape::Ty(ty), what, at.span, role);
                }
            }
            self.place(n, Vec::new(), t, at);
        }
    }

    /// The node a term's value is: a variable's, a field read's, or a
    /// fresh one it is placed in.
    pub(super) fn term_node(&mut self, t: &mut Term, at: &At) -> usize {
        match t {
            Term::Var(v) => self.var(v),
            _ => {
                let n = self.node();
                self.place(n, Vec::new(), t, at);
                n
            }
        }
    }

    /// `t` is `n`'s value at `path`.
    pub(super) fn place(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        match t {
            Term::Var(v) => {
                let x = self.var(v);
                self.link(n, path, x);
            }
            Term::Wildcard => {}
            Term::Obj(m) => {
                for (k, x) in m.iter_mut() {
                    // A keyed list's element names its key (`[key]`).
                    if k.starts_with('[') {
                        continue;
                    }
                    let mut p = path.clone();
                    p.push(Seg::Key(k.clone()));
                    self.place(n, p, x, at);
                }
            }
            Term::List(xs) => {
                let mut p = path;
                p.push(Seg::Elem);
                for x in xs {
                    self.place(n, p.clone(), x, at);
                }
            }
            Term::Val(Value::Obj(_) | Value::List(_)) => {
                let Term::Val(v) = std::mem::replace(t, Term::Wildcard) else {
                    unreachable!("matched")
                };
                let mut held = unfold(v);
                self.place(n, path, &mut held, at);
                *t = fold(held);
            }
            Term::Val(_) => self.site(n, path, t, at),
            Term::Func { .. } => self.call(n, path, t, at),
            // `[x | B]`: each `x` is an element.
            Term::ListComp { item, body } => {
                self.body(body, at);
                let mut p = path;
                p.push(Seg::Elem);
                self.place(n, p, item, at);
            }
        }
    }

    /// A call `t` is `n`'s value at `path`: a literal's node, a field
    /// read, a computed value read as its position's type, a sum, a
    /// spread, a literal `lower` writes as a call, a function's arguments.
    pub(super) fn call(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        let Term::Func { name, args } = t else {
            unreachable!("a call")
        };
        self.arithmetic(name, args, at);
        match (name.as_str(), args.as_mut_slice()) {
            // A literal's node ([`marked`]): its reading the node's.
            (NODE, [Term::Val(Value::Int(id)), held]) => {
                let before = self.readings.is_some().then(|| held.clone());
                self.place(n, path, held, at);
                if before.is_some_and(|b| b != *held) {
                    self.read.insert(node_id(*id), held.clone());
                }
            }
            // `x.p`: the part of `x` at `p`, a field of an object (R-185).
            ("__path", [Term::Var(v), Term::Val(Value::Str(p))]) => {
                let field = checks::Field {
                    node: self.var(v),
                    var: v.clone(),
                    path: p.clone(),
                    span: at.atom,
                };
                self.checks.fields.push(field);
                let (x, p) = (self.var(v), keys(p));
                let part = self.node();
                self.link(x, p, part);
                self.link(n, path, part);
            }
            // A computed value read as its position's type at run
            // time ([`types::at_run_time`]): the value is the
            // position's.
            (types::AS, [inner, _]) => self.place(n, path, inner, at),
            // `{ ..a, k: v }`, `[..a, x]`: the spread source and what
            // is written beside it are each the value's (R-192).
            ("add" | "sub", [a, b]) => self.sum(n, path, a, b, at),
            (crate::functions::MERGE | crate::functions::CONCAT, args) => {
                for a in args {
                    self.place(n, path.clone(), a, at);
                }
            }
            (types::AMBIGUOUS, _) => self.site(n, path, t, at),
            (name, _) if name == crate::range::LOWERED => self.site(n, path, t, at),
            (name, args) => {
                if let Some(f) = crate::functions::get(name)
                    && !f.internal
                {
                    self.params(f, args, at);
                } else {
                    for a in args {
                        let x = self.node();
                        self.place(x, Vec::new(), a, at);
                    }
                }
            }
        }
    }

    /// `a + b` and its kin (`name`, over `args`): checked once the columns
    /// are settled (arithmetic is on numbers, quantities and times).
    pub(super) fn arithmetic(&mut self, name: &str, args: &[Term], at: &At) {
        let (Some(op), [a, b]) = (lattice::arithmetic(name), args) else {
            return;
        };
        let sides = [self.side(unmarked(a), None), self.side(unmarked(b), None)];
        let span = at.atom;
        self.checks.checks.push(checks::Check { op, sides, span });
    }

    /// `a + b`, `a - b` at `n`'s `path`: each side takes the other's type
    /// and the result's (a duration beside a time).
    pub(super) fn sum(&mut self, n: usize, path: Path, a: &mut Term, b: &mut Term, at: &At) {
        let (x, y, r) = (self.term_node(a, at), self.term_node(b, at), self.node());
        self.link(n, path, r);
        for (side, other) in [(x, y), (y, x), (x, r), (y, r)] {
            let what = "the other side".to_string();
            self.edge(side, Shape::Like(other), what, at.span, Role::Reads);
        }
    }
}

/// A constant object or list as terms.
pub(super) fn unfold(v: Value) -> Term {
    match v {
        Value::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, unfold(v))).collect()),
        Value::List(xs) => Term::List(xs.into_iter().map(unfold).collect()),
        v => Term::Val(v),
    }
}

/// [`unfold`] back: still a constant.
pub(super) fn fold(t: Term) -> Term {
    fn value(t: Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v),
            Term::Obj(m) => m
                .into_iter()
                .map(|(k, t)| Some((k, value(t)?)))
                .collect::<Option<_>>()
                .map(Value::Obj),
            Term::List(xs) => xs
                .into_iter()
                .map(value)
                .collect::<Option<_>>()
                .map(Value::List),
            _ => None,
        }
    }
    let kept = t.clone();
    value(t).map_or(kept, Term::Val)
}
