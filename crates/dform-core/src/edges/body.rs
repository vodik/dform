//! A clause's literals in the walk: a comparison's sides reach each
//! other's types and are checked, `=` makes one value, a body's atom
//! types its arguments by what it is (a function's, an attribute's, a
//! membership's, a relation's), `x in l` an element of its collection.

use super::shape::*;
use super::*;

impl Graph<'_> {
    pub(super) fn body(&mut self, body: &mut [Lit], at: &At) {
        for l in body {
            self.lit(l, at);
        }
    }

    pub(super) fn lit(&mut self, l: &mut Lit, at: &At) {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(a, at),
            Lit::Eq(x, y) => self.equal(x, y, at),
            Lit::Neq(x, y) => self.compare("!=", x, y, at),
            Lit::Gt(x, y) => self.compare(">", x, y, at),
            Lit::Ge(x, y) => self.compare(">=", x, y, at),
            Lit::Lt(x, y) => self.compare("<", x, y, at),
            Lit::Le(x, y) => self.compare("<=", x, y, at),
        }
    }

    /// One side of a check, `term` as written: its node when it is a
    /// variable, the node of the reference it reads an attribute of
    /// (`r.p`), and its site.
    pub(super) fn side(&mut self, term: Term, site: Option<usize>) -> checks::Side {
        let (node, base) = match &term {
            Term::Var(v) => (Some(self.var(v)), None),
            Term::Func { name, args } if name == "__path" => match args.as_slice() {
                [Term::Var(r), _] => (None, Some(self.var(r))),
                _ => (None, None),
            },
            _ => (None, None),
        };
        checks::Side {
            term,
            node,
            base,
            site,
        }
    }

    /// `x op y`: the sides reach each other's types (`c > 500m`), their
    /// columns stay apart, and the comparison is checked (R-34).
    pub(super) fn compare(&mut self, op: &'static str, x: &mut Term, y: &mut Term, at: &At) {
        let (wx, wy) = (unmarked(x), unmarked(y));
        let first = self.sites.len();
        let a = self.term_node(x, at);
        let mid = self.sites.len();
        let b = self.term_node(y, at);
        self.reach(a, b);
        let (sx, sy) = (self.one_site(first, a), self.one_site(mid, b));
        let sides = [self.side(wx, sx), self.side(wy, sy)];
        let span = at.span;
        self.checks.checks.push(checks::Check { op, sides, span });
    }

    /// `x = y`: one value. A variable equal to a literal holds it (its
    /// kind is the variable's column's, R-34), to a call its result's
    /// type.
    pub(super) fn equal(&mut self, x: &mut Term, y: &mut Term, at: &At) {
        let (wx, wy) = (unmarked(x), unmarked(y));
        let first = self.sites.len();
        let a = self.term_node(x, at);
        let mid = self.sites.len();
        let b = self.term_node(y, at);
        self.union(a, b);
        let (sx, sy) = (self.one_site(first, a), self.one_site(mid, b));
        match (&wx, &wy) {
            (Term::Var(_), Term::Val(_)) => self.literal(a, wy, sy, at.span),
            (Term::Val(_), Term::Var(_)) => self.literal(a, wx, sx, at.span),
            (Term::Var(_), _) if sy.is_some() => self.literal(a, wy, sy, at.span),
            (_, Term::Var(_)) if sx.is_some() => self.literal(a, wx, sx, at.span),
            (Term::Var(_), t) | (t, Term::Var(_)) => {
                self.checks.shape(a, t, at.span);
                if let Some((ty, what)) = lattice::term_type(t) {
                    let role = Role::Types { prints: false };
                    self.edge(a, Shape::Ty(ty), what, at.span, role);
                }
                // `x = r.p`: the attribute of the resource `r` names.
                if let Term::Func { name, args } = t
                    && let ("__path", [Term::Var(r), Term::Val(Value::Str(path))]) =
                        (name.as_str(), args.as_slice())
                {
                    let node = self.var(r);
                    self.checks.throughs.push(checks::Through {
                        node,
                        var: r.clone(),
                        path: path.clone(),
                        into: a,
                        span: at.span,
                    });
                }
            }
            _ => {}
        }
    }

    /// An atom of a body: a function's arguments its parameters', an
    /// attribute's value the schema's, a membership's element its
    /// collection's, any other relation's arguments its columns.
    pub(super) fn atom(&mut self, a: &mut Atom, at: &At) {
        let at = &At {
            atom: a.span,
            ..at.clone()
        };
        if let Some(f) = crate::functions::get(a.pred.name()) {
            let span = a.span;
            self.params(f, &mut a.args, &At { span, ..at.clone() });
            return;
        }
        match (a.pred.name(), a.args.as_mut_slice()) {
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
                if let Term::Val(Value::Str(text)) = unmarked(v) {
                    self.ref_compared(typ, &unmarked(addr), path, &text, a.span);
                }
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

    /// The attribute `p` of the `typ` at `addr` joined with the string
    /// `text` at `span` (`s.vpc == "main"`): an error when it is a
    /// reference, which is never a string.
    pub(super) fn ref_compared(&mut self, typ: &str, addr: &Term, p: &str, text: &str, span: Span) {
        let attr = self.schema.attr(typ, p).map(|a| Ty::parse(&a.ty));
        if let Some(Ty::Ref(want)) = attr {
            let read = format!("{}.{p}", lattice::shown_term(addr));
            let written = format!("`{read} == {text:?}`");
            let d = lattice::ref_and_string(&written, &read, &want, text, span);
            self.checks.said.push(d);
        }
    }

    /// `x in l`: `x` is an element of `l`; a variable over a range is an
    /// int, over a list of literals each element's kind (R-34).
    pub(super) fn member(&mut self, list: &mut Term, item: &mut Term, at: &At, span: Span) {
        let written = unmarked(list);
        let first = self.sites.len();
        let l = self.term_node(list, at);
        let elems = self.sites.len() - first;
        let x = self.term_node(item, at);
        self.link(l, vec![Seg::Elem], x);
        // `x in o` over an object is an error (R-58).
        let over = match &written {
            Term::Var(v) => Some(Some((l, v.clone()))),
            Term::Obj(_) | Term::Val(Value::Obj(_)) => Some(None),
            _ => None,
        };
        if let Some(list) = over {
            let item = unmarked(item);
            self.checks
                .members
                .push(checks::Member { list, item, span });
        }
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
                true => self.literal(x, t, Some(first + k), span),
                false if matches!(t, Term::Val(_)) => self.literal(x, t, None, span),
                false => {}
            }
        }
    }
}
