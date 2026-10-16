//! The walk: the statements `lower` writes, each literal marked with its
//! node, collected into the graph: the edges each value reaches and the
//! links between values (R-192), and what each column's type settles
//! from (R-34).

use super::shape::*;
use super::sites::Holder;
use super::*;

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
            atom: span,
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
        let at = &At {
            atom: a.span,
            ..at.clone()
        };
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
}
