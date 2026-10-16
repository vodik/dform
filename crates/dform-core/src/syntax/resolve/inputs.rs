//! An input (R-29, R-74, R-101, R-104): its type, default and check, an
//! input under a clause (a dependent input), an object input's fields
//! each declared, the clause it is declared under.

use super::*;

impl Lowerer<'_> {
    /// `input k: T [= d] [check B] [where G]`, `key k: T`, `input k { f:
    /// T .. }` built as an `Input` item (R-211 step 5): its default read
    /// as its type, its refinement's and its clause's goals, its fields
    /// items of their own; `lower` writes the declaration.
    pub(super) fn input(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        // `input k { f: T [= d] [check B] .. }` (R-54).
        let fields = self.input_fields(n, scope, outer, &name)?;
        let ty = match node(n, TYPE_EXPR) {
            Some(t) => self.input_type(&t),
            None => self.fields_type(&fields),
        };
        let key = is_key(n);
        if key && let Some(path) = self.module_path(self.file) {
            return self.key_in_module(n, &name, &path, span);
        }
        if key && n.parent().is_some_and(|p| p.kind() != SOURCE_FILE) {
            return self.error(
                span,
                format!(
                    "key {name} inside a block: a key selects the stack's deployment, \
                             so it is declared at the top of the stack's file"
                ),
            );
        }
        if key && matches!(&ty, TypeExpr::Apply(t, _) if t == "secret") {
            let d = Diagnostic::error(span, format!("key {name} is a secret")).with_note(
                "a key's value names the deployment: its state's directory and \
                             its registry entry",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let mut rc = self.rc(n, scope, outer);
        let default_node = match terms(n).next() {
            // A literal default is read as the declared type (R-31).
            Some(t) => match self.typed_constant(&mut rc, &t, &ty)? {
                Ok(d) => Some(d),
                Err(why) => {
                    return self.error(self.span(&t), format!("input {name} {why}"));
                }
            },
            None => None,
        };
        self.refinement(n, scope, Some(&name))?;
        let refined = self.clause_at(node(n, REFINEMENT).and_then(|r| node(&r, BODY)));
        // `input k: T where B` (R-104): declared where `B` holds. A
        // clause that reads the input itself is a check misspelled.
        if let Some(c) = node(n, CLAUSE)
            && c.descendants()
                .filter_map(|x| Chain::of(&x))
                .any(|x| x.head == name)
        {
            let head = n.text().to_string();
            let at: usize = (c.text_range().start() - n.text_range().start()).into();
            let body = c.text().to_string();
            let body = body.trim_start().trim_start_matches("where").trim();
            let d = Diagnostic::error(
                self.span(&c),
                format!(
                    "the clause of input {name} reads {name}: a clause picks where it is declared"
                ),
            )
            .with_help(format!(
                "a refinement is spelled `check` (R-1): `{} check {body}`",
                head[..at].trim()
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let guard = self.clauses(&mut rc, n)?;
        let guarded = self.clause_at(node(n, CLAUSE));
        // An input is declared where its clause holds, which every read of
        // it repeats: a clause binds no aggregate there, as a resource's
        // does not; a `let` binds it, read here.
        self.aggregate_in(guarded, span)?;
        if key && !guard.is_empty() {
            return self.error(
                span,
                format!(
                    "key {name} has a clause: a key names the deployment, so every \
                             deployment has it"
                ),
            );
        }
        let rows = self.inputs_named(n, &name, span)?;
        let i = crate::program::build::InputLowered {
            name,
            key,
            ty,
            default: default_node,
            refinement: refined,
            guard: guarded,
            fields,
            rows,
            span,
            scope: self.item_scope,
        };
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &|_| true);
        let item = b.input_item(i);
        gather.done(b);
        Ok(item)
    }

    /// The object type of an input's fields.
    pub(super) fn fields_type(&self, fields: &[ItemId]) -> TypeExpr {
        let field = |&f: &ItemId| match &self.program.items[f].kind {
            crate::program::ItemKind::Input { name, ty, .. } => (name.clone(), ty.clone()),
            _ => unreachable!("a field is an input"),
        };
        TypeExpr::Object(fields.iter().map(field).collect())
    }

    /// The clause of the body `at` just lowered, gathered (empty when no
    /// literal is written in it); none when there is no body, or nothing
    /// gathers.
    pub(super) fn clause_at(&mut self, at: Option<SyntaxNode>) -> Option<crate::program::ClauseId> {
        let span = self.span(&at?);
        let gather = self.gather.as_mut()?;
        Some(gather.body_clause(&mut self.program, span))
    }

    /// The fields of an object input's block form (R-54), each a
    /// declaration named by its field: `f: T [= d] [check B]`, a nested
    /// object `f: { .. }`. A field's default is read as its type (R-31).
    /// `path` is the object's (`db`, `db.pool`); its fields are in scope by
    /// their bare names in each field's check, as the enclosing objects'.
    pub(super) fn input_fields(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        path: &str,
    ) -> L<Vec<ItemId>> {
        let names = n
            .children()
            .filter(|c| c.kind() == ATTR_DECL)
            .filter_map(|a| node(&a, BLOCK_PATH))
            .map(|p| p.text().to_string())
            .collect();
        self.objects.push((path.to_string(), names));
        let r = self.field_decls(n, scope, outer, path);
        self.objects.pop();
        r
    }

    /// The fields of `input_fields`, with the object's in scope.
    pub(super) fn field_decls(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        object: &str,
    ) -> L<Vec<ItemId>> {
        let mut out: Vec<(String, Span, ItemId)> = Vec::new();
        let mut failed = false;
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let r = (|| {
                let span = self.span(&a);
                let path = node(&a, BLOCK_PATH).ok_or(Skip)?;
                let name = self.block_path(&path)?;
                if name.contains('.') || name.contains('[') {
                    return self.error(
                        span,
                        format!(
                            "a field is a name: a nested object is `{}: {{ .. }}`",
                            name.split('.').next().unwrap_or(&name)
                        ),
                    );
                }
                if let Some((_, first, _)) = out.iter().find(|(f, ..)| *f == name) {
                    let d = Diagnostic::error(span, format!("field {name} is declared twice"))
                        .with_label(*first, "first here");
                    self.diags.push(d);
                    return Err(Skip);
                }
                let fields = self.input_fields(&a, scope, outer, &format!("{object}.{name}"))?;
                let ty = match node(&a, TYPE_EXPR) {
                    Some(t) => self.input_type(&t),
                    None => self.fields_type(&fields),
                };
                let mut rc = self.rc(&a, scope, outer);
                let default = match terms(&a).next() {
                    Some(t) => match self.typed_constant(&mut rc, &t, &ty)? {
                        Ok(d) => Some(d),
                        Err(why) => {
                            return self.error(self.span(&t), format!("field {name} {why}"));
                        }
                    },
                    None => None,
                };
                self.refinement(&a, scope, Some(&name))?;
                let refinement = self.clause_at(node(&a, REFINEMENT).and_then(|r| node(&r, BODY)));
                let i = crate::program::build::InputLowered {
                    name: name.clone(),
                    key: false,
                    ty,
                    default,
                    refinement,
                    guard: None,
                    fields,
                    rows: None,
                    span,
                    scope: self.item_scope,
                };
                let gather = self
                    .gather
                    .as_mut()
                    .expect("a ported statement is gathered");
                let mut b = gather.builder(&mut self.program, span, &|_| true);
                let item = b.input_item(i);
                gather.done(b);
                Ok((name, span, item))
            })();
            match r {
                Ok(f) => out.push(f),
                Err(Skip) => failed = true,
            }
        }
        match failed {
            true => Err(Skip),
            false => Ok(out.into_iter().map(|(.., f)| f).collect()),
        }
    }
}
