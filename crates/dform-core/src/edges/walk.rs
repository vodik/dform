//! The walk: the statements `lower` writes, each literal marked with its
//! node, collected into the graph: the edges each value reaches and the
//! links between values (R-192), and what each column's type settles
//! from (R-34).

use super::shape::*;
use super::sites::Holder;
use super::*;
use crate::infer;

/// `any` written as a column's type.
fn is_any(t: &TypeExpr) -> bool {
    matches!(t, TypeExpr::Name(n) if n == "any")
}

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
            Stmt::Rule(r)
                if r.head.pred == "arg"
                    && r.head.args.len() >= 4
                    && matches!(r.head.args[0], Term::Var(_)) =>
            {
                let a = at(r.head.span);
                self.set_through(r, &a);
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
                self.uses(n);
                let what = format!("input {}", i.name);
                let ty = Shape::Declared(i.ty.clone());
                self.edge(n, ty, what, i.span, Role::Both);
                if let Some(d) = &mut i.default {
                    self.place(n, Vec::new(), d, &at(i.span));
                }
                self.refinement(i, n, &at(i.span));
            }
            Stmt::Output(o) => self.output(o, &at(o.span)),
            Stmt::Decl(d) => self.decl(d, scope),
            Stmt::ExternFn(e) => self.extern_fn(e, scope),
            // A let with parameters (R-187): its last column its value.
            Stmt::Mode(e) if e.arity > 0 => {
                let col = (
                    self.declaring(scope, &e.pred),
                    e.pred.clone(),
                    e.arity,
                    e.arity - 1,
                );
                self.typing.names.insert(col, "value".into());
            }
            Stmt::Instance(u) => self.copy(u, "component", &at(u.span)),
            Stmt::Use(u) => self.copy(u, "module", &at(u.span)),
            _ => {}
        }
    }

    /// `input k: T where R`: what of `R` is no table's refinement holds of
    /// the input's value, the cell `n` (`modules::refinement`), its
    /// literals read by the input's own reading, not here.
    fn refinement(&mut self, i: &crate::ast::InputDecl, n: usize, at: &At) {
        if i.name.contains('.') {
            return;
        }
        let mut written = i.clone();
        written.refinement = i.refinement.iter().map(unmarked_lit).collect();
        let (_, rest) = crate::refine::split_input(&written);
        if rest.is_empty() {
            return;
        }
        let v = "__Input".to_string();
        let names = BTreeMap::from([(i.name.clone(), Term::Var(v.clone()))]);
        let mut body: Vec<Lit> = rest.iter().map(|l| l.replace_names(&names)).collect();
        self.vars.clear();
        self.vars.insert(v, n);
        let at = At {
            sites: false,
            ..at.clone()
        };
        self.body(&mut body, &at);
    }

    /// `set x.p = v where x in resource`: `v` is the attribute of each
    /// type `x` may be.
    fn set_through(&mut self, r: &mut crate::ast::RuleStmt, at: &At) {
        let Term::Val(Value::Str(path)) = &r.head.args[2] else {
            return;
        };
        let n = self.node();
        let rule = unmarked_rule(r);
        for typ in types::set_types(self.schema, &rule, path) {
            let what = format!("{typ}.{path}");
            let shape = Shape::Attr(typ, keys(path));
            self.edge(n, shape, what, r.head.span, Role::Reads);
        }
        let at = At {
            sites: false,
            ..at.clone()
        };
        self.place(n, Vec::new(), &mut r.head.args[3], &at);
    }

    /// An output's declaration and its value are two statements: its
    /// cell is one.
    fn output(&mut self, o: &mut crate::ast::OutputDecl, at: &At) {
        let n = self.column(at.scope, &format!("{OUTPUT}{}", o.name), 1, 0);
        self.cell(n, format!("output {}", o.name));
        if let Some(ty) = &o.ty {
            let what = format!("output {}", o.name);
            self.edge(n, Shape::Declared(ty.clone()), what, o.span, Role::Both);
        }
        if let Some(v) = &mut o.value {
            self.give(n, v, at, true);
        }
    }

    /// `decl p(a: T, ..)`: each typed column's type, a column declared
    /// `any` open ([`Self::declarations`]); its names the columns'.
    fn decl(&mut self, d: &crate::ast::Decl, scope: ScopeId) {
        let arity = d.fields.len();
        let declaring = self.declaring(scope, &d.pred);
        self.typing.decls.insert((declaring, d.pred.clone(), arity));
        for (i, f) in d.fields.iter().enumerate() {
            let col = (declaring, d.pred.clone(), arity, i);
            self.typing.names.insert(col, f.clone());
        }
        for (i, t) in d.types.iter().enumerate() {
            let Some(t) = t.as_ref().filter(|t| !is_any(t)) else {
                continue;
            };
            let n = self.column(scope, &d.pred, arity, i);
            let what = format!("decl {}", d.pred);
            self.edge(n, Shape::Declared(t.clone()), what, d.span, Role::Both);
        }
    }

    /// `extern p(+a: T, -b)`: each typed column's type (R-34).
    fn extern_fn(&mut self, e: &crate::ast::ExternFn, scope: ScopeId) {
        self.typing.externs.insert(e.name.clone());
        self.extern_columns(e, scope);
    }

    /// The typed columns of the extern `e`, read in `scope`.
    pub(super) fn extern_columns(&mut self, e: &crate::ast::ExternFn, scope: ScopeId) {
        let arity = e.args.len();
        for (i, b) in e.args.iter().enumerate() {
            let Some(ty) = &b.ty else { continue };
            let n = self.column(scope, &e.name, arity, i);
            let what = format!("{}'s column {}", e.name, b.name);
            let role = Role::Types { prints: false };
            self.edge(n, Shape::Declared(ty.clone()), what, e.span, role);
        }
    }

    /// What `stmts` declare, in their scopes, before the walk, wherever
    /// the `decl` stands: the columns declared `any`, which take anything
    /// and join nothing, and each relation's columns by name.
    pub(super) fn declarations(&mut self, stmts: &[Stmt], scope: ScopeId) {
        for s in stmts {
            match s {
                Stmt::Decl(d) => {
                    let declaring = self.declaring(scope, &d.pred);
                    let named = (declaring, d.pred.clone());
                    self.typing.fields.entry(named).or_insert(d.fields.clone());
                    for (i, t) in d.types.iter().enumerate() {
                        if t.as_ref().is_some_and(is_any) {
                            let col = (declaring, d.pred.clone(), d.fields.len(), i);
                            self.typing.open.insert(col);
                        }
                    }
                }
                Stmt::Module(m) => {
                    let scope = self.definition(&m.name);
                    self.declarations(&m.body, scope);
                }
                _ => {}
            }
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
            let shape = Shape::Attr(typ.clone(), keys(&f.key));
            self.edge(n, shape, what, f.span, Role::Reads);
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
    /// `k = v` the module's input `k` (`cfg.cidr = ..` a field of `cfg`),
    /// then each row it gives a relation of the module's (R-55).
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
                true => self.give(n, t, &at, false),
                false => self.place(n, path, t, &at),
            }
        }
        for row in &mut u.rows {
            self.vars.clear();
            self.row(row, scope, at);
        }
    }

    /// A row a copy's block gives the module's relation (R-55): its head
    /// the module's, in `def`, its body the user's.
    fn row(&mut self, row: &mut Stmt, def: ScopeId, at: &At) {
        let head = At {
            scope: def,
            ..at.clone()
        };
        match row {
            Stmt::Fact(a) => self.relation(a, &head, true),
            Stmt::Rule(r) => {
                self.relation(&mut r.head, &head, true);
                self.body(&mut r.body, at);
            }
            _ => {}
        }
    }

    /// A rule's head: a let's cell, an attribute's contribution, or a
    /// relation's row.
    fn head(&mut self, a: &mut Atom, at: &At) {
        if let Some(k) = let_key(a).map(str::to_string) {
            let n = self.column(at.scope, &k, 1, 0);
            self.uses(n);
            self.cell(n, format!("let {k}"));
            let at = At {
                holder: Some(Holder::Let(k)),
                span: a.span,
                ..at.clone()
            };
            self.give(n, &mut a.args[1], &at, true);
            return;
        }
        // `output(k, V) :- B`: the value of `output k`.
        if let ("output", [Term::Val(Value::Str(k)), _]) = (a.pred.as_str(), a.args.as_slice()) {
            let n = self.column(at.scope, &format!("{OUTPUT}{k}"), 1, 0);
            self.cell(n, format!("output {k}"));
            let at = At {
                span: a.span,
                ..at.clone()
            };
            self.give(n, &mut a.args[1], &at, true);
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
            self.edge(n, Shape::Attr(typ, keys(&path)), what, a.span, Role::Reads);
            let at = At {
                sites: false,
                ..at.clone()
            };
            self.place(n, Vec::new(), &mut a.args[3], &at);
            return;
        }
        self.relation(a, at, true);
    }

    /// The relation the column `n` is of has a signature: a let's, an
    /// input's (R-34), read or not.
    fn uses(&mut self, n: usize) {
        if let Some((scope, pred, arity, _)) = self.col_of[n].clone() {
            self.typing.used.insert((scope, pred, arity));
        }
    }

    /// The node `n` is the cell a message names `name` (`let apex`).
    fn cell(&mut self, n: usize, name: String) {
        if let Some(col) = self.col_of[n].clone() {
            self.typing.cells.insert(col, name);
        }
    }

    /// A relation's atom: each argument is its column's. A relation the
    /// program has (not one the compiler writes with values of any type,
    /// [`infer::untyped`]) takes its arguments' types (R-34), and a
    /// head's variables name its columns; a column declared `any`, and a
    /// compiler's relation's, joins nothing: its arguments only reach
    /// each other's types.
    fn relation(&mut self, a: &mut Atom, at: &At, head: bool) {
        if let Some(mut placed) = self.record(a, at.scope) {
            return self.relation(&mut placed, at, head);
        }
        let (arity, pred, span) = (a.args.len(), a.pred.clone(), a.span);
        let typed = !infer::untyped(&pred);
        if typed {
            let (declaring, name) = self.resolve(at.scope, &pred);
            self.typing.used.insert((declaring, name, arity));
        }
        for (i, t) in a.args.iter_mut().enumerate() {
            let n = self.column(at.scope, &pred, arity, i);
            let col = self.col_of[n].clone().expect("a column's node");
            if !typed || self.typing.open.contains(&col) {
                let x = self.node();
                self.reach(x, n);
                self.place(x, Vec::new(), t, at);
                continue;
            }
            if head {
                self.typing.head_name(col, t);
            }
            self.argument(n, t, at, span);
        }
    }

    /// A record atom (`p { a: x }`) with its fields in their columns by
    /// the relation's `decl`, a field it does not write a hole; `None` for
    /// any other, and where no `decl` says the columns.
    fn record(&self, a: &Atom, scope: ScopeId) -> Option<Atom> {
        let record = a.record.as_ref().filter(|_| a.args.is_empty())?;
        let fields = self.typing.fields.get(&self.resolve(scope, &a.pred))?;
        let args = fields
            .iter()
            .map(|f| record.get(f).cloned().unwrap_or(Term::Wildcard))
            .collect();
        Some(Atom {
            args,
            record: None,
            ..a.clone()
        })
    }

    /// `t` in the column `n` of an atom at `span`: [`Self::held`], and a
    /// computed value's type the column's (R-34); a literal is read at the
    /// column's type in turn.
    fn argument(&mut self, n: usize, t: &mut Term, at: &At, span: Span) {
        let written = unmarked(t);
        match self.held(n, t, at, span) {
            Some(site) => {
                self.typing.of_site.insert(site, n);
            }
            None if infer::constant(&written).is_none() => {
                if let Some((ty, what)) = infer::term_type(&written) {
                    let role = Role::Types { prints: false };
                    self.edge(n, Shape::Ty(ty), what, span, role);
                }
            }
            None => {}
        }
    }

    /// `t` placed in the column `n` at `span`, and its literal's kind the
    /// column's (R-34), as its site reads it: the site of a literal
    /// written alone.
    fn held(&mut self, n: usize, t: &mut Term, at: &At, span: Span) -> Option<usize> {
        let written = unmarked(t);
        let first = self.sites.len();
        self.place(n, Vec::new(), t, at);
        if let Some(site) = self.one_site(first, n) {
            self.typing.literal(n, written, Some(site), span);
            return Some(site);
        }
        if let Some(v) = infer::constant(&written) {
            self.typing.literal(n, Term::Val(v), None, span);
        }
        None
    }

    /// The site placing a term in `n` made since `first`, when it made
    /// one, the term's own (a literal written alone, `500m`, a range).
    fn one_site(&self, first: usize, n: usize) -> Option<usize> {
        let one = self.sites.len() == first + 1;
        (one && self.sites[first].node == n && self.sites[first].path.is_empty()).then_some(first)
    }

    fn body(&mut self, body: &mut [Lit], at: &At) {
        for l in body {
            self.lit(l, at);
        }
    }

    fn lit(&mut self, l: &mut Lit, at: &At) {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(a, at),
            Lit::Eq(x, y) => self.equal(x, y, at),
            // The sides of a comparison reach each other's types (`c >
            // 500m`); their columns stay apart (R-34 checks them).
            Lit::Neq(x, y) | Lit::Gt(x, y) | Lit::Ge(x, y) | Lit::Lt(x, y) | Lit::Le(x, y) => {
                let a = self.term_node(x, at);
                let b = self.term_node(y, at);
                self.reach(a, b);
            }
        }
    }

    /// `x = y`: one value. A variable equal to a literal holds it (its
    /// kind is the variable's column's, R-34), to a call its result's
    /// type.
    fn equal(&mut self, x: &mut Term, y: &mut Term, at: &At) {
        let (wx, wy) = (unmarked(x), unmarked(y));
        let first = self.sites.len();
        let a = self.term_node(x, at);
        let mid = self.sites.len();
        let b = self.term_node(y, at);
        self.union(a, b);
        let (sx, sy) = (self.one_site(first, a), self.one_site(mid, b));
        match (&wx, &wy) {
            (Term::Var(_), Term::Val(_)) => self.typing.literal(a, wy, sy, at.span),
            (Term::Val(_), Term::Var(_)) => self.typing.literal(a, wx, sx, at.span),
            (Term::Var(_), _) if sy.is_some() => self.typing.literal(a, wy, sy, at.span),
            (_, Term::Var(_)) if sx.is_some() => self.typing.literal(a, wx, sx, at.span),
            (Term::Var(_), t) | (t, Term::Var(_)) => {
                if let Some((ty, what)) = infer::term_type(t) {
                    let role = Role::Types { prints: false };
                    self.edge(a, Shape::Ty(ty), what, at.span, role);
                }
            }
            _ => {}
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
            // `attr(T, A, P, V)`: `V` is the attribute's; a variable's
            // column its type (R-34).
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
                let role = match v {
                    Term::Var(_) => Role::Both,
                    _ => Role::Reads,
                };
                self.edge(n, shape, what, a.span, role);
                self.place(n, Vec::new(), v, at);
            }
            ("member", [list, item]) => self.member(list, item, at, a.span),
            ("output", [copy, Term::Val(Value::Str(k)), v]) => {
                let (copy, k, v) = (unmarked(copy), k.clone(), unmarked(v));
                self.relation(a, at, false);
                self.copy_output(&copy, &k, v, at.scope, a.span);
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
            _ => self.relation(a, at, false),
        }
    }

    /// `output(C, "k", V)`, the output `k` of the copy `C` read in
    /// `scope`: `V` is the cell of its definition's (R-34), a variable one
    /// column with it, a literal one it holds.
    fn copy_output(&mut self, copy: &Term, k: &str, v: Term, scope: ScopeId, span: Span) {
        let Term::Val(Value::Str(copy)) = copy else {
            return;
        };
        let body = self.scopes.copied_body(scope, copy);
        let Some(body) = body.or_else(|| self.used(scope, copy)) else {
            return;
        };
        let cell = self.column(body, &format!("{OUTPUT}{k}"), 1, 0);
        match v {
            Term::Var(x) => {
                let x = self.var(&x);
                self.typing.joins.push((x, cell));
            }
            v @ Term::Val(_) => self.typing.literal(cell, v, None, span),
            _ => {}
        }
    }

    /// `x in l`: `x` is an element of `l`; a variable over a range is an
    /// int, over a list of literals each element's kind (R-34).
    fn member(&mut self, list: &mut Term, item: &mut Term, at: &At, span: Span) {
        let written = unmarked(list);
        let first = self.sites.len();
        let l = self.term_node(list, at);
        let elems = self.sites.len() - first;
        let x = self.term_node(item, at);
        self.link(l, vec![Seg::Elem], x);
        if !matches!(item, Term::Var(_)) {
            return;
        }
        let elements: Vec<Term> = match written {
            Term::Func { name, .. } if name == "int.range" => {
                let int = Shape::Ty(Ty::Scalar("int".into()));
                let role = Role::Types { prints: false };
                self.edge(x, int, "a range".into(), span, role);
                return;
            }
            Term::List(xs) => xs,
            Term::Val(Value::List(xs)) => xs.into_iter().map(Term::Val).collect(),
            _ => return,
        };
        // Each element's site, where each is one literal.
        let one_each = elems == elements.len()
            && self.sites[first..first + elems]
                .iter()
                .all(|s| s.node == l && s.path == [Seg::Elem]);
        for (k, t) in elements.into_iter().enumerate() {
            match one_each {
                true => self.typing.literal(x, t, Some(first + k), span),
                false if matches!(t, Term::Val(_)) => self.typing.literal(x, t, None, span),
                false => {}
            }
        }
    }

    /// `t` is one of the values of the cell `n` (a let's, an output's,
    /// an input's): a variable flows into it, one way; anything else is
    /// placed in it. A let's or an output's (`cell`) takes, besides, a
    /// variable's type one way (R-213) and a literal's kind (R-34).
    fn give(&mut self, n: usize, t: &mut Term, at: &At, cell: bool) {
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
    fn params(&mut self, f: &crate::functions::Function, args: &mut [Term], at: &At) {
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
            self.edge(side, Shape::Like(other), what, at.span, Role::Reads);
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
