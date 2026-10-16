//! The declarations that lower to themselves (H-11, R-34, R-155): an
//! extern, a type block and its refinements, a relation's `decl`, a
//! relation input and its table, a rank; the stack's settings; and a
//! type as written, its aliases and references resolved.

use super::*;

impl Lowerer<'_> {
    /// `extern f(+a: T, -b)`: a relation a provider answers, its columns
    /// as declared (R-211 step 5: an item carrying them).
    pub(super) fn extern_item(&mut self, n: &SyntaxNode) -> L<ItemId> {
        let span = self.span(n);
        let name = dotted_text(n, 1);
        self.check_extern(&name, span)?;
        let args = n
            .children()
            .filter(|c| c.kind() == BIND_ARG)
            .map(|b| BindArg {
                input: tokens(&b).next().is_some_and(|t| t.kind() == PLUS),
                name: word_text(&b, 0),
                ty: node(&b, TYPE_EXPR).map(|t| self.type_expr(&t)),
            })
            .collect();
        // R-60: one persistence concept, `memo.first`, said by the
        // program where a value is kept, not by the extern.
        if let Some(t) = tokens(n).find(|t| t.text() == "persist") {
            let r = t.text_range();
            let at = Span {
                start: u32::from(r.start()),
                end: u32::from(r.end()),
                ..span
            };
            let d = Diagnostic::error(at, format!("extern {name}: `persist` is gone")).with_help(
                "keep an answer where it is read: `memo.first(KEY, CANDIDATE, VALUE)` keeps the \
                 first candidate given for KEY",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let kind = crate::program::ItemKind::Extern { name, args };
        Ok(self.program.item(span, self.item_scope, kind))
    }

    /// `type T { p: T flag* }`: a type block, its attributes as read
    /// (R-211 step 5: an item carrying them).
    pub(super) fn type_block(&mut self, n: &SyntaxNode, scope: ScopeId) -> L<ItemId> {
        let name = dotted_text(n, 1);
        let attrs = self.attr_decls(n, scope)?;
        let kind = crate::program::ItemKind::TypeBlock { name, attrs };
        let item = self.program.item(self.span(n), self.item_scope, kind);
        // The helpers its refinements made (a `not`'s rule) follow it.
        if !self.helpers.is_empty() {
            let made = std::mem::take(&mut self.helpers);
            self.program
                .terms_made
                .insert(crate::program::NodeId::Item(item), made);
        }
        Ok(item)
    }

    /// `decl p(a, b)` declares the relation `p/2` by its columns (H-11):
    /// one no rule of the program defines is fed from outside (a provider,
    /// a given fact); `decl p(a, b) mixed` lets it have both facts and
    /// rules. The column names are the named-argument form's.
    pub(super) fn decl(&mut self, n: &SyntaxNode, scope: ScopeId) -> ItemId {
        let pred = dotted_text(n, 1);
        let binds: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == BIND_ARG).collect();
        let columns = binds
            .iter()
            .map(|b| {
                (
                    word_text(b, 0),
                    node(b, TYPE_EXPR).map(|t| self.type_expr(&t)),
                )
            })
            .collect();
        let mixed = tokens(n).last().is_some_and(|t| t.text() == "mixed");
        // A relation the scope's own statements do not define: one in
        // another module of the same name is another relation (R-65).
        let fed = !self.names(self.decl_scope(scope)).defines(&pred);
        let span = self.span(n);
        let rel = crate::program::node::RelRef::own(pred, span);
        let kind = crate::program::ItemKind::Decl {
            rel,
            columns,
            mixed,
            fed,
        };
        self.program.item(span, self.item_scope, kind)
    }

    /// `input p from TERM [where B]` (R-55): rows of the relation `p`, its
    /// columns `decl p(..)`'s. Several lines are one relation, their rows
    /// together, and facts the program states join them. `input p` alone,
    /// in a module or a component, is a relation its user gives the rows
    /// of, in the `use` or `instance` block.
    pub(super) fn relation_input(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<ItemId> {
        let span = self.span(n);
        let pred = word_text(n, 1);
        let module = self.module_path(self.file).is_some()
            || n.parent().is_some_and(|p| p.kind() != SOURCE_FILE);
        let Some(source) = terms(n).next() else {
            if !module {
                return self.error(
                    span,
                    format!(
                        "`input {pred}` with no `from` is a module's relation, which its user \
                         gives: a stack gives a relation's rows, `input {pred} from ..`"
                    ),
                );
            }
            let arity = self
                .relation_decl(scope, &pred)
                .map_or(0, |d| d.children().filter(|c| c.kind() == BIND_ARG).count());
            let r = crate::program::build::RelationInputLowered {
                span,
                scope: self.item_scope,
                rel: pred,
                arity,
                mixed: false,
                from: None,
            };
            let mut b = crate::program::Builder::new(&mut self.program, span, &|_| true);
            return Ok(b.relation_input_item(r));
        };
        if module {
            return self.error(
                span,
                format!(
                    "a module's relation is given by its user: declare `input {pred}`, and its \
                     user writes `{pred} from ..` in the `use` block"
                ),
            );
        }
        self.reject_facts(&source)?;
        let (cols, arity) = match self.relation_decl(scope, &pred) {
            // No `decl`: the columns are the first source's (R-34).
            None => {
                let cols = match self.read_columns.get(&pred) {
                    Some(c) => c.clone(),
                    None => {
                        let c = self.first_source_columns(&pred, n, &source, span)?;
                        self.read_columns.insert(pred.clone(), c.clone());
                        c
                    }
                };
                let arity = cols.len();
                (cols, arity)
            }
            Some(decl) => {
                let cols = self.table_columns(&pred, &decl)?;
                (
                    cols,
                    decl.children().filter(|c| c.kind() == BIND_ARG).count(),
                )
            }
        };
        self.table_item(n, scope, outer, (pred, arity, true), cols)
    }

    /// `p from DOC [where B]`'s rows (R-39), its columns `cols`, as a
    /// `RelationInput` item: a stack's own relation (`mixed`), or the rows
    /// a copy's block gives its module's relation.
    pub(super) fn table_item(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        (pred, arity, mixed): (String, usize, bool),
        cols: Vec<BindArg>,
    ) -> L<ItemId> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let rows = self.table(&mut rc, &pred, cols, n, body, span)?;
        let r = crate::program::build::RelationInputLowered {
            span,
            scope: self.item_scope,
            rel: pred,
            arity,
            mixed,
            from: Some(crate::program::build::RelationRows {
                clause,
                externs: rows.externs.clone(),
                head: &rows.head,
                rows: rows.rows,
                made: &self.helpers[rows.made..],
            }),
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.relation_input_item(r);
        gather.done(b);
        Ok(item)
    }

    /// The columns of `input p from FORMAT("path")` with no `decl`: the
    /// first row's of the document at `path`, read now (R-34). A source the
    /// compiler cannot read (a `git(..)` one, a path with holes, a missing
    /// file) is an error that says to declare the columns.
    pub(super) fn first_source_columns(
        &mut self,
        pred: &str,
        n: &SyntaxNode,
        src: &SyntaxNode,
        span: Span,
    ) -> L<Vec<BindArg>> {
        let declare = format!("declare its columns: `decl {pred}(a: T, ..)`");
        if let Some(e) = self.old_loader(src) {
            return e.map(|_| Vec::new());
        }
        let read = self.loader_of(src).filter(|_| node(n, SELECTOR).is_none());
        let format = read.as_ref().map(|r| r.0.clone()).unwrap_or_default();
        let args: Vec<SyntaxNode> = read
            .and_then(|(_, r)| node(&r, ARG_LIST))
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let path = match args.as_slice() {
            [a] if a.kind() == LITERAL && !format.is_empty() => tokens(a)
                .find(|t| t.kind() == STRING)
                .filter(|t| !has_hole(t.text()))
                .and_then(|t| string_value(t.text()).ok()),
            _ => None,
        };
        let Some(path) = path else {
            let d = Diagnostic::error(
                span,
                format!(
                    "input {pred} from ..: {pred} has no `decl`, so its columns are its first \
                     source's, and that is no file path the compiler can read"
                ),
            )
            .with_help(declare);
            self.diags.push(d);
            return Err(Skip);
        };
        let base = crate::diag::location(span)
            .map(|(file, _, _)| crate::project::base_of(std::path::Path::new(&file)))
            .unwrap_or_default();
        let read = std::fs::read_to_string(base.join(&path))
            .map_err(anyhow::Error::from)
            .and_then(|text| crate::infer::document_columns(&format, pred, &text));
        match read {
            Ok(cols) => Ok(cols
                .into_iter()
                .map(|(name, ty)| BindArg {
                    input: false,
                    name,
                    ty,
                })
                .collect()),
            Err(e) => {
                let d = Diagnostic::error(
                    span,
                    format!(
                        "input {pred} from ..: {pred} has no `decl`, so its columns are its first \
                         source's, and {path} cannot be read for them: {e:#}"
                    ),
                )
                .with_help(declare);
                self.diags.push(d);
                Err(Skip)
            }
        }
    }

    /// The `decl` of the relation `pred` in scope.
    pub(super) fn relation_decl(&self, scope: ScopeId, pred: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).decl(pred).cloned())
    }

    pub(super) fn rank_tok(&mut self, n: &SyntaxNode) -> L<Option<Rank>> {
        let Some(t) = tokens(n).find(|t| t.kind() == RANK) else {
            return Ok(None);
        };
        match t.text() {
            "@default" => Ok(Some(Rank::Default)),
            "@override" => Ok(Some(Rank::Override)),
            other => self.error(
                self.span_of(t.text_range()),
                format!("unknown rank `{other}`: a rank is `@default` or `@override`"),
            ),
        }
    }

    pub(super) fn attr_decls(&mut self, n: &SyntaxNode, scope: ScopeId) -> L<Vec<AttrDecl>> {
        let mut out = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let path = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let ty = node(&a, TYPE_EXPR).map(|t| self.type_expr(&t));
            let flags = tokens(&a)
                .filter(|t| t.kind() == IDENT)
                .map(|t| t.text().to_string())
                .collect();
            let refinement = self.refinement(&a, scope, None)?;
            let children = self.attr_decls(&a, scope)?;
            out.push(AttrDecl {
                path,
                ty,
                flags,
                refinement,
                children,
                span: self.span(&a),
            });
        }
        Ok(out)
    }

    /// A refinement's `check` body. An input's (`checked` its name, a
    /// field's name) is a clause like any other: the value checked is its
    /// name's text, every other name in scope reads as it would anywhere
    /// (a sibling input, an object's field by its path, a `let`). A type
    /// block's names are their own text (its attributes).
    pub(super) fn refinement(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        checked: Option<&str>,
    ) -> L<Vec<Lit>> {
        let Some(b) = node(n, REFINEMENT).and_then(|w| node(&w, BODY)) else {
            return Ok(Vec::new());
        };
        let saved = (self.lenient, self.checked.take());
        self.lenient = true;
        self.checked = checked.map(str::to_string);
        let mut rc = Rc {
            scope,
            ..Rc::default()
        };
        let r = self.body(&mut rc, &b);
        (self.lenient, self.checked) = saved;
        r
    }

    /// The stack's settings as dform.toml gives them (R-29): each a
    /// constant, held by the program's `stack`.
    pub(super) fn stack_settings(&mut self, st: &StackSource, entries: &[usize]) -> Option<Config> {
        let &first = entries.first()?;
        let scope = self.file_scope(self.units[first].file);
        let (saved_file, saved_offset) = (self.file, self.offset);
        let mut config = Vec::new();
        for s in &st.settings {
            let (text, offset) = match &s.value {
                SettingValue::Plain(t) => {
                    config.push((s.key.clone(), t.clone(), s.span));
                    continue;
                }
                SettingValue::Term { text, offset } => (text.replace("{stack}", &st.name), *offset),
            };
            self.file = s.span.file;
            self.offset = offset;
            let _ = self.setting_term(&text, s.span).and_then(|src| {
                let mut rc = self.rc(&src, scope, &Rc::default());
                self.calls(Calls::Data, |l| l.constant(&mut rc, &src))
                    .map(|t| config.push((s.key.clone(), t, s.span)))
            });
        }
        (self.file, self.offset) = (saved_file, saved_offset);
        Some(Config {
            name: st.name.clone(),
            of: None,
            config,
            span: st.span,
        })
    }

    /// A setting's value, parsed as a term.
    pub(super) fn setting_term(&mut self, text: &str, at: Span) -> L<SyntaxNode> {
        let parse = parse::parse_term(text);
        if let Some(e) = parse.errors.first() {
            let span = Span {
                start: self.offset + e.start as u32,
                end: self.offset + e.end as u32,
                ..at
            };
            return self.error(
                span,
                e.message
                    .replace("the end of the file", "the end of the value"),
            );
        }
        terms(&parse.syntax()).next().ok_or(Skip)
    }

    pub(super) fn type_expr(&mut self, n: &SyntaxNode) -> TypeExpr {
        self.type_expr_in(n, true)
    }

    /// An input's type: a resource type `T` is `ref(T)`, a reference to a
    /// resource of it (`input namespace: k8s.namespace`), wherever a type
    /// alias does not name it.
    pub(super) fn input_type(&mut self, n: &SyntaxNode) -> TypeExpr {
        let t = self.type_expr(n);
        self.refs_of(t)
    }

    /// `t` with each dotted name that is no alias read as `ref(T)`: a type
    /// of a namespace the compiler closes must be one it knows, and a name
    /// under a used module is a missing alias, both left to `check_type`.
    pub(super) fn refs_of(&self, t: TypeExpr) -> TypeExpr {
        match t {
            TypeExpr::Name(n) => {
                let ns = n.split('.').next().unwrap_or_default();
                let module = self.module_at(ns).is_some()
                    || self
                        .program
                        .scopes
                        .names
                        .values()
                        .any(|s| s.module(ns).is_some() || s.component(ns).is_some());
                let known = self.decls.types.contains(&n) || !self.decls.closed.contains(ns);
                if n.contains('.') && known && !module {
                    TypeExpr::Apply("ref".into(), vec![TypeExpr::Name(n)])
                } else {
                    TypeExpr::Name(n)
                }
            }
            TypeExpr::Apply(n, args) if n != "enum" && n != "ref" => {
                TypeExpr::Apply(n, args.into_iter().map(|a| self.refs_of(a)).collect())
            }
            TypeExpr::Object(fs) => {
                TypeExpr::Object(fs.into_iter().map(|(k, t)| (k, self.refs_of(t))).collect())
            }
            t => t,
        }
    }

    /// `type_expr`; `aliases`: a bare name may be a type alias (not in an
    /// `enum(..)`, whose members are values).
    pub(super) fn type_expr_in(&mut self, n: &SyntaxNode, aliases: bool) -> TypeExpr {
        let first = tokens(n).next();
        match first.as_ref().map(|t| t.kind()) {
            Some(STRING) => {
                let t = first.unwrap();
                TypeExpr::Str(self.string(&t).unwrap_or_default())
            }
            Some(L_BRACE) => TypeExpr::Object(
                n.children()
                    .filter(|c| c.kind() == OBJECT_FIELD)
                    .map(|f| {
                        let key = tokens(&f).next().map(|t| t.text().to_string());
                        let ty = node(&f, TYPE_EXPR).map(|t| self.type_expr(&t));
                        (
                            key.unwrap_or_default(),
                            ty.unwrap_or(TypeExpr::Name(String::new())),
                        )
                    })
                    .collect(),
            ),
            _ => {
                let name = dotted_text(n, 0);
                let args: Vec<TypeExpr> = n
                    .children()
                    .filter(|c| c.kind() == TYPE_EXPR)
                    .map(|c| self.type_expr_in(&c, name != "enum"))
                    .collect();
                if args.is_empty() {
                    // R-134: a uri is RFC 3986's, not a browser's url.
                    if name == "url" && aliases && self.alias(n, &name).is_none() {
                        let d = Diagnostic::error(self.span(n), "unknown type url").with_help(
                            "the type is `uri`, RFC 3986's generic syntax: `let u: uri = \
                             \"https://example.com\"`",
                        );
                        self.diags.push(d);
                    }
                    // R-180: a range of addresses is a range of ips.
                    if name == "iprange" && aliases && self.alias(n, &name).is_none() {
                        let d = Diagnostic::error(self.span(n), "unknown type iprange").with_help(
                            "a range of addresses is `range(ip)`: `let r: range(ip) = \
                                 \"10.0.0.10..=10.0.0.99\"`",
                        );
                        self.diags.push(d);
                    }
                    match aliases.then(|| self.alias(n, &name)).flatten() {
                        Some(t) => t,
                        None => TypeExpr::Name(name),
                    }
                } else if name == "enum" {
                    // `enum("a", "b")`: its members are names.
                    TypeExpr::Apply(
                        name,
                        args.into_iter()
                            .map(|a| match a {
                                TypeExpr::Str(s) => TypeExpr::Name(s),
                                other => other,
                            })
                            .collect(),
                    )
                } else {
                    // `range(T)` for an ordered `T` (R-180).
                    if name == "range"
                        && !matches!(args.as_slice(), [TypeExpr::Name(t)]
                            if crate::range::ORDERED.contains(&t.as_str()))
                    {
                        let d = Diagnostic::error(
                            self.span(n),
                            format!("`{}` is no range type", n.text()),
                        )
                        .with_help(format!(
                            "a range is of one ordered type: `range(T)`, `T` one of {}",
                            crate::range::ORDERED.join(", ")
                        ));
                        self.diags.push(d);
                    }
                    TypeExpr::Apply(name, args)
                }
            }
        }
    }
}
