//! A module or component used, copied or deployed (R-55, R-65, R-67,
//! R-186, R-208): `use`, `instance`, a copy and its block's rows, a copy
//! named by its clause (R-191), the instance a component is named
//! through, its inputs given once each, and a component checked against
//! its signature.

use super::*;

impl Lowerer<'_> {
    /// The checks of an input declared more than once beside `n` (R-104):
    /// each under a clause, of one type. Which declaration it is, and at
    /// the first where each is (its `__declared` row and the denies of two
    /// that both hold, `lower` writes).
    pub(super) fn inputs_named(
        &mut self,
        n: &SyntaxNode,
        name: &str,
        span: Span,
    ) -> L<Option<(usize, Vec<Span>)>> {
        let same: Vec<SyntaxNode> = n
            .parent()
            .into_iter()
            .flat_map(|p| p.children())
            .filter(|c| c.kind() == INPUT && word_text(c, 1) == name)
            .collect();
        if same.len() < 2 {
            return Ok(None);
        }
        if same.first() != Some(n) && !same.iter().all(|c| node(c, CLAUSE).is_some()) {
            let d = Diagnostic::error(
                span,
                format!("`{name}` is declared twice; give each a `where`"),
            )
            .with_label(self.span(&same[0]), "first here")
            .with_help(format!(
                "an input is declared once, or several times each under a clause that \
                     picks it (`input {name}: T where cloud == \"gcp\"`)"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let ty = |c: &SyntaxNode| {
            node(c, TYPE_EXPR).map(|t| t.text().to_string().replace(char::is_whitespace, ""))
        };
        if same.first() == Some(n) && same.iter().any(|c| ty(c) != ty(n)) {
            let mut d = Diagnostic::error(
                span,
                format!("input {name} is declared with another type in each declaration"),
            );
            for c in &same {
                d = d.with_label(
                    self.span(c),
                    format!("{name}: {}", ty(c).unwrap_or_default()),
                );
            }
            self.diags.push(d.with_help(
                "one input has one type: give each declaration the same, or each its own name",
            ));
            return Err(Skip);
        }
        let i = same.iter().position(|c| c == n).unwrap_or_default();
        let sites = match i {
            0 => same.iter().map(|c| self.span(c)).collect(),
            _ => Vec::new(),
        };
        Ok(Some((i, sites)))
    }

    /// The declarations of `name` by a `use` or an `instance` beside `n`,
    /// in source order: the names of a scope's copies and imports are one
    /// namespace. A name may be declared more than once when every
    /// declaration of it has a clause (R-104); else the second is the
    /// error, naming the first.
    pub(super) fn redeclared(&mut self, n: &SyntaxNode, name: &str) -> L<()> {
        let Some(parent) = n.parent() else {
            return Ok(());
        };
        // A provider's name is in the scope's one namespace too (R-112):
        // `use db` of a provider beside `use db` of a module is the error
        // two uses are.
        let same: Vec<SyntaxNode> = parent
            .children()
            .filter(|c| c.kind() == USE || (self.is_copy(c) && !named_by_clause(c)))
            .filter(|c| {
                let other = match c.kind() {
                    USE => use_parts(c).1,
                    _ => copy_parts(c).1,
                };
                other == name
            })
            .collect();
        if same.len() < 2
            || same.first() == Some(n)
            || same.iter().all(|c| node(c, CLAUSE).is_some())
        {
            return Ok(());
        }
        let span = self.span(n);
        let first = self.span(&same[0]);
        let d = Diagnostic::error(
            span,
            format!("`{name}` is declared twice; give each a `where`"),
        )
        .with_label(first, "first here")
        .with_help(
            "a name is declared once in a scope, or several times each under a clause that \
             picks it (`resource pg_aws db { .. } where cloud == \"aws\"`); another name is \
             another statement, `resource C NAME { .. }`, `use PATH as NAME`",
        );
        self.diags.push(d);
        Err(Skip)
    }

    /// `use PATH [as NAME] [{ k = v }] [where B]` (R-65): a module imported
    /// once under NAME, its inputs the block's, its rules and denies run
    /// over what this scope sees, its items read as `NAME.x`; or a stack's
    /// deployments, read as `NAME[k=v].out`.
    pub(super) fn use_stmt(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<Option<ItemId>> {
        let span = self.span(n);
        let (written, name) = use_parts(n);
        self.redeclared(n, &name)?;
        if let Some(rest) = written.strip_prefix("std.") {
            let fns = crate::functions::registry();
            if !fns.packages().contains(&rest) {
                return self.error(
                    span,
                    format!(
                        "no module `{written}` in the standard library: its modules are {}",
                        fns.packages().join(", ")
                    ),
                );
            }
            // `std` is used everywhere already.
            return Ok(None);
        }
        if self.decls.deployed.iter().any(|d| d.path == written) {
            if node(n, CLAUSE).is_some() || node(n, BLOCK).is_some() {
                return self.error(
                    span,
                    format!(
                        "`use {written}` binds a stack's deployments, which take no block and \
                         no clause: put the `where` on what reads them"
                    ),
                );
            }
            return Ok(None);
        }
        let path = self.module_path_of(scope, &written);
        match self.module_at(&path) {
            None => return self.error(span, format!("no module `{written}`")),
            Some((_, true)) => {
                let d = Diagnostic::error(
                    span,
                    format!(
                        "{written} is a component, a type: make a resource of it, `resource \
                         {written} NAME {{ .. }}`"
                    ),
                )
                .with_note(
                    "`use` imports a module, a file, once under its name; a component, an item \
                     `component NAME { .. }`, is a type the program defines",
                );
                self.diags.push(d);
                return Err(Skip);
            }
            Some(_) => {}
        }
        // A module is stamped once under the name the `use` gives it, its
        // inputs the block's, else their defaults.
        let copy = (CopyKind::Use, path, name, None);
        self.copy(n, scope, outer, copy).map(Some)
    }

    /// `resource stacks.S NAME { k = v } [where B]` in a project module
    /// (R-114): a deployment of the stack S, a resource of its type whose
    /// attributes are its key's values. A project module makes nothing
    /// else. Built as the `Resource` item it is.
    pub(super) fn deployment(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let written = dotted_text(n, 1);
        if self.decls.deployed.iter().any(|d| d.path == written) {
            return self.block_stmt(n, scope, outer);
        }
        let stacks: Vec<String> = self
            .decls
            .deployed
            .iter()
            .map(|d| format!("`{}`", d.path))
            .collect();
        let d = Diagnostic::error(
            self.span(n),
            format!(
                "{written} is no stack: a project module's resources are deployments of stacks"
            ),
        )
        .with_help(format!(
            "make a deployment of a stack by its path, `resource {} NAME {{ KEY = VALUE }}`; \
             what else is made is a stack's",
            self.decls
                .deployed
                .first()
                .map_or("stacks.NAME", |d| d.path.as_str())
        ));
        let d = match stacks.is_empty() {
            true => d,
            false => d.with_note(format!("the stacks it deploys: {}", stacks.join(", "))),
        };
        self.diags.push(d);
        Err(Skip)
    }

    /// `resource PATH NAME { k = v } [where B]` of a component (R-113,
    /// R-65): one copy of the component, named NAME.
    pub(super) fn instance(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let (written, name) = copy_parts(n);
        let module = match self.component_path(scope, &written) {
            Ok(m) => m,
            Err(_) if self.signature(scope, &written).is_some() => {
                let d = Diagnostic::error(
                    span,
                    format!("{written} is a component signature, which has no resources"),
                )
                .with_help(format!(
                    "make a resource of a component that has it, `component C: {written} {{ .. \
                     }}`, and pick one by a clause: `resource C {name} {{ .. }} where ..`"
                ));
                self.diags.push(d);
                return Err(Skip);
            }
            Err(d) => {
                self.diags.push(Diagnostic { span, ..*d });
                return Err(Skip);
            }
        };
        if !named_by_clause(n) {
            self.redeclared(n, &name)?;
        }
        if tokens(n).any(|t| t.kind() == RANK) {
            return self.error(
                span,
                format!(
                    "a resource of the component {written} takes no rank: its own resources do"
                ),
            );
        }
        if name == "_" {
            return self.error(
                span,
                format!("a resource is written by its name: `resource {written} _` names nothing"),
            );
        }
        if self.is_value(scope, &name) && header_name(n).is_some_and(|t| t.kind() != STRING) {
            return self.error(
                span,
                format!(
                    "`{name}` is a value in scope, but a resource's header name is literal: this \
                     is the resource {name} of {written}; name it otherwise"
                ),
            );
        }
        let via = self.via(scope, &written, &module);
        self.copy(n, scope, outer, (CopyKind::Component, module, name, via))
    }

    /// The module instance the component `component`, written `written`
    /// in `scope`, is named through (R-186): `a.volume` under `use backups
    /// as a`, the component an item of that module. Its body reads that
    /// instance's items bare.
    pub(super) fn via(&self, scope: ScopeId, written: &str, component: &str) -> Option<Via> {
        let (head, _) = written.split_once('.')?;
        let (parent, _) = component.rsplit_once('.')?;
        let declared = self.chain_of(scope).into_iter().find(|s| {
            let sc = self.names(*s);
            sc.component(head).is_some() || sc.module(head).is_some()
        })?;
        if self.names(declared).module(head)? != parent {
            return None;
        }
        Some(Via {
            module: parent.to_string(),
            name: head.to_string(),
            absolute: !self.own_scopes(scope).contains(&declared),
        })
    }

    /// One copy of the component `module`, named `name`, its inputs the
    /// block of `n` (an `instance` or a `use`) and its clause `n`'s. Built
    /// as a `Copy` item (R-211 step 5): a name from the clause, its
    /// inputs' values and the reads they hoisted, its rows items of their
    /// own, its clause B's goals; `lower` writes it.
    pub(super) fn copy(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        (kind, module, name, via): (CopyKind, String, String, Option<Via>),
    ) -> L<ItemId> {
        let span = self.span(n);
        self.check_gives(n, scope, &module)?;
        let mut rc = self.rc(n, scope, outer);
        self.clauses(&mut rc, n)?;
        let gathered = self.gather.as_mut().and_then(|g| g.take_clause());
        let named_at = self.helpers.len();
        // A name from the clause (R-191): each row a copy, named as a
        // provider type's resource is.
        let named_id = match header_name(n).filter(|_| named_by_clause(n)) {
            Some(t) => Some(self.name_from_clause(&mut rc, &t)?),
            None => None,
        };
        let made = self.helpers.len();
        let fields = match node(n, BLOCK) {
            Some(block) => self.fields(&mut rc, &block)?,
            None => Vec::new(),
        };
        let mut inputs = Vec::new();
        let mut from = made;
        for (f, id, to) in fields {
            if matches!(f.op, FieldOp::Add) {
                return self.error(f.span, "an input of a copy is set with `=`, not `+=`");
            }
            if f.rank.is_some() {
                return self.error(f.span, "an input of a copy takes no rank");
            }
            inputs.push((f.key, f.value, f.span, id, from..to));
            from = to;
        }
        let values: Vec<bind::Term> = inputs
            .iter()
            .map(|&(_, _, _, id, _)| id)
            .chain(named_id)
            .map(bind::Term::Expr)
            .collect();
        self.check_bound(&rc, gathered, &values)?;
        let rows = match node(n, BLOCK) {
            Some(block) => self.block_rows(&block, &module, scope, outer)?,
            None => Default::default(),
        };
        // A copy binds no aggregate: its rows do, each folding its own.
        self.aggregate_in(gathered, span)?;
        let c = crate::program::build::CopyLowered {
            span,
            scope: self.item_scope,
            kind,
            module,
            name,
            named: named_id.map(|id| (id, &self.helpers[named_at..made])),
            inputs: inputs
                .iter()
                .map(|(key, _, at, id, h)| crate::program::build::InputGiven {
                    key: key.clone(),
                    span: *at,
                    value: *id,
                    made: &self.helpers[h.clone()],
                })
                .collect(),
            rows,
            clause: gathered,
            via,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.copy_item(c);
        gather.done(b);
        Ok(item)
    }

    /// The rows a `use` or `instance` block gives the relations its
    /// module takes, `input p` (R-55): `p(t, ..) [where B]`, a rule in
    /// this scope, and `p from FORMAT(..) [where B]`, a table, its columns
    /// the module's `decl p`. Each head is the module's own `p`, made the
    /// copy's by `modules`. Their items.
    pub(super) fn block_rows(
        &mut self,
        block: &SyntaxNode,
        module: &str,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<Vec<ItemId>> {
        let Some(inner) = self.module_at(module).map(|m| m.0) else {
            return Ok(Default::default());
        };
        let given = |k: &DeclKind| matches!(k, DeclKind::RelationInput { given: true });
        let takes: BTreeSet<String> = self.names(inner).names(given).cloned().collect();
        let mut out = Vec::new();
        let mut failed = false;
        for n in block
            .children()
            .filter(|c| matches!(c.kind(), RULE | FACT | INPUT_RELATION))
        {
            let span = self.span(&n);
            let pred = match n.kind() {
                INPUT_RELATION => word_text(&n, 0),
                _ => n
                    .children()
                    .find(|c| c.kind() == CALL)
                    .and_then(|c| self.callee(&c))
                    .unwrap_or_default(),
            };
            if !takes.contains(&pred) {
                let mut d = Diagnostic::error(span, format!("{module} takes no relation {pred}"));
                d = if takes.is_empty() {
                    d.with_note(format!("{module} declares no relation input, `input p`"))
                } else {
                    d.with_note(format!(
                        "the relations it takes: {}",
                        takes.iter().cloned().collect::<Vec<_>>().join(", ")
                    ))
                };
                self.diags.push(d);
                failed = true;
                continue;
            }
            let r = match n.kind() {
                INPUT_RELATION => (|| {
                    let Some(decl) = self.names(inner).decl(&pred).cloned() else {
                        return self.error(
                            span,
                            format!(
                                "{pred} from ..: {module} declares no columns for {pred}, \
                                 `decl {pred}(a: T, ..)`"
                            ),
                        );
                    };
                    let cols = self.table_columns(&pred, &decl)?;
                    let arity = cols.len();
                    self.table_item(&n, scope, outer, (pred, arity, false), cols)
                })(),
                // A row's references are the user's resources, the
                // module's column typed as its `decl` says.
                _ => {
                    self.given = Some(inner);
                    let r = self.rule(&n, scope, outer);
                    self.given = None;
                    r
                }
            };
            match r {
                Ok(item) => out.push(item),
                Err(Skip) => failed = true,
            }
        }
        if failed {
            return Err(Skip);
        }
        Ok(out)
    }

    /// A header name with holes, `"agent-${i}"`, of a resource of any
    /// type, a provider's, a stack's or a component's (R-112, R-191): a
    /// term per row of the clause, bound last by `format`, one segment of
    /// the address (quoted when it holds a dot). Its variable.
    pub(super) fn name_from_clause(&mut self, rc: &mut Rc, header: &SyntaxToken) -> L<ExprId> {
        let s = self.string_expr(rc, header)?;
        let v = fresh(rc, "Addr");
        let span = self.span_of(header.text_range());
        let id = self.building(rc, span, |b| {
            let segment = b.form(crate::address::NAME_SEGMENT, vec![s]);
            b.bound_into(segment, &v)
        });
        Ok(id)
    }

    /// The component signature `name` in scope: the file and the `type`
    /// statement. A dotted name is another module's, by its path or the
    /// name its `use` binds (`db.database`).
    pub(super) fn signature(&self, scope: ScopeId, name: &str) -> Option<(u32, SyntaxNode)> {
        match name.rsplit_once('.') {
            Some((m, s)) => {
                let path = self.module_path_of(scope, m);
                let at = self.module_at(&path)?.0;
                let sig = self.names(at).signature(s);
                sig.map(|(f, n)| (f, n.clone()))
            }
            None => self
                .chain_of(scope)
                .into_iter()
                .find_map(|s| self.names(s).signature(name).map(|(f, n)| (f, n.clone()))),
        }
    }

    /// A type as its declaration in `file` reads, aliases expanded: what
    /// a signature and a component compare.
    pub(super) fn type_text_in(&mut self, file: u32, t: Option<SyntaxNode>) -> String {
        let Some(t) = t else {
            return "any".to_string();
        };
        let saved = std::mem::replace(&mut self.file, file);
        let ty = self.type_expr(&t);
        self.file = saved;
        crate::inputs::type_text(&ty)
    }

    /// `component C: T { .. }` (R-104): C declares every input and output
    /// the signature `T` does, of its type; an input `T` does not declare
    /// has a default, so a copy picked by `T` can be given what `T` says
    /// alone. Each difference is an error at the component.
    pub(super) fn check_signature(&mut self, n: &SyntaxNode, t: &SyntaxNode, scope: ScopeId) {
        let span = self.span(n);
        let comp = word_text(n, 1);
        let sig = dotted_text(t, 0);
        let Some((file, decl)) = self.signature(scope, &sig) else {
            self.diags.push(
                Diagnostic::error(self.span(t), format!("no component signature `{sig}`"))
                    .with_note("a signature is `type NAME = component { input ..  output .. }`"),
            );
            return;
        };
        let items = |b: Option<SyntaxNode>| -> Vec<SyntaxNode> {
            b.into_iter()
                .flat_map(|b| b.children().collect::<Vec<_>>())
                .collect()
        };
        let sig_items = items(node(&decl, SIGNATURE).and_then(|s| node(&s, STMT_BLOCK)));
        let own = items(node(n, STMT_BLOCK));
        let find = |kind: SyntaxKind, name: &str| {
            own.iter()
                .find(|c| c.kind() == kind && word_text(c, 1) == name)
                .cloned()
        };
        let at = self.span(&decl);
        let mut errors = Vec::new();
        for c in &sig_items {
            let name = word_text(c, 1);
            match c.kind() {
                INPUT | OUTPUT_DECL => {
                    let what = if c.kind() == INPUT { "input" } else { "output" };
                    let want = self.type_text_in(file, node(c, TYPE_EXPR));
                    match find(c.kind(), &name) {
                        None => errors.push(format!(
                            "component {comp} has no {what} {name}, which {sig} declares ({name}: {want})"
                        )),
                        Some(o) => {
                            let got = self.type_text_in(self.file, node(&o, TYPE_EXPR));
                            if got != want {
                                errors.push(format!(
                                    "{what} {name} of component {comp} is {got}; {sig} declares {want}"
                                ));
                            }
                        }
                    }
                }
                INPUT_RELATION => {
                    if find(INPUT_RELATION, &name).is_none() {
                        errors.push(format!(
                            "component {comp} takes no relation {name}, which {sig} declares"
                        ));
                    }
                }
                _ => errors.push(format!(
                    "{sig} is a signature: it declares inputs and outputs, not `{}`",
                    c.text()
                        .to_string()
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                )),
            }
        }
        // An input the signature does not declare has a default: a copy
        // picked by the signature is given what the signature says.
        for c in own.iter().filter(|c| c.kind() == INPUT) {
            let name = word_text(c, 1);
            let declared = sig_items
                .iter()
                .any(|s| s.kind() == INPUT && word_text(s, 1) == name);
            if !declared && terms(c).next().is_none() {
                errors.push(format!(
                    "input {name} of component {comp} is not in {sig} and has no default"
                ));
            }
        }
        for e in errors {
            self.diags
                .push(Diagnostic::error(span, e).with_label(at, format!("the signature {sig}")));
        }
    }
}
