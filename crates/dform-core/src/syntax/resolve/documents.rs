//! A document read as a relation's rows (R-155): a relation input's
//! source, a loader call (`yaml.decode(io.read(L))`) in a term, a
//! table's columns and selector, and the rows' goal over the extern that
//! reads the document.

use super::*;

impl Lowerer<'_> {
    /// `input p from DOC [where B]`: a table (`crate::tables`), its
    /// columns `cols`. Its rows are the answers of the extern
    /// `table.FORMAT.p`, asked once the source is known: `p(Cols) :- B,
    /// reads, Path = SOURCE, table.FORMAT.p(Path, At, Cols)`; of a
    /// document value, `table.value.p` (`doc_source`). `clause` is B's.
    pub(super) fn table(
        &mut self,
        rc: &mut Rc,
        (pred, cols): (&str, Vec<BindArg>),
        (n, clause): (&SyntaxNode, Option<ClauseId>),
        span: Span,
    ) -> L<TableRows> {
        let vars: Vec<Term> = cols
            .iter()
            .map(|c| var(&fresh(rc, &capitalise(&c.name))))
            .collect();
        let made = self.helpers.len();
        let (externs, rows) = self.doc_source(rc, n, pred, cols, vars.clone())?;
        self.check_bound(rc, clause, &[])?;
        Ok(TableRows {
            externs,
            head: atom_at(pred, vars, span),
            rows,
            made,
        })
    }

    /// A table's columns, its relation's `decl`'s: each typed, with an
    /// input's types, never a secret.
    pub(super) fn table_columns(&mut self, pred: &str, decl: &SyntaxNode) -> L<Vec<BindArg>> {
        let mut cols: Vec<BindArg> = Vec::new();
        for b in decl.children().filter(|c| c.kind() == BIND_ARG) {
            let name = word_text(&b, 0);
            let ty = node(&b, TYPE_EXPR).map(|t| self.type_expr(&t));
            let at = self.span(&b);
            if ty.is_none() {
                return self.error(
                    at,
                    format!("{pred} is read from a table: its column {name} needs a type"),
                );
            }
            if cols.iter().any(|c| c.name == name) {
                return self.error(at, format!("{pred}: two columns are named {name}"));
            }
            if let Some(Err(e)) = ty.as_ref().map(crate::inputs::check_type) {
                return self.error(at, format!("{pred}: column {name}: {e}"));
            }
            if matches!(&ty, Some(TypeExpr::Apply(t, _)) if t == "secret") {
                let d = Diagnostic::error(at, format!("{pred}: column {name} is a secret"))
                    .with_note(
                        "a table's rows are read in the clear and recorded in the plan file; \
                         a secret comes from a secret input, a data source's secret column or a std function such as `random.password`",
                    );
                self.diags.push(d);
                return Err(Skip);
            }
            cols.push(BindArg {
                input: false,
                name,
                ty,
            });
        }
        Ok(cols)
    }

    /// The rows of `table` a `from` statement `n` reads (R-39): its term a
    /// loader call, `FORMAT(PATH)` (`table_body`), or any other document
    /// value (an input, a `let`, a selection into one), read by the extern
    /// `table.value.TABLE(+doc, -at, ..)`; and its selector, `.f` and `[_]`
    /// steps into the document, in the table's name (`tables::selected`).
    pub(super) fn doc_source(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
        outs: Vec<Term>,
    ) -> L<(Vec<Stmt>, GoalId)> {
        let src = terms(n).next().ok_or(Skip)?;
        let selector = node(n, SELECTOR)
            .map(|sel| self.selector(&sel))
            .transpose()?
            .unwrap_or_default();
        let name = crate::tables::selected(table, &selector);
        self.reject_facts(&src)?;
        // A read (`io.read`, a decode of one), or a call that is no
        // function: a table's source (the error names the formats).
        let callee = (src.kind() == CALL).then(|| self.callee(&src)).flatten();
        if self.loader_of(&src).is_some()
            || callee.is_some_and(|c| crate::functions::registry().get(&c).is_none())
        {
            return self.table_body(rc, &src, &name, cols, outs);
        }
        let location = self.expr(rc, &src, Pos::Content)?;
        let path = fresh(rc, "Doc");
        let at = fresh(rc, "At");
        let ext = crate::tables::extern_name(crate::tables::VALUE, &name);
        let span = self.span(&src);
        let read = DocRead {
            location,
            path,
            at,
            name: ext.clone(),
            span,
        };
        let rows = self.rows_goal(rc, &read, &outs);
        let mut ins = vec![
            BindArg {
                input: true,
                name: "doc".into(),
                ty: None,
            },
            BindArg {
                input: false,
                name: "at".into(),
                ty: None,
            },
        ];
        ins.extend(cols);
        let decl = Stmt::ExternFn(ExternFn {
            name: ext,
            args: ins,
            span,
        });
        Ok((vec![decl], rows))
    }

    /// A read as a term (R-39, R-155): `io.read(LOCATION)` (the whole of
    /// it, a string) and a decode of one, `yaml.decode(io.read(LOCATION))`,
    /// `toml`, `json`, `csv` (a list of objects by its header), a location
    /// a path or a uri (R-153), is the document, a value: `V` reading
    /// `table.FORMAT.document(Location, At, V)`, whose answer the plan file
    /// records and the controller watches as a table's, and whose rows keep
    /// their place (R-131). `None` for any other call; an old loader
    /// (`yaml(..)`) no relation of the program names is an error naming
    /// the composition.
    pub(super) fn loader_call(&mut self, rc: &mut Rc, n: &SyntaxNode) -> Option<L<ExprId>> {
        if let Some(e) = self.old_loader(n) {
            return Some(e.map(|never| match never {}));
        }
        let (format, _) = self.loader_of(n)?;
        let table = crate::tables::DOCUMENT.to_string();
        let v = fresh(rc, &capitalise(&format));
        let cols = vec![BindArg {
            input: false,
            name: "value".into(),
            ty: Some(TypeExpr::Name("any".into())),
        }];
        let (stmts, read) = match self.doc_read(rc, n, &table, cols) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        for st in stmts {
            let known = |h: &Stmt| matches!((h, &st), (Stmt::ExternFn(a), Stmt::ExternFn(b)) if a.name == b.name);
            if !self.helpers.iter().any(known) {
                self.helpers.push(st);
            }
        }
        // `table.FORMAT.document(Path, At, V)` after `Path = LOCATION`.
        Some(Ok(self.building(rc, read.span, |b| {
            let path = b.bound_into(read.location, &read.path);
            let at = b.variable(&read.at);
            b.call_into((&read.name, Reach::Own), vec![path, at], &v)
        })))
    }

    /// A read's format and its `io.read(..)` call (R-155): `io.read(L)`
    /// is `text`, `F.decode(io.read(L))` is `F` for a format the tables
    /// read; `None` for any other term.
    pub(super) fn loader_of(&self, n: &SyntaxNode) -> Option<(String, SyntaxNode)> {
        if n.kind() != CALL {
            return None;
        }
        let name = self.callee(n)?;
        if name == crate::tables::READ {
            return Some(("text".to_string(), n.clone()));
        }
        let format = name.strip_suffix(".decode")?;
        if !crate::tables::FORMATS.contains(&format) || format == "text" {
            return None;
        }
        let list = node(n, ARG_LIST)?;
        let args: Vec<SyntaxNode> = terms(&list).collect();
        match args.as_slice() {
            [a] if a.kind() == CALL && self.callee(a).as_deref() == Some(crate::tables::READ) => {
                Some((format.to_string(), a.clone()))
            }
            _ => None,
        }
    }

    /// `yaml(LOCATION)`, `text(..)` and the other loaders a program wrote
    /// before R-155, when no relation of the program has the name: an
    /// error naming the read and its decode.
    pub(super) fn old_loader(&mut self, n: &SyntaxNode) -> Option<L<std::convert::Infallible>> {
        if n.kind() != CALL {
            return None;
        }
        let name = self.callee(n)?;
        if !crate::tables::FORMATS.contains(&name.as_str())
            || self.program.scopes.declares_relation(&name)
        {
            return None;
        }
        let arg = node(n, ARG_LIST)
            .and_then(|l| terms(&l).next())
            .map(|a| a.text().to_string())
            .unwrap_or_else(|| "LOCATION".to_string());
        let read = format!("{}({arg})", crate::tables::READ);
        let new = match name.as_str() {
            "text" => read,
            f => format!("{f}.decode({read})"),
        };
        // `secrets(..)` never was a loader: the given secrets' file is
        // read as every document is (R-108).
        let (headline, help) = match name.as_str() {
            "secrets" => (
                "a file of given secrets is read by `secrets.decode(io.read(..))`".into(),
                format!(
                    "`{new}`: the file `dform secrets set` writes, opened with the deployment's key"
                ),
            ),
            _ => (
                format!("`{name}(..)` is gone (R-155): a location is read by `io.read`"),
                format!(
                    "`{new}`: `io.read` reads the text, a format's `decode` the document in it, \
                     its rows at their lines"
                ),
            ),
        };
        let d = Diagnostic::error(self.span(n), headline).with_help(help);
        self.diags.push(d);
        Some(Err(Skip))
    }

    /// `from facts(..)` is gone (R-39): an error naming what replaces it.
    pub(super) fn reject_facts(&mut self, src: &SyntaxNode) -> L<()> {
        if src.kind() != CALL || self.callee(src).as_deref() != Some("facts") {
            return Ok(());
        }
        let d = Diagnostic::error(
            self.span(src),
            "`facts(..)` is gone (R-39): a `.df` file of facts is a module",
        )
        .with_help(
            "`use data.releases` reads data/releases.df, its relations `releases.p(..)`; rows \
             from outside are a table, `input p from csv.decode(io.read(\"data/p.csv\"))`",
        );
        self.diags.push(d);
        Err(Skip)
    }

    /// A selector's steps as text: `.teams[_].services`.
    pub(super) fn selector(&mut self, n: &SyntaxNode) -> L<String> {
        let mut out = String::new();
        for t in tokens(n) {
            match t.kind() {
                DOT | L_BRACKET | R_BRACKET => out.push_str(t.text()),
                STRING => out.push_str(&self.segment(&t)?),
                _ => out.push_str(t.text()),
            }
        }
        Ok(out)
    }

    /// A table's source, `FORMAT(LOCATION)`, a path or a uri (R-153),
    /// read and asked of its extern, its outputs `outs`;
    /// the extern's declaration.
    pub(super) fn table_body(
        &mut self,
        rc: &mut Rc,
        src: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
        outs: Vec<Term>,
    ) -> L<(Vec<Stmt>, GoalId)> {
        let (stmts, read) = self.doc_read(rc, src, table, cols)?;
        let rows = self.rows_goal(rc, &read, &outs);
        Ok((stmts, rows))
    }

    /// The rows of a document `read` reads into `outs`, `name(Path, At,
    /// outs..)` after `Path = LOCATION`: their goal.
    pub(super) fn rows_goal(&mut self, rc: &Rc, read: &DocRead, outs: &[Term]) -> GoalId {
        let rel = RelRef::own(read.name.clone(), read.span);
        self.building(rc, read.span, |b| {
            let path = b.bound_into(read.location, &read.path);
            let mut args = vec![path, b.variable(&read.at)];
            for o in outs {
                let Term::Var(v) = o else {
                    unreachable!("a document's column is read into a variable")
                };
                args.push(b.variable(v));
            }
            let args = RelArgs::Positional(args.into_iter().map(|a| b.pattern_of(a)).collect());
            b.goal_node(read.span, GoalKind::Rel { rel, args })
        })
    }

    /// The read of a document's rows [`Self::table_body`] makes: the
    /// extern declared for it, its location's node (the reads no node of
    /// it holds into `pre`), the variables it is read into and the
    /// extern's name.
    pub(super) fn doc_read(
        &mut self,
        rc: &mut Rc,
        src: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
    ) -> L<(Vec<Stmt>, DocRead)> {
        let span = self.span(src);
        let bad = |l: &mut Self, what: &str| -> L<(Vec<Stmt>, DocRead)> {
            let d = Diagnostic::error(
                span,
                format!("{what}: a table's source is FORMAT.decode(io.read(LOCATION))"),
            )
            .with_help(format!(
                "FORMAT is one of {}; LOCATION is a path, `\"data/p.csv\"`, or a uri, \
                     `\"git+https://HOST/OWNER/REPO/PATH?ref=TAG\"`, `\"ssh://USER@HOST/PATH\"`",
                crate::tables::DECODERS.join(", ")
            ));
            l.diags.push(d);
            Err(Skip)
        };
        if let Some(e) = self.old_loader(src) {
            return e.and_then(|never| match never {});
        }
        let (format, read) = match (self.loader_of(src), self.callee(src)) {
            (Some(r), _) => r,
            (None, Some(f)) if src.kind() == CALL => {
                return bad(self, &format!("unknown format {f}"));
            }
            _ => return bad(self, "not a format"),
        };
        let args: Vec<SyntaxNode> = node(&read, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let [arg] = args.as_slice() else {
            return bad(self, "`io.read` takes one location");
        };
        if arg.kind() == CALL && self.callee(arg).as_deref() == Some("git") {
            let d = Diagnostic::error(
                self.span(arg),
                "`git(..)` is gone (R-153): a repository's file is a location",
            )
            .with_help(
                "`io.read(\"git+https://HOST/OWNER/REPO/PATH?ref=TAG\")` (`git+ssh://` over \
                 ssh, `git+file:REPO/PATH?ref=TAG` for a repository in the project), read at \
                 the commit the ref names, which the plan file records",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let location = self.expr(rc, arg, Pos::Content)?;
        let path = fresh(rc, "Path");
        let at = fresh(rc, "At");
        let mut ins = vec![
            BindArg {
                input: true,
                name: "path".into(),
                ty: None,
            },
            BindArg {
                input: false,
                name: "at".into(),
                ty: None,
            },
        ];
        ins.extend(cols);
        let name = crate::tables::extern_name(&format, table);
        let decl = Stmt::ExternFn(ExternFn {
            name: name.clone(),
            args: ins,
            span,
        });
        let read = DocRead {
            location,
            path,
            at,
            name,
            span,
        };
        Ok((vec![decl], read))
    }
}
