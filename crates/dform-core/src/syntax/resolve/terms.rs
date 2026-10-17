//! A term as its node (R-211 step 7): a literal, a constant, a call, a
//! list, an object, a comprehension, an operator, a range, a string
//! with its interpolations, a chain resolved and realized; the builder of
//! the statement's nodes the resolver builds them with; and a term read
//! as a declared type (`retyped`).

use super::*;

impl Lowerer<'_> {
    /// A block path or keypath as a stored path: `a.b`, `a[0].b`, a quoted
    /// segment unquoted unless it holds `.`, `[`, `]`, `/` or `"` (R-77).
    pub(super) fn block_path(&mut self, n: &SyntaxNode) -> L<String> {
        let mut out = String::new();
        for t in tokens(n) {
            match t.kind() {
                DOT => out.push('.'),
                L_BRACKET | R_BRACKET | INT => out.push_str(t.text()),
                STRING => out.push_str(&crate::address::path_key(&self.string(&t)?)),
                _ => out.push_str(t.text()),
            }
        }
        Ok(out)
    }

    /// A selector's quoted step `."k"` is one key: `.`, `[` or `]` inside
    /// it would read as a second step (a path's quoted segment carries
    /// any character, R-77).
    pub(super) fn segment(&mut self, t: &SyntaxToken) -> L<String> {
        let s = self.string(t)?;
        if s.contains(['.', '[', ']']) {
            return self.error(
                self.span_of(t.text_range()),
                format!(
                    "the key {s:?} holds `.`, `[` or `]`, which a selector's step cannot carry"
                ),
            );
        }
        Ok(s)
    }

    pub(super) fn string(&mut self, t: &SyntaxToken) -> L<String> {
        let text = if self.text {
            unescape(t.text())
        } else {
            string_value(t.text())
        };
        match text {
            Ok(s) => Ok(s),
            Err(e) => self.error(self.span_of(t.text_range()), e),
        }
    }

    /// A term that must be a constant: no read, no variable.
    pub(super) fn constant(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Term> {
        let id = self.constant_node(rc, n)?;
        Ok(self.term_of(id))
    }

    /// The constant `t` read as the type `ty` (R-31): its node, or why it
    /// is not one.
    pub(super) fn typed_constant(
        &mut self,
        rc: &mut Rc,
        t: &SyntaxNode,
        ty: &TypeExpr,
    ) -> L<Result<ExprId, String>> {
        let id = self.constant_node(rc, t)?;
        let was = self.term_of(id);
        Ok(
            crate::types::literal(&crate::types::of_expr(ty), was.clone())
                .map(|now| self.retyped(id, &was, now)),
        )
    }

    /// [`Self::constant`] as its node.
    pub(super) fn constant_node(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        let id = self.expr(rc, n, Pos::Whole)?;
        if self.reads(id) {
            return self.error(
                self.span(n),
                "this value is read before the program runs: it must be a constant",
            );
        }
        self.check_bound(rc, None, &[bind::Term::Expr(id)])?;
        Ok(id)
    }

    /// A term as its node (R-211 step 7), the reads it makes held by the
    /// node. The helper statements made while it is built are its terms'.
    pub(super) fn expr(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let made = self.helpers.len();
        let id = self.expr_of(rc, n, pos);
        if let Some(s) = &mut self.gather {
            s.term(made..self.helpers.len());
        }
        id
    }

    /// Whether the node `id` reads anything: whether its lowering writes
    /// a read before the term (it is no constant).
    pub(super) fn reads(&self, id: ExprId) -> bool {
        !crate::program::lower_expr(&self.program, id).1.is_empty()
    }

    pub(super) fn expr_of(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        match n.kind() {
            LITERAL => self.literal(rc, n),
            PAREN => {
                let inner = terms(n).next().ok_or(Skip)?;
                self.bind(false, |l| l.expr(rc, &inner, Pos::Content))
            }
            BIN_EXPR => self.binary(rc, n),
            UNARY_EXPR => self.unary(rc, n),
            RANGE => self.range(rc, n),
            SPREAD => self.misplaced_spread(n),
            TUPLE => self.tuple_value(n),
            CHAIN | CALL_CHAIN => self.chain_term(rc, n, pos),
            CALL => self.call(rc, n, pos),
            LIST if !n.children().any(|c| c.kind() == SPREAD) => self.list_expr(rc, n, pos),
            LIST => self.list(rc, n, pos),
            OBJECT => self.object(rc, n, pos),
            COMPREHENSION => self.comprehension(rc, n, pos),
            k => {
                let span = self.span(n);
                self.error(span, format!("unexpected {k:?} as a term"))
            }
        }
    }

    /// `f` with a builder of the statement's nodes at `span`, its variables
    /// the statement's (none outside one).
    pub(super) fn building<T>(
        &mut self,
        rc: &Rc,
        span: Span,
        f: impl FnOnce(&mut crate::program::Builder) -> T,
    ) -> T {
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        match self.gather.as_mut() {
            Some(g) => {
                let mut b = g.builder(&mut self.program, span, &written);
                let out = f(&mut b);
                g.done(b);
                out
            }
            None => f(&mut crate::program::Builder::new(
                &mut self.program,
                span,
                &written,
            )),
        }
    }

    /// A node of the program at `span`.
    pub(super) fn node(&mut self, span: Span, kind: ExprKind) -> ExprId {
        self.program.expr_node(span, kind)
    }

    /// The constant string `s`'s node at `span`.
    pub(super) fn str_node(&mut self, span: Span, s: &str) -> ExprId {
        self.node(span, ExprKind::Lit(Value::Str(s.to_string())))
    }

    /// The goal `kind` at `span`.
    pub(super) fn goal(&mut self, rc: &Rc, span: Span, kind: GoalKind) -> GoalId {
        self.building(rc, span, |b| b.goal_node(span, kind))
    }

    /// Whether a read built so far in the statement tests the copy in the
    /// scope `scope` itself (an `Output` holding its test).
    pub(super) fn held(&self, scope: ExprId) -> bool {
        self.gather.as_ref().is_some_and(|g| g.holds(scope))
    }

    /// The written literal being lowered is the goal `g`, built of its
    /// terms' nodes (R-211 step 7c).
    pub(super) fn built(&mut self, g: GoalId) {
        if let Some(s) = &mut self.gather {
            s.built(g);
        }
    }

    /// What `res` reads when it is one read, its parts as resolved: the
    /// forms a literal tests or binds in place ([`Self::read_atom`]).
    pub(super) fn in_place_read(res: &Res) -> Option<PlanRead> {
        Some(match res {
            Res::Ref { typ, addr, path } if path.len() == 1 => match &path[0] {
                Seg::F(p) => PlanRead::Attr {
                    typ: *typ,
                    addr: *addr,
                    attr: crate::address::path_key(p).into_owned(),
                },
                Seg::I(_) | Seg::K(_) => return None,
            },
            Res::Output {
                inst,
                key,
                path,
                of,
                ..
            } if path.is_empty() => PlanRead::Output {
                copy: *inst,
                key: key.clone(),
                of: of.clone(),
            },
            Res::World { typ, addr, path } => PlanRead::World {
                typ: typ.clone(),
                addr: *addr,
                path: path.clone(),
            },
            Res::Value { decl, via, path } if path.is_empty() => {
                PlanRead::Value(decl.clone(), via.clone())
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } if path.is_empty() => PlanRead::Lookup {
                rel: pred.clone(),
                args: args.clone(),
                out: *out,
            },
            _ => return None,
        })
    }

    /// The read `res` is, in place, at `span`: its node.
    pub(super) fn in_place(&mut self, rc: &Rc, res: &Res, span: Span) -> Option<ExprId> {
        let read = Self::in_place_read(res)?;
        Some(self.building(rc, span, |b| b.read_node(read.of())))
    }

    /// The node of the variable `v` (its lowered name) of the statement.
    pub(super) fn var_node(&mut self, rc: &Rc, span: Span, v: &str) -> ExprId {
        self.building(rc, span, |b| b.variable(v))
    }

    /// The node of a resolved name's term: a constant, a variable (a
    /// variable's type, `x in T` over several), `_`.
    pub(super) fn name_node(&mut self, rc: &Rc, span: Span, t: &Term) -> ExprId {
        match t {
            Term::Val(v) => self.node(span, ExprKind::Lit(v.clone())),
            Term::Var(v) => self.var_node(rc, span, v),
            Term::Wildcard => self.node(span, ExprKind::Hole),
            t => unreachable!("a resolved name is a constant or a variable: {t:?}"),
        }
    }

    /// A second node of the leaf `id` (a constant, a variable): a node
    /// stands in one place.
    pub(super) fn leaf_copy(&mut self, id: ExprId) -> ExprId {
        let e = self.program.exprs[id].clone();
        debug_assert!(matches!(e.kind, ExprKind::Lit(_) | ExprKind::Var(_)));
        self.program.exprs.insert(e)
    }

    /// A part of what a chain resolves to (an index, a key, an address):
    /// the term's node.
    pub(super) fn part(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        self.expr(rc, n, Pos::Content)
    }

    /// The term node `id` is, its reads aside: what a typing of a term
    /// reads (`types::operands`, `types::range_ends`).
    pub(super) fn term_of(&self, id: ExprId) -> Term {
        crate::program::lower_expr(&self.program, id).0
    }

    /// `id`, whose term is `was`, as a typing read it, `now` (an ambiguous
    /// quantity read as the other side's dimension, R-66): the node of the
    /// constant `now` where it differs.
    pub(super) fn read_as(&mut self, id: ExprId, was: &Term, now: Term) -> ExprId {
        if format!("{now:?}") == format!("{was:?}") {
            return id;
        }
        let Term::Val(v) = now else { return id };
        let span = self.program.exprs[id].span;
        self.node(span, ExprKind::Lit(v))
    }

    /// `id`, whose term is `was`, as a declared type reads it, `now`: a
    /// constant read as the type ([`Self::read_as`]), the value read `as`
    /// it (`__as(v, "T")`), a written list's or object's parts each read
    /// so.
    pub(super) fn retyped(&mut self, id: ExprId, was: &Term, now: Term) -> ExprId {
        if *was == now {
            return id;
        }
        let e = self.program.exprs[id].clone();
        match (&now, was, e.kind) {
            (Term::Func { name, args }, _, _) if name == crate::types::AS => {
                match args.as_slice() {
                    [v, Term::Val(Value::Str(ty))] if v == was => {
                        let ty = TypeExpr::Name(ty.clone());
                        self.node(e.span, ExprKind::As { value: id, ty })
                    }
                    [v, Term::Val(Value::Str(ty))] => {
                        let ty = TypeExpr::Name(ty.clone());
                        let value = self.retyped(id, was, v.clone());
                        self.node(e.span, ExprKind::As { value, ty })
                    }
                    _ => id,
                }
            }
            (Term::List(ns), Term::List(ws), ExprKind::List { parts })
                if e.hoisted.is_none()
                    && ns.len() == parts.len()
                    && ws.len() == parts.len()
                    && parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                let parts = parts
                    .into_iter()
                    .zip(ws.iter().zip(ns))
                    .map(|(p, (w, n))| match p {
                        ListPart::Elem(x) => ListPart::Elem(self.retyped(x, w, n.clone())),
                        p => p,
                    })
                    .collect();
                self.node(e.span, ExprKind::List { parts })
            }
            (Term::Obj(ns), Term::Obj(ws), ExprKind::Object { parts })
                if e.hoisted.is_none()
                    && parts.iter().all(|p| {
                        matches!(p, ObjPart::Field { key, .. }
                            if ws.contains_key(key) && ns.contains_key(key))
                    }) =>
            {
                let parts = parts
                    .into_iter()
                    .map(|p| match p {
                        ObjPart::Field {
                            key,
                            key_span,
                            value,
                            pun,
                        } => {
                            let value = self.retyped(value, &ws[&key], ns[&key].clone());
                            ObjPart::Field {
                                key,
                                key_span,
                                value,
                                pun,
                            }
                        }
                        p => p,
                    })
                    .collect();
                self.node(e.span, ExprKind::Object { parts })
            }
            _ => self.read_as(id, was, now),
        }
    }

    /// A literal: an int, a quantity (R-66), a string, an IPv6 address, a
    /// bool.
    pub(super) fn literal(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        match tokens(n).next() {
            Some(t) if t.kind() == STRING => self.string_expr(rc, &t),
            _ => self.literal_expr(n),
        }
    }

    /// A literal's node: an int, a quantity (one whose dimension its
    /// position decides kept as written), a string read whole (its holes
    /// are [`Lowerer::string_expr`]'s), a bool.
    pub(super) fn literal_expr(&mut self, n: &SyntaxNode) -> L<ExprId> {
        let span = self.span(n);
        let t = tokens(n).next().ok_or(Skip)?;
        let kind = match t.kind() {
            INT => match crate::lexer::int_value(t.text()) {
                Some(i) => ExprKind::Lit(Value::Int(i)),
                None => return self.error(span, "integer out of range"),
            },
            // A bare IPv6 address is its text, as its quoted spelling is:
            // its position reads it.
            INET => ExprKind::Lit(Value::Str(t.text().to_string())),
            QUANTITY => match crate::quantity::literal(t.text()) {
                Ok(crate::quantity::Literal::Known(q)) => ExprKind::Lit(Value::Quantity(q)),
                Ok(crate::quantity::Literal::Ambiguous) => ExprKind::Quantity {
                    text: t.text().to_string(),
                },
                Err(why) => return self.error(span, why),
            },
            STRING => ExprKind::Lit(Value::Str(self.string(&t)?)),
            TRUE_KW => ExprKind::Lit(Value::Bool(true)),
            _ => ExprKind::Lit(Value::Bool(false)),
        };
        Ok(self.node(span, kind))
    }

    /// A chain: `x.len` (R-155), a resource by its bare name given as a
    /// value (R-43), a read.
    pub(super) fn chain_term(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let span = self.span(n);
        let mut c = Chain::read(n).ok_or(Skip)?;
        // `x.len` (R-155): the length of a list, a string or an
        // object, the one field that is a computation; `x."len"`
        // is an object's key.
        let len = matches!(c.ops.last(), Some(Op::Field(f)) if f == "len")
            && tokens(n)
                .filter(|t| !t.kind().is_trivia())
                .last()
                .is_some_and(|t| t.kind() == IDENT);
        if len {
            c.ops.pop();
            let res = self.resolve(rc, &c)?;
            let base = self.realize_expr(rc, res, Pos::Content, span)?;
            let path = vec![crate::program::node::Step::Len];
            return Ok(self.node(span, ExprKind::Field { base, path }));
        }
        // A resource by its bare name, given as a value: the
        // reference, in its module or out of it (R-43).
        if pos == Pos::Value && self.resource_named(rc, &c) {
            if let Some(id) = self.deferred_ref(rc, &c.head, span) {
                return Ok(id);
            }
            let (typ, addr) = self.reference(rc, &c, span)?;
            let empty = self.str_node(span, "");
            let args = vec![typ, addr, empty];
            return Ok(self.building(rc, span, |b| b.form(crate::address::REF, args)));
        }
        let res = self.resolve(rc, &c)?;
        if let Res::Ref { path, .. } = &res
            && matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
        {
            return self.identity_read(n, span);
        }
        self.realize_expr(rc, res, pos, span)
    }

    /// A call of a function, with its named arguments; `env.var`, a
    /// loader, `ref`. A call of a function is its node, its arguments'
    /// nodes; a call the lowering writes as a form of its own (an
    /// operator's function, `ref`, `__path`, ..) and the others are still
    /// built from what they lower to.
    pub(super) fn call(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let span = self.span(n);
        if let Some(id) = self.env_var_call(rc, n, pos) {
            return id;
        }
        if let Some(id) = self.loader_call(rc, n) {
            return id;
        }
        // A let with parameters (R-187).
        if let Some(id) = self.function_call(rc, n) {
            return id;
        }
        let name = self.callee(n);
        let Some(name) = name else {
            return self.error(span, "a function is named by a plain name");
        };
        if name == "ref"
            && let Some(list) = node(n, ARG_LIST)
            && terms(&list).count() == 1
        {
            return self.ref_call(rc, &list, span);
        }
        let named: Vec<(String, SyntaxNode)> = node(n, ARG_LIST)
            .into_iter()
            .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG))
            .filter_map(|a| Some((word_text(&a, 0), terms(&a).next()?)))
            .collect();
        let declared = crate::functions::get(&name).filter(|f| !f.internal);
        if !named.is_empty() && declared.is_none() {
            return self.error(
                span,
                format!(
                    "`{name}` is a function here: named arguments name a relation's \
                     columns in an atom"
                ),
            );
        }
        if !self.lenient && self.calls != Calls::Data && !crate::functions::callable(&name) {
            let at = n
                .children()
                .find_map(|c| Chain::of(&c))
                .map_or(span, |c| self.span_of(c.range));
            self.module_call(rc, n)?;
            self.near_function(rc.scope, &name, at)?;
        }
        self.check_function(&name, span);
        let args = self.call_args(rc, n, (declared, named), span)?;
        let was: Vec<Term> = args.iter().map(|&a| self.term_of(a)).collect();
        self.check_aggregated(&name, &was, span);
        // `cloud_ref(T, name, path)`, a form of the language, is
        // the lowering's `__cloud_ref` (R-155).
        let name = match name.as_str() {
            "cloud_ref" => crate::address::CLOUD_REF.to_string(),
            "ref" => crate::address::REF.to_string(),
            _ => name,
        };
        let now = self.typed_args(&name, was.clone(), span)?;
        let args: Vec<ExprId> = args
            .into_iter()
            .zip(was.iter().zip(now))
            .map(|(a, (was, now))| self.read_as(a, was, now))
            .collect();
        Ok(self.building(rc, span, |b| b.form(&name, args)))
    }

    /// The arguments of the call `n` of the std function `declared`, by
    /// position then by name (`named`), a parameter left out before a
    /// named one its default: each its node.
    fn call_args(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        (declared, named): (
            Option<&'static crate::functions::Function>,
            Vec<(String, SyntaxNode)>,
        ),
        span: Span,
    ) -> L<Vec<ExprId>> {
        let mut args = self.bind(false, |l| l.arg_exprs(rc, n))?;
        let mut defaults = Vec::new();
        if let (false, Some(f)) = (named.is_empty(), declared) {
            let mut given = Vec::new();
            for (k, t) in named {
                given.push((k, Some(self.bind(false, |l| l.arg_expr(rc, &t))?)));
            }
            let placed = crate::functions::with_named(f, args, given, |v| {
                defaults.push(v);
                None
            });
            args = match placed {
                Ok(a) => a,
                Err(why) => return self.error(span, why),
            };
        }
        let mut defaults = defaults.into_iter();
        Ok(args
            .into_iter()
            .map(|a| match a {
                Some(a) => a,
                None => {
                    let v = defaults.next().expect("a default per argument left out");
                    self.node(span, ExprKind::Lit(v))
                }
            })
            .collect())
    }

    /// A call's positional arguments, each its node.
    pub(super) fn arg_exprs(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Option<ExprId>>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for t in terms(&list) {
            out.push(Some(self.arg_expr(rc, &t)?));
        }
        Ok(out)
    }

    /// A call's argument `t`: read as any term is, a resource by its
    /// bare name the reference, as it is given to a block's entry.
    pub(super) fn arg_expr(&mut self, rc: &mut Rc, t: &SyntaxNode) -> L<ExprId> {
        let named = Chain::of(t).is_some_and(|c| self.resource_named(rc, &c));
        let pos = if named { Pos::Value } else { Pos::Content };
        self.expr(rc, t, pos)
    }

    /// Whether `c` is a resource by its bare name: one in scope that no
    /// variable or value of the name hides.
    pub(super) fn resource_named(&self, rc: &Rc, c: &Chain) -> bool {
        c.is_bare()
            && c.head != "_"
            && !rc.vars.contains_key(&c.head)
            && !rc.types.contains_key(&c.head)
            && !self.is_value(rc.scope, &c.head)
            && self.resource(rc.scope, &c.head).is_some()
    }

    /// `[a, b]`, written element by element: its node, each element's.
    /// A list with a spread is `spread.rs`'s.
    pub(super) fn list_expr(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let mut parts = Vec::new();
        for t in terms(n) {
            parts.push(ListPart::Elem(self.expr(rc, &t, pos)?));
        }
        Ok(self.node(self.span(n), ExprKind::List { parts }))
    }

    /// An object, its keys given or computed (`{ "${k}": v }`), its
    /// fields written or spread (`{ ..base, k: v }`, R-199): its node,
    /// each field's value its node.
    pub(super) fn object(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        self.written_twice(n)?;
        if n.children().any(|c| c.kind() == SPREAD) {
            return self.spread_object(rc, n, pos);
        }
        let fields: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == OBJECT_FIELD).collect();
        let parts = self.object_fields(rc, &fields, pos)?;
        Ok(self.node(self.span(n), ExprKind::Object { parts }))
    }

    /// Written fields, each its key and its value's node; a key with holes
    /// (`{ "${k}": v }`) is computed, so each key of the fields is (the
    /// object is built at run time, `functions::OBJECT`).
    pub(super) fn object_fields(
        &mut self,
        rc: &mut Rc,
        fields: &[SyntaxNode],
        pos: Pos,
    ) -> L<Vec<ObjPart>> {
        let computed = self.computed_keys(fields);
        let mut parts = Vec::new();
        for f in fields {
            let k = tokens(f).next().ok_or(Skip)?;
            let key_span = self.span_of(k.text_range());
            let computed_key = match (computed, k.kind()) {
                (false, _) => None,
                (true, STRING) => Some(self.string_expr(rc, &k)?),
                (true, _) => Some(self.node(key_span, ExprKind::Lit(Value::Str(k.text().into())))),
            };
            let key = match k.kind() {
                _ if computed => String::new(),
                STRING => self.string(&k)?,
                _ => k.text().to_string(),
            };
            let (value, pun) = match terms(f).next() {
                Some(t) => (self.expr(rc, &t, pos)?, false),
                // `{ a }` is `{ a: a }`.
                None => {
                    let name = if computed { k.text() } else { &key };
                    (self.punned(rc, name, &k, pos)?, true)
                }
            };
            parts.push(match computed_key {
                Some(key) => ObjPart::Computed { key, value },
                None => ObjPart::Field {
                    key,
                    key_span,
                    value,
                    pun,
                },
            });
        }
        Ok(parts)
    }

    /// Whether a field's key has holes (`{ "${k}": v }`): the object is
    /// built at run time (`functions::OBJECT`).
    pub(super) fn computed_keys(&self, fields: &[SyntaxNode]) -> bool {
        fields
            .iter()
            .filter_map(|f| tokens(f).next())
            .any(|k| k.kind() == STRING && !self.text && has_hole(k.text()))
    }

    /// `{ a }`'s value: the name `a` read.
    pub(super) fn punned(
        &mut self,
        rc: &mut Rc,
        key: &str,
        k: &SyntaxToken,
        pos: Pos,
    ) -> L<ExprId> {
        let c = Chain {
            head: key.to_string(),
            head_kind: k.kind(),
            call: None,
            range: k.text_range(),
            ops: Vec::new(),
        };
        let res = self.resolve(rc, &c)?;
        self.realize_expr(rc, res, pos, self.span_of(k.text_range()))
    }

    /// A list comprehension: its body resolved nested, its item over it.
    /// Its node's clause is the goals its literals were built as; outside
    /// a statement, or a literal not built, the clause is built from what
    /// the body lowered to.
    pub(super) fn comprehension(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let span = self.span(n);
        let body_node = node(n, BODY).ok_or(Skip)?;
        let saved = (
            std::mem::take(&mut rc.reads),
            std::mem::take(&mut rc.values),
        );
        if let Some(g) = &mut self.gather {
            g.open();
        }
        self.nested += 1;
        let body = self.body(rc, &body_node);
        self.nested -= 1;
        let at = self.span_of(body_node.text_range());
        let clause = match &mut self.gather {
            Some(g) => g.close(&mut self.program, at),
            None => None,
        };
        body?;
        let item_node = terms(n).next().ok_or(Skip)?;
        // A resource collected into a value is its reference.
        let item_pos = if pos == Pos::Value {
            Pos::Value
        } else {
            Pos::Whole
        };
        let tests = std::mem::take(&mut rc.tests);
        let item = self.bind(false, |l| l.expr(rc, &item_node, item_pos))?;
        (rc.reads, rc.values) = saved;
        // A test the item made before a read it holds none of is the
        // body's last goal.
        let item_tests = std::mem::replace(&mut rc.tests, tests);
        let clause = clause.unwrap_or_else(|| self.building(rc, at, |b| b.clause_of(Vec::new())));
        let held = |l: &Self, s: Option<ExprId>| s.is_some_and(|s| l.held(s));
        let unheld: Vec<GoalId> = item_tests
            .into_iter()
            .filter(|&(s, _)| !held(self, s))
            .map(|(_, g)| g)
            .collect();
        self.program.clauses[clause].goals.extend(unheld);
        Ok(self.node(span, ExprKind::Comprehension { item, clause }))
    }

    /// Arithmetic; `us-east` and `us-test-1a`, meant as strings, are
    /// errors that say so.
    pub(super) fn binary(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let op = tokens(n).next().ok_or(Skip)?;
        // `us-test-1a`, names and a quantity with no spaces (R-66
        // lexes `1a` as one token): meant as a string.
        let word = |k: SyntaxKind| matches!(k, IDENT | QUANTITY | INT | MINUS) || k.is_keyword();
        let toks: Vec<_> = n
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .collect();
        if op.kind() == MINUS
            && toks.iter().all(|t| word(t.kind()))
            && toks.iter().any(|t| t.kind() == QUANTITY)
            && toks.iter().any(|t| t.kind() == IDENT)
        {
            let d = Diagnostic::error(span, format!("`{}` is arithmetic, not a name", n.text()))
                .with_help(format!(
                    "`-` subtracts here, a quantity from a name; a name with a `-` in it is \
                 a string, \"{}\"",
                    n.text()
                ));
            self.diags.push(d);
            return Err(Skip);
        }
        // `us-east` with no spaces: meant as a string.
        if op.kind() == MINUS
            && ts.iter().all(|t| t.kind() == CHAIN)
            && ts[0].text_range().end() == op.text_range().start()
            && op.text_range().end() == ts[1].text_range().start()
        {
            let d = Diagnostic::error(span, format!("`{}` is arithmetic on two names", n.text()))
                .with_help(format!(
                    "`-` subtracts `{}` from `{}`; a name with a `-` in it is a string, \
                 \"{}\"",
                    ts[1].text(),
                    ts[0].text(),
                    n.text()
                ));
            self.diags.push(d);
            return Err(Skip);
        }
        let bin = match op.kind() {
            PLUS => BinOp::Add,
            MINUS => BinOp::Sub,
            STAR => BinOp::Mul,
            SLASH => BinOp::Div,
            _ => BinOp::Mod,
        };
        let a = self.bind(false, |l| l.expr(rc, &ts[0], Pos::Content))?;
        let b = self.bind(false, |l| l.expr(rc, &ts[1], Pos::Content))?;
        let (ta, tb) = (self.term_of(a), self.term_of(b));
        match crate::types::operands(op.text(), ta.clone(), tb.clone()) {
            Ok((x, y)) => {
                let lhs = self.read_as(a, &ta, x);
                let rhs = self.read_as(b, &tb, y);
                Ok(self.node(span, ExprKind::Binary { op: bin, lhs, rhs }))
            }
            Err(why) => self.error(span, why),
        }
    }

    /// A negation: of a literal, its value.
    pub(super) fn unary(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        let span = self.span(n);
        let inner = terms(n).next().ok_or(Skip)?;
        let e = self.bind(false, |l| l.expr(rc, &inner, Pos::Content))?;
        let kind = match &self.program.exprs[e].kind {
            ExprKind::Lit(Value::Int(i)) if inner.kind() == LITERAL => {
                ExprKind::Lit(Value::Int(-i))
            }
            ExprKind::Lit(Value::Quantity(q)) if inner.kind() == LITERAL => {
                match q.checked_mul(-1) {
                    Some(q) => ExprKind::Lit(Value::Quantity(q)),
                    None => return self.error(span, "quantity out of range"),
                }
            }
            _ => ExprKind::Neg(e),
        };
        Ok(self.node(span, kind))
    }

    /// `lo..hi` (its end left out) or `lo..=hi` (its end in it): a range
    /// (R-180), the value when its ends are known, else
    /// `__range(lo, hi, inclusive)`. An ambiguous quantity end is read as
    /// the other end's (`100m..=1` is a cpu's).
    pub(super) fn range(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        let span = self.span(n);
        let ends: Vec<SyntaxNode> = terms(n).collect();
        let [lo, hi] = ends.as_slice() else {
            return Err(Skip);
        };
        let lo = self.expr(rc, lo, Pos::Content)?;
        let hi = self.expr(rc, hi, Pos::Content)?;
        let (tl, th) = (self.term_of(lo), self.term_of(hi));
        let (rl, rh) = crate::types::range_ends(tl.clone(), th.clone());
        let inclusive = tokens(n).any(|t| t.kind() == DOT2_EQ);
        if let (Term::Val(a), Term::Val(b)) = (&rl, &rh) {
            match crate::range::Range::new(a.clone(), b.clone(), inclusive) {
                Ok(r) => return Ok(self.node(span, ExprKind::Lit(r.into()))),
                // Known ends that make no range: why.
                Err(why) => return self.error(span, format!("`{}` is no range: {why}", n.text())),
            }
        }
        let (lo, hi) = (self.read_as(lo, &tl, rl), self.read_as(hi, &th, rh));
        Ok(self.node(span, ExprKind::Range { lo, hi, inclusive }))
    }

    /// A string's node: its text, or its pieces of text and holes.
    pub(super) fn string_expr(&mut self, rc: &mut Rc, t: &SyntaxToken) -> L<ExprId> {
        use crate::program::node::Piece as Part;
        let text = t.text();
        let span = self.span_of(t.text_range());
        if self.text || !text.contains("${") {
            let s = self.string(t)?;
            return Ok(self.node(span, ExprKind::Lit(Value::Str(s))));
        }
        let base: u32 = t.text_range().start().into();
        let mut parts = Vec::new();
        let ps = match scan(text) {
            Ok(ps) => ps,
            Err(at) => {
                let at = base + at as u32;
                let span = self.span_of(rowan::TextRange::new(at.into(), (at + 2).into()));
                return self.error(
                    span,
                    "an interpolation `${` is never closed; a literal `${` is `$${`",
                );
            }
        };
        for p in ps {
            match p {
                Piece::Text(lit) => {
                    let s = unescape(&format!("\"{lit}\"")).map_err(|e| {
                        self.diags.push(Diagnostic::error(span, e));
                        Skip
                    })?;
                    if s.contains("%s") {
                        return self.error(span, "an interpolated string cannot hold `%s`");
                    }
                    if !s.is_empty() {
                        parts.push(Part::Text(s));
                    }
                }
                Piece::Hole(hole, at) => {
                    let at = base + at as u32;
                    parts.push(Part::Hole(self.bind(false, |l| l.hole(rc, hole, at))?));
                }
            }
        }
        if !parts.iter().any(|p| matches!(p, Part::Hole(_))) {
            let s: String = parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(s) => s,
                    Part::Hole(_) => unreachable!("no hole"),
                })
                .collect();
            return Ok(self.node(span, ExprKind::Lit(Value::Str(s))));
        }
        Ok(self.node(span, ExprKind::Interp { parts }))
    }

    /// An interpolation hole: a term, read now (a content position).
    pub(super) fn hole(&mut self, rc: &mut Rc, src: &str, at: u32) -> L<ExprId> {
        // A variable `in T` types is a reference: it interpolates as its
        // address, `T["A"]`, as an untyped one does (R-42).
        let parse = parse::parse_term(src.trim());
        let typed = terms(&parse.syntax())
            .next()
            .and_then(|t| Chain::of(&t))
            .is_some_and(|c| c.is_bare() && rc.types.contains_key(&c.head));
        let pos = if typed { Pos::Value } else { Pos::Content };
        self.text_expr(rc, src, at, pos)
    }

    /// A block entry's value: its term, or, for an entry that is only a
    /// simple name, the pun: the name as a term, resolved where the value
    /// would be (R-33), `zone` for `zone`. A pun is one name on both sides
    /// (R-216): `spec.selector.color` alone is an error.
    pub(super) fn entry_value(
        &mut self,
        rc: &mut Rc,
        a: &SyntaxNode,
        pos: Pos,
    ) -> L<(Term, ExprId)> {
        if let Some(t) = terms(a).next() {
            return self.arg_node(rc, &t, pos);
        }
        let path = node(a, BLOCK_PATH).ok_or(Skip)?;
        let seg = tokens(&path).last().ok_or(Skip)?;
        let named = seg.kind().is_word()
            && !matches!(
                seg.kind(),
                NOT_KW | IN_KW | HAS_KW | WHERE_KW | IF_KW | TRUE_KW | FALSE_KW
            );
        let written = path.text().to_string();
        let one = tokens(&path).nth(1).is_none();
        if named && !one {
            let parent = written[..written.len() - seg.text().len()].trim_end_matches('.');
            let d = Diagnostic::error(
                self.span(a),
                format!("`{written}` alone is not a pun: a pun is one simple name on both sides"),
            )
            .with_help(format!(
                "write `{written} = {seg}`, or `{parent} = {{ {seg} }}`"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        if !named || !one {
            let d = Diagnostic::error(
                self.span(a),
                format!("`{written}` names no value: an entry is `path = term`"),
            )
            .with_help(format!("give it a value: `{written} = ..`"));
            self.diags.push(d);
            return Err(Skip);
        }
        let at: u32 = seg.text_range().start().into();
        let id = self.text_expr(rc, seg.text(), at, pos)?;
        Ok((self.term_of(id), id))
    }

    /// The term `src`, written at byte `at`, as its node in `pos`.
    pub(super) fn text_expr(&mut self, rc: &mut Rc, src: &str, at: u32, pos: Pos) -> L<ExprId> {
        let parse = parse::parse_term(src);
        let saved = self.offset;
        self.offset += at;
        let r = (|| {
            if let Some(e) = parse.errors.first() {
                let span = Span {
                    file: self.file,
                    start: self.offset + e.start as u32,
                    end: self.offset + e.end as u32,
                    origin: 0,
                };
                let msg = e
                    .message
                    .replace("the end of the file", "the end of the hole");
                return self.error(span, format!("in an interpolation: {msg}"));
            }
            let root = parse.syntax();
            let t = terms(&root).next().ok_or(Skip)?;
            self.expr(rc, &t, pos)
        })();
        self.offset = saved;
        r
    }
}
