//! The walk: the statements `lower` writes, each literal marked with its
//! node, collected into the graph.

use super::shape::*;
use super::sites::Holder;
use super::*;

/// The key `k` of a `let(k, t, rank)` head.
fn let_key(a: &Atom) -> Option<&str> {
    match (a.pred.as_str(), a.args.as_slice()) {
        (crate::modules::LET, [Term::Val(Value::Str(k)), _, _]) => Some(k),
        _ => None,
    }
}

impl Graph<'_> {
    /// The statements `lower` writes, each in its scope, its variables its
    /// own.
    pub(super) fn program(&mut self, stmts: &mut [Stmt], scope: ScopeId) {
        for s in stmts {
            self.vars.clear();
            self.stmt(s, scope);
        }
    }

    fn stmt(&mut self, s: &mut Stmt, scope: ScopeId) {
        let at = |span: Span| At {
            scope,
            span,
            holder: None,
            sites: true,
        };
        match s {
            Stmt::Fact(a) => self.head(a, &at(a.span)),
            // `set x.p = v where x in resource`: `v` is the attribute of
            // each type `x` may be.
            Stmt::Rule(r)
                if r.head.pred == "arg"
                    && r.head.args.len() >= 4
                    && matches!(r.head.args[0], Term::Var(_)) =>
            {
                let a = at(r.head.span);
                if let Term::Val(Value::Str(path)) = &r.head.args[2] {
                    let n = self.node();
                    let rule = unmarked_rule(r);
                    for typ in types::set_types(self.schema, &rule, path) {
                        let what = format!("{typ}.{path}");
                        self.edge(n, Shape::Attr(typ, keys(path)), what, r.head.span);
                    }
                    let at = At {
                        sites: false,
                        ..a.clone()
                    };
                    self.place(n, Vec::new(), &mut r.head.args[3], &at);
                }
                self.body(&mut r.body, &a);
            }
            Stmt::Rule(r) => {
                let a = at(r.head.span);
                self.head(&mut r.head, &a);
                self.body(&mut r.body, &a);
            }
            Stmt::Module(m) => {
                let scope = self.definition(&m.name);
                self.program(&mut m.body, scope)
            }
            Stmt::Resource(r) => self.resource(r, &at(r.span)),
            Stmt::Input(i) => {
                let n = self.column(scope, &i.name, 1, 0);
                let what = format!("input {}", i.name);
                self.edge(n, Shape::Declared(i.ty.clone()), what, i.span);
                if let Some(d) = &mut i.default {
                    self.place(n, Vec::new(), d, &at(i.span));
                }
            }
            // An output's declaration and its value are two statements:
            // its cell is one.
            Stmt::Output(o) => {
                let n = self.column(scope, &format!("{OUTPUT}{}", o.name), 1, 0);
                if let Some(ty) = &o.ty {
                    let what = format!("output {}", o.name);
                    self.edge(n, Shape::Declared(ty.clone()), what, o.span);
                }
                if let Some(v) = &mut o.value {
                    self.give(n, v, &at(o.span));
                }
            }
            Stmt::Decl(d) => {
                let arity = d.fields.len();
                for (i, t) in d.types.iter().enumerate() {
                    let Some(t) = t else { continue };
                    let n = self.column(scope, &d.pred, arity, i);
                    let what = format!("decl {}", d.pred);
                    self.edge(n, Shape::Declared(t.clone()), what, d.span);
                }
            }
            Stmt::Instance(u) => self.copy(u, "component", &at(u.span)),
            Stmt::Use(u) => self.copy(u, "module", &at(u.span)),
            _ => {}
        }
    }

    /// `resource T n { p = v .. } [where B]`: each entry is its
    /// attribute's (read by the attribute's own reading), then the body.
    fn resource(&mut self, r: &mut crate::ast::Resource, at: &At) {
        let Term::Val(Value::Str(typ)) = &r.typ else {
            return;
        };
        let typ = typ.clone();
        let name = unmarked(&r.name);
        let fields = At {
            sites: false,
            ..at.clone()
        };
        for f in &mut r.fields {
            let n = self.node();
            let what = shown_attr(&typ, &name, &f.key);
            self.edge(n, Shape::Attr(typ.clone(), keys(&f.key)), what, f.span);
            let at = At {
                span: f.span,
                ..fields.clone()
            };
            self.place(n, Vec::new(), &mut f.value, &at);
        }
        if let Some(b) = &mut r.body {
            self.body(b, at);
        }
    }

    /// `resource C name { k = v }`, `use M { k = v }`: its body, then each
    /// `k = v` the module's input `k` (`cfg.cidr = ..` a field of `cfg`).
    fn copy(&mut self, u: &mut crate::ast::Instance, kind: &str, at: &At) {
        if let Some(b) = &mut u.body {
            self.body(b, at);
        }
        let module = u.module.clone();
        let scope = self.definition(&module);
        for (k, t, span) in &mut u.inputs {
            let mut path = keys(k);
            let Some(Seg::Key(input)) = (!path.is_empty()).then(|| path.remove(0)) else {
                continue;
            };
            let n = self.column(scope, &input, 1, 0);
            let holder = Holder::Named(format!("input {k} of {kind} {module}"));
            let at = At {
                span: *span,
                holder: Some(holder),
                ..at.clone()
            };
            match path.is_empty() {
                true => self.give(n, t, &at),
                false => self.place(n, path, t, &at),
            }
        }
    }

    /// A rule's head: a let's cell, an attribute's contribution, or a
    /// relation's row.
    fn head(&mut self, a: &mut Atom, at: &At) {
        if let Some(k) = let_key(a).map(str::to_string) {
            let n = self.column(at.scope, &k, 1, 0);
            let at = At {
                holder: Some(Holder::Let(k)),
                ..at.clone()
            };
            self.give(n, &mut a.args[1], &at);
            return;
        }
        // `output(k, V) :- B`: the value of `output k`.
        if let ("output", [Term::Val(Value::Str(k)), _]) = (a.pred.as_str(), a.args.as_slice()) {
            let n = self.column(at.scope, &format!("{OUTPUT}{k}"), 1, 0);
            self.give(n, &mut a.args[1], at);
            return;
        }
        // `arg(T, A, P, V, ..)`: a `set` (R-38); its literals the
        // attribute's own reading reads.
        if a.pred == "arg"
            && a.args.len() >= 4
            && let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) =
                (unmarked(&a.args[0]), &a.args[2])
        {
            let path = path.clone();
            let n = self.node();
            let what = shown_attr(&typ, &unmarked(&a.args[1]), &path);
            self.edge(n, Shape::Attr(typ, keys(&path)), what, a.span);
            let at = At {
                sites: false,
                ..at.clone()
            };
            self.place(n, Vec::new(), &mut a.args[3], &at);
            return;
        }
        self.relation(a, at);
    }

    /// A relation's atom: each argument is its column's.
    fn relation(&mut self, a: &mut Atom, at: &At) {
        let arity = a.args.len();
        let pred = a.pred.clone();
        for (i, t) in a.args.iter_mut().enumerate() {
            let n = self.column(at.scope, &pred, arity, i);
            self.place(n, Vec::new(), t, at);
        }
    }

    fn body(&mut self, body: &mut [Lit], at: &At) {
        for l in body {
            self.lit(l, at);
        }
    }

    fn lit(&mut self, l: &mut Lit, at: &At) {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(a, at),
            // Both sides of a comparison are one type.
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => {
                let a = self.term_node(x, at);
                let b = self.term_node(y, at);
                self.union(a, b);
            }
        }
    }

    /// An atom of a body: a function's arguments its parameters', an
    /// attribute's value the schema's, a membership's element its
    /// collection's, any other relation's arguments its columns.
    fn atom(&mut self, a: &mut Atom, at: &At) {
        if let Some(f) = crate::functions::get(&a.pred) {
            let span = a.span;
            self.params(f, &mut a.args, &At { span, ..at.clone() });
            return;
        }
        match (a.pred.as_str(), a.args.as_mut_slice()) {
            // `attr(T, A, P, V)`: `V` is the attribute's.
            (
                "attr",
                [
                    Term::Val(Value::Str(typ)),
                    addr,
                    Term::Val(Value::Str(path)),
                    v,
                ],
            ) => {
                let n = self.node();
                let what = shown_attr(typ, &unmarked(addr), path);
                let shape = Shape::Attr(typ.clone(), keys(path));
                self.edge(n, shape, what, a.span);
                self.place(n, Vec::new(), v, at);
            }
            // `x in l`: `x` is an element of `l`.
            ("member", [list, item]) => {
                let l = self.term_node(list, at);
                let x = self.term_node(item, at);
                self.link(l, vec![Seg::Elem], x);
            }
            // `(k, v) in o`: `v` is `o`'s at `k`; `l[i]`, an element.
            ("member", [object, key, value]) => {
                let seg = match unmarked(key) {
                    Term::Val(Value::Str(k)) => Seg::Key(k),
                    Term::Val(Value::Int(_)) => Seg::Elem,
                    _ => Seg::Any,
                };
                let o = self.term_node(object, at);
                let v = self.term_node(value, at);
                self.link(o, vec![seg], v);
            }
            ("want", _) => {}
            _ => self.relation(a, at),
        }
    }

    /// `t` is one of the values of the cell `n` (a let's, an output's,
    /// an input's): a variable flows into it, one way; anything else is
    /// placed in it.
    fn give(&mut self, n: usize, t: &mut Term, at: &At) {
        let var = match t {
            Term::Var(v) => Some(v.clone()),
            Term::Func { name, args } if name == types::AS => match args.as_slice() {
                [Term::Var(v), _] => Some(v.clone()),
                _ => None,
            },
            _ => None,
        };
        match var {
            Some(v) => {
                let x = self.var(&v);
                self.flows.push((x, n));
            }
            None => self.place(n, Vec::new(), t, at),
        }
    }

    /// A function's arguments: each its parameter's type (a `string`
    /// parameter takes a value as its text, R-133); a coeffect's none.
    fn params(&mut self, f: &crate::functions::Function, args: &mut [Term], at: &At) {
        for (i, t) in args.iter_mut().enumerate() {
            let p = f
                .params
                .get(i)
                .or_else(|| f.variadic.then(|| f.params.last()).flatten());
            let n = self.node();
            // A coeffect's argument is the host's to read (`oci.resolve`,
            // `io.read`): its row is keyed by the value as written.
            if let Some(p) = p.filter(|_| !f.coeffect) {
                let ty = Ty::parse(&p.ty);
                if readable(&ty) {
                    let what = format!("`{}`'s argument `{}`", f.name, p.name);
                    self.edge(n, Shape::Ty(ty), what, at.span);
                }
            }
            self.place(n, Vec::new(), t, at);
        }
    }

    /// The node a term's value is: a variable's, a field read's, or a
    /// fresh one it is placed in.
    fn term_node(&mut self, t: &mut Term, at: &At) -> usize {
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
    fn place(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
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
    fn call(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        let Term::Func { name, args } = t else {
            unreachable!("a call")
        };
        match (name.as_str(), args.as_mut_slice()) {
            // A literal's node ([`marked`]): its reading the node's.
            (NODE, [Term::Val(Value::Int(id)), held]) => {
                let before = self.readings.is_some().then(|| held.clone());
                self.place(n, path, held, at);
                if before.is_some_and(|b| b != *held) {
                    self.read.insert(node_id(*id), held.clone());
                }
            }
            // `x.p`: the part of `x` at `p`.
            ("__path", [Term::Var(v), Term::Val(Value::Str(p))]) => {
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

    /// `a + b`, `a - b` at `n`'s `path`: each side takes the other's type
    /// and the result's (a duration beside a time).
    fn sum(&mut self, n: usize, path: Path, a: &mut Term, b: &mut Term, at: &At) {
        let (x, y, r) = (self.term_node(a, at), self.term_node(b, at), self.node());
        self.link(n, path, r);
        for (side, other) in [(x, y), (y, x), (x, r), (y, r)] {
            let what = "the other side".to_string();
            self.edge(side, Shape::Like(other), what, at.span);
        }
    }
}

/// A constant object or list as terms.
fn unfold(v: Value) -> Term {
    match v {
        Value::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, unfold(v))).collect()),
        Value::List(xs) => Term::List(xs.into_iter().map(unfold).collect()),
        v => Term::Val(v),
    }
}

/// [`unfold`] back: still a constant.
fn fold(t: Term) -> Term {
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
