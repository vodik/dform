//! Instancing (R-211): each copy of a module or a component, `use m [as
//! n] { k = V } where B` or `resource c n { k = V } where B`, is its
//! definition's items lowered again from the program's nodes under the
//! copy (`chain.rs`): one mechanism, the definition's statements under the
//! scope `n` (DESIGN.org R-65, proposals/I-modules.md). Its private
//! relations are its own (`n::p`), its resources, cells and outputs are
//! under its scope, a read of an enclosing definition's item is the
//! instance's it is taken from (R-186), and a copy named by its clause
//! (R-191) is one per row, each relation private to it given the copy's
//! name first. What the copy's user writes (its inputs, its rows, its
//! clause) is its user's, the copy's interface checked by `modules`.
//!
//! Copies are lowered outside in: a copy inside another is lowered under
//! both, each term and atom final when it is made, so no statement is
//! rewritten after it is written. A copy's private relations are those its
//! definition's statements define, found once per definition by lowering
//! it under a copy that renames nothing ([`Instancer::keys`]).

mod chain;

pub use chain::{Chain, Frame, From, Named, Stage, absolute, private_name, raw_stmt};

use super::{Env, Records};
use crate::ast::{self, Atom, InputDecl, Lit, RuleStmt, Span, Stmt, Term, str_term};
use crate::diag::{self, Diagnostic};
use crate::modules::{self, Book, GATE, INSTANCE_OF, Interface, LEXICAL_OF};
use crate::program::{ItemId, ItemKind, Program};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// The instancing of one program's copies.
pub(crate) struct Instancer<'p> {
    nodes: &'p Program,
    /// The program's own record fields.
    top: Records,
    /// Every definition, by path.
    defs: BTreeMap<&'p str, ItemId>,
    /// The record fields a definition's items are lowered by: its
    /// enclosing definitions' `decl`s and its own over the program's.
    records: BTreeMap<&'p str, Records>,
    /// The relations each definition makes private to a copy, by path.
    keys: BTreeMap<String, Rc<BTreeSet<String>>>,
    /// The copies named by their clause each definition's copy makes.
    named_in: BTreeMap<String, usize>,
    /// The definitions being instanced, outermost first: a component that
    /// reaches itself is an error, not a loop.
    expanding: Vec<String>,
    /// The copies named by their clause so far: each one's variable for
    /// its name inside it is its own (R-191).
    named: usize,
}

impl<'p> Instancer<'p> {
    /// The instancing of the copies of `nodes`, the program's own records
    /// `records` (its `decl`s over the core's).
    pub(crate) fn new(nodes: &'p Program, records: &Records) -> Instancer<'p> {
        let mut i = Instancer {
            nodes,
            top: records.clone(),
            defs: BTreeMap::new(),
            records: BTreeMap::new(),
            keys: BTreeMap::new(),
            named_in: BTreeMap::new(),
            expanding: Vec::new(),
            named: 0,
        };
        for &root in &nodes.roots {
            i.definitions(root, records);
        }
        i
    }

    /// The definition `id` and those inside it, each with its records.
    fn definitions(&mut self, id: ItemId, outer: &Records) {
        let ItemKind::Module { path, items, .. } = &self.nodes.items[id].kind else {
            return;
        };
        let records = outer.with_items(self.nodes, items);
        self.defs.insert(path, id);
        for &i in items {
            self.definitions(i, &records);
        }
        self.records.insert(path, records);
    }

    /// The copy the program's top level writes as `u` (`used`: a `use`),
    /// by its user's scope `""`: the root item that makes it.
    pub(crate) fn top(&mut self, book: &mut Book, u: &ast::Instance, used: bool) -> Vec<Stmt> {
        let found = self.nodes.roots.iter().copied().find(|&i| {
            let it = &self.nodes.items[i];
            // A span's `==` holds of any two: its place is compared.
            let at = |s: Span| (s.file, s.start, s.end);
            matches!(it.kind, ItemKind::Copy { .. }) && at(it.span) == at(u.span)
        });
        let Some(item) = found else {
            unreachable!("a copy the program writes is an item of its nodes")
        };
        let mut chain = Chain::default();
        // Its helpers are the program's statements already.
        self.copy(book, item, &mut chain, "", used).0
    }

    /// `def`, or `None` (reported) when it is being instanced already.
    fn enter(&mut self, book: &mut Book, path: &str, span: Span) -> Option<ItemId> {
        let def = *self.defs.get(path)?;
        if self.expanding.iter().any(|p| p == path) {
            let mut cycle = self.expanding.clone();
            cycle.push(path.to_string());
            book.diags.push(Diagnostic::error(
                span,
                format!("{path} reaches itself: {}", cycle.join(" -> ")),
            ));
            return None;
        }
        self.expanding.push(path.to_string());
        Some(def)
    }

    /// The items of the definition `def`.
    fn items(&self, def: ItemId) -> &'p [ItemId] {
        match &self.nodes.items[def].kind {
            ItemKind::Module { items, .. } => items,
            _ => unreachable!("a definition"),
        }
    }

    /// The definition `def`'s statements as written: its interface and
    /// the rest, what the checks read.
    fn written(&self, def: ItemId) -> (Interface, Vec<Stmt>) {
        let mut stmts = Vec::new();
        for &i in self.items(def) {
            if !matches!(self.nodes.items[i].kind, ItemKind::Module { .. }) {
                stmts.extend(super::lower_item(self.nodes, i));
            }
        }
        modules::interface(&stmts)
    }
}

/// One copy being instanced: its item's parts, and where it stands.
struct Copying<'a> {
    item: ItemId,
    module: &'a str,
    name: &'a str,
    used: bool,
    /// `module` or `component`, as messages name the definition.
    kind: &'static str,
    /// Its scope's absolute name.
    abs: String,
    /// A used module the stack reaches by `use` alone: its inputs are the
    /// stack's to give, `m.k` (R-55).
    flat: bool,
    /// The statement as the program writes it, what its rules are lowered
    /// out of (`diag::origin_id`).
    origin: String,
    span: Span,
}

impl<'a> Copying<'a> {
    fn new(nodes: &'a Program, item: ItemId, at: &str, reached: bool) -> Copying<'a> {
        let it = &nodes.items[item];
        let ItemKind::Copy {
            kind,
            module,
            name,
            named,
            ..
        } = &it.kind
        else {
            unreachable!("a copy")
        };
        let used = matches!(kind, crate::program::node::CopyKind::Use);
        let origin = match used {
            true if module.rsplit('.').next() == Some(name.as_str()) => format!("use {module}"),
            true => format!("use {module} as {name}"),
            false if named.is_some() && !name.starts_with('"') => {
                format!("resource {module} \"{name}\"")
            }
            false => format!("resource {module} {name}"),
        };
        Copying {
            item,
            module,
            name,
            used,
            kind: if used { "module" } else { "component" },
            abs: crate::types::dotted(at, name),
            flat: used && reached,
            origin,
            span: it.span,
        }
    }

    /// As `modules` checks a copy.
    fn checked<'u>(&self, u: &'u ast::Instance) -> modules::Copy<'u> {
        modules::Copy {
            u,
            kind: self.kind,
            flat: self.flat,
            abs: self.abs.clone(),
            origin: self.origin.clone(),
        }
    }
}

impl<'p> Instancer<'p> {
    /// The copy `item` made in the scope `at` under its user's copies
    /// `chain` (`used`: a `use`): what its user writes of it (its inputs,
    /// its rows, its gate), then its definition's statements under it.
    fn copy(
        &mut self,
        book: &mut Book,
        item: ItemId,
        chain: &mut Chain,
        at: &str,
        used: bool,
    ) -> (Vec<Stmt>, Vec<Stmt>) {
        let site = self.expanding.last().cloned();
        let user_inputs = book.inputs_here.last().cloned().unwrap_or_default();
        let c = Copying::new(self.nodes, item, at, book.flat.contains(at));
        debug_assert_eq!(c.used, used);
        let Some(def) = self.enter(book, c.module, c.span) else {
            return Default::default();
        };
        let (iface, written) = self.written(def);
        let f = self.push_frame(chain, &c, def);
        let (u, rows, helpers) = self.user(chain, f, &iface);
        let mut out = Vec::new();
        {
            let cc = c.checked(&u);
            book.check_once(&cc, &iface, &written);
            if c.flat {
                book.flat.insert(c.abs.clone());
            }
            let finish = |a: Atom| chain.atom_from(a, Some(by_row(f)));
            modules::instance_inputs(&cc, &iface, &mut out, &mut book.diags, &finish);
        }
        book.inputs_here
            .push(iface.inputs.iter().map(|i| i.name.clone()).collect());
        let body = self.body(book, def, chain, f, &c.abs);
        book.inputs_here.pop();
        self.expanding.pop();
        let mut copy = self.statements(chain, f, def, &iface, body);
        out.extend(self.rows(chain, f, &u, rows, &iface));
        book.privates(&c.checked(&u), &chain.frames[f].keys);
        self.scoped(chain, f, def, &c, &mut copy);
        if !used {
            self.reached(book, chain, f, &c, site.as_deref(), &mut copy);
        }
        book.outputs(&c.checked(&u), &iface);
        if c.flat {
            let finish = |a: Atom| chain.atom_from(a, Some(by_row(f)));
            for i in &iface.inputs {
                out.extend(modules::given_rules(c.name, &c.abs, i, &finish));
            }
        }
        book.declare_inputs(&c.checked(&u), &iface, &user_inputs);
        if chain.frames[f].named.is_some() {
            self.named += 1;
            copy.extend(self.secret_cells(book, chain, f, &c));
        }
        if chain.frames[f].named.is_some() {
            // Its inputs' values are read as their types here, by the
            // scope's declarations.
            for s in &mut out {
                if let Stmt::Fact(h) | Stmt::Rule(RuleStmt { head: h, .. }) = s {
                    modules::read_input_of(h, book.declared.iter().filter(|d| d.scope == c.abs));
                }
            }
        }
        out.extend(self.gate(chain, f, &c, &u, copy));
        chain.frames.pop();
        (out, helpers)
    }
}

/// Where the user's side of the copy `f` starts: its name per row.
fn by_row(f: usize) -> From {
    From {
        by_row: true,
        ..From::at(f, Stage::Named)
    }
}

impl<'p> Instancer<'p> {
    /// The frame of the copy `c` of `def` pushed on `chain`: its index.
    fn push_frame(&mut self, chain: &mut Chain, c: &Copying, def: ItemId) -> usize {
        let keys = self.keys(def, c.module);
        let ItemKind::Copy { named, via, .. } = &self.nodes.items[c.item].kind else {
            unreachable!("a copy")
        };
        let named = named.map(|e| {
            let inside = self.named + self.named_in(c.module) + 1;
            let by_row = {
                let env = Env::plain(self.nodes);
                super::expr::Lowering::new(env).expr(e)
            };
            Named {
                scope: c.name.to_string(),
                inside: Term::Var(format!("__copy{inside}")),
                by_row,
            }
        });
        chain.frames.push(Frame {
            item: c.item,
            scope: c.name.to_string(),
            def: c.module.to_string(),
            vars: !c.used,
            keys,
            via: via.clone(),
            named,
            unreached: Default::default(),
        });
        chain.frames.len() - 1
    }
}

impl<'p> Instancer<'p> {
    /// What lowers the nodes under `chain` from `from`, with the record
    /// fields of `def`'s scope.
    fn env<'a>(&'a self, chain: &'a Chain, from: Option<From>, records: &'a Records) -> Env<'a>
    where
        'p: 'a,
    {
        Env {
            program: self.nodes,
            records: Some(records),
            errors: None,
            chain: Some(chain),
            from,
        }
    }

    /// The record fields of the scope the copy `f` is written in: its
    /// user's definition's, or the program's.
    fn user_records(&self, chain: &Chain, f: usize) -> &Records {
        match f {
            0 => &self.top,
            _ => &self.records[chain.frames[f - 1].def.as_str()],
        }
    }

    /// What the user of the copy `f` writes of it (its inputs, its clause,
    /// its name per row), each per row of it; its rows, each a relation
    /// the definition takes named the copy's (R-55, `true`) or not; and
    /// the helper statements its parts made, its user's.
    fn user(
        &self,
        chain: &Chain,
        f: usize,
        iface: &Interface,
    ) -> (ast::Instance, Vec<(Stmt, bool)>, Vec<Stmt>) {
        let item = self.copy_item(chain, f);
        let records = self.user_records(chain, f);
        let env = self.env(chain, Some(by_row(f)), records);
        let takes: BTreeSet<String> = iface.relations.iter().map(|e| e.pred.clone()).collect();
        super::items::copy_parts(env, item, Some((chain.frames[f].scope.as_str(), &takes)))
    }

    /// The copy item the frame `f` is of.
    fn copy_item(&self, chain: &Chain, f: usize) -> ItemId {
        chain.frames[f].item
    }

    /// The rows the copy `f`'s block gives the relations its definition
    /// takes (R-55), written in its user's scope: each the copy's own
    /// relation, and with a clause holding only while it does, as the copy
    /// does; and that each is one relation of facts and rules.
    fn rows(
        &self,
        chain: &Chain,
        f: usize,
        u: &ast::Instance,
        rows: Vec<(Stmt, bool)>,
        iface: &Interface,
    ) -> Vec<Stmt> {
        let clause = u.clause.clone().unwrap_or_default();
        let scope = &chain.frames[f].scope;
        let mixed = iface.relations.iter().map(|e| {
            let e = ast::Extern {
                pred: private_name(scope, &e.pred),
                ..e.clone()
            };
            Stmt::Mixed(chain.relation(e, Some(by_row(f))))
        });
        rows.into_iter()
            .map(|(st, taken)| match (st, taken) {
                (Stmt::Fact(a), true) => modules::fact_or_rule(a, clause.clone()),
                (Stmt::Rule(mut r), true) => {
                    r.body.splice(0..0, clause.iter().cloned());
                    Stmt::Rule(r)
                }
                (other, _) => other,
            })
            .chain(mixed)
            .collect()
    }
}

impl<'p> Instancer<'p> {
    /// The definition `def`'s statements under the copy `f` (its scope
    /// `abs`): a check per name its copies bind more than once (R-104),
    /// then each item's, a copy inside instanced in its place; its
    /// interface (inputs, outputs, the relations it takes and exports)
    /// left to the copy's.
    fn body(
        &mut self,
        book: &mut Book,
        def: ItemId,
        chain: &mut Chain,
        f: usize,
        abs: &str,
    ) -> (Vec<Stmt>, Vec<ast::OutputDecl>) {
        let path = chain.frames[f].def.clone();
        let mut out = self.exclusive_copies(chain, f, def);
        let mut values = Vec::new();
        for &i in self.items(def) {
            match &self.nodes.items[i].kind {
                ItemKind::Module { .. } => {}
                ItemKind::Copy { kind, .. } => {
                    let used = matches!(kind, crate::program::node::CopyKind::Use);
                    let (stmts, helpers) = self.copy(book, i, chain, abs, used);
                    out.extend(stmts);
                    out.extend(helpers);
                }
                _ => {
                    let records = &self.records[path.as_str()];
                    let env = self.env(chain, chain.all(), records);
                    for s in super::items::statements_in(env, i) {
                        match s {
                            Stmt::Output(o) if o.relation.is_none() && o.value.is_some() => {
                                values.push(o)
                            }
                            Stmt::Input(_) | Stmt::RelationInput(_) | Stmt::Output(_) => {}
                            s => out.push(s),
                        }
                    }
                }
            }
        }
        (out, values)
    }

    /// The checks of the names the copies in `def` bind more than once,
    /// each under its clause (R-104): where each holds, and a deny where
    /// two do.
    fn exclusive_copies(&self, chain: &Chain, f: usize, def: ItemId) -> Vec<Stmt> {
        let mut groups: BTreeMap<&str, Vec<ItemId>> = BTreeMap::new();
        for &i in self.items(def) {
            if let ItemKind::Copy {
                name, named: None, ..
            } = &self.nodes.items[i].kind
            {
                groups.entry(name).or_default().push(i);
            }
        }
        let records = &self.records[chain.frames[f].def.as_str()];
        let env = self.env(chain, chain.all(), records);
        let finish = |a: Atom| chain.atom_from(a, chain.all());
        let mut out = Vec::new();
        for (name, copies) in groups.into_iter().filter(|(_, c)| c.len() > 1) {
            let alts: Vec<modules::Alternative> = copies
                .into_iter()
                .map(|i| super::items::alternative(env, i))
                .collect();
            out.extend(modules::exclusive_with(name, &alts, &finish));
        }
        out
    }

    /// The copy `f`'s statements of its definition `def`: its body, then
    /// its outputs' values (an `addr` output's the copy's resource), each
    /// input's refinement and each relation it exports.
    fn statements(
        &self,
        chain: &Chain,
        f: usize,
        def: ItemId,
        iface: &Interface,
        (body, values): (Vec<Stmt>, Vec<ast::OutputDecl>),
    ) -> Vec<Stmt> {
        let all = chain.all();
        let finish = |a: Atom| chain.atom_from(a, all);
        let mut out = body;
        for o in values {
            // An undeclared output is reported once, by `check_component`.
            let Some(decl) = iface.outputs.get(&o.name) else {
                continue;
            };
            let mut value = o.value.expect("an output value");
            if matches!(&decl.ty, Some(ast::TypeExpr::Name(t)) if t == "addr") {
                value = chain.scoped_fold(chain::scoped(str_term(""), value), all, all);
            }
            let head = ast::atom("output", vec![str_term(&o.name), value], o.span);
            let head = chain.atom(head, all, &[all, None]);
            out.push(modules::fact_or_rule(head, Vec::new()));
        }
        let inputs = self.inputs(chain, f, def);
        let written: Vec<InputDecl> = iface
            .inputs
            .iter()
            .flat_map(crate::inputs::leaves)
            .collect();
        let read: Vec<InputDecl> = inputs.iter().flat_map(crate::inputs::leaves).collect();
        for (i, w) in read.iter().zip(&written) {
            out.extend(modules::refinement(i, w, &chain.frames[f].scope, &finish));
        }
        out.extend(
            iface
                .relation_outputs
                .iter()
                .map(|o| modules::rows_of(o, &finish)),
        );
        out
    }

    /// The copy `f`'s statements made its own: lowered out of its
    /// statement, its `let`s read as its cells, its inputs read.
    fn scoped(&self, chain: &Chain, f: usize, def: ItemId, c: &Copying, copy: &mut Vec<Stmt>) {
        modules::set_origin(copy, diag::origin_id(&c.origin));
        let frame = &chain.frames[f];
        let after = Some(From::at(f, Stage::Named));
        let finish = |a: Atom| chain.atom_from(a, after);
        let name = |k: &str| frame.rename(k);
        let stmts = std::mem::take(copy);
        *copy = modules::lets(stmts, &frame.scope, Some(&name), &finish);
        for i in self.inputs(chain, f, def) {
            copy.extend(modules::input_reader(&frame.scope, &i, &name, &finish));
        }
    }

    /// The copy `c` (`f`, a component's) reading its module's items, or an
    /// enclosing component's, as the instance it was taken from has them
    /// (R-186): recorded in `__lexical_of` for `why`; a read no instance
    /// holds (a copy by its path with no `use`) is an error at the read.
    fn reached(
        &self,
        book: &mut Book,
        chain: &Chain,
        f: usize,
        c: &Copying,
        site: Option<&str>,
        copy: &mut Vec<Stmt>,
    ) {
        let frame = &chain.frames[f];
        let parent = c.module.rsplit_once('.').map(|(p, _)| p);
        let after = Some(From::at(f, Stage::Named));
        let scope = match &frame.via {
            Some(via) => Some(match absolute(&via.scope) {
                Some(t) => (t.clone(), None),
                None => (via.scope.clone(), after),
            }),
            None if parent.is_some() && parent == site => Some((str_term(""), after)),
            None => None,
        };
        if let Some((scope, at)) = scope {
            let fact = ast::atom(
                LEXICAL_OF,
                vec![str_term(""), str_term(c.name), scope],
                c.span,
            );
            copy.push(Stmt::Fact(chain.atom(fact, after, &[after, after, at])));
        }
        let Some((body, name, span)) = frame.unreached.borrow_mut().take() else {
            return;
        };
        let kind = match self
            .defs
            .get(body.as_str())
            .map(|&d| &self.nodes.items[d].kind)
        {
            Some(ItemKind::Module {
                component: true, ..
            }) => "component",
            _ => "module",
        };
        let help = match kind {
            "module" => format!(
                "`use {body}` beside the copy: `resource {} {}` then reads that instance's \
                 `{name}`",
                c.module, c.name
            ),
            _ => format!("make the copy inside component {body}'s body, whose `{name}` it reads"),
        };
        let d = Diagnostic::error(
            span,
            format!(
                "`{name}` is an item of {kind} {body}, and the copy {} of {} is made outside \
                 every instance of {body}: a component reads the items of the instance it is \
                 taken from",
                c.name, c.module
            ),
        )
        .with_label(c.span, format!("the copy {}", c.name))
        .with_help(help);
        book.diags.push(d);
    }
}

impl<'p> Instancer<'p> {
    /// The copy `c`'s statements, and for a component's copy the fact
    /// `instance_of(path, "", scope)`. With a clause `B` they exist only
    /// while it holds: the copy's own `scope::__instance(path) :- B` gates
    /// each of its rules and resources, and `instance_of` is derived from
    /// it. The gate is the copy's own relation, so that one copy's clause
    /// reading another's outputs is no cycle through every copy. A copy
    /// named by its clause is recorded per row, gated per row.
    fn gate(
        &self,
        chain: &Chain,
        f: usize,
        c: &Copying,
        u: &ast::Instance,
        copy: Vec<Stmt>,
    ) -> Vec<Stmt> {
        let frame = &chain.frames[f];
        let named = frame.named.is_some();
        let record = !c.used || named;
        let after = Some(From::at(f, Stage::Named));
        let fact = ast::atom(
            INSTANCE_OF,
            vec![str_term(c.module), str_term(""), str_term(c.name)],
            c.span,
        );
        let Some(b) = u.clause.as_ref().filter(|b| !b.is_empty()) else {
            let mut out = copy;
            if record {
                out.push(Stmt::Fact(chain.atom_from(fact, after)));
            }
            return out;
        };
        let own = ast::atom(
            &format!("{}::{GATE}", c.name),
            vec![str_term(c.module)],
            c.span,
        );
        let on = Lit::Pos(chain.atom_from(own.clone(), after));
        let mut out: Vec<Stmt> = copy
            .into_iter()
            .map(|s| match s {
                Stmt::Fact(a) => Stmt::Rule(RuleStmt::new(a, vec![on.clone()])),
                Stmt::Rule(mut r) => {
                    r.body.insert(0, on.clone());
                    Stmt::Rule(r)
                }
                Stmt::Resource(mut r) => {
                    r.body.get_or_insert_with(Vec::new).insert(0, on.clone());
                    r.reads = r.reads.start + 1..r.reads.end + 1;
                    Stmt::Resource(r)
                }
                other => other,
            })
            .collect();
        let head = chain.atom_from(own, Some(by_row(f)));
        out.push(Stmt::Rule(RuleStmt::new(head, b.to_vec())));
        if record {
            out.push(Stmt::Rule(RuleStmt::new(
                chain.atom_from(fact, after),
                vec![on],
            )));
        }
        out
    }

    /// The secret cells of the copy `c` (`f`) named by its clause (R-191):
    /// those its scope and the scopes inside it declare (its secret inputs
    /// and outputs, stated by the scope as written) are each copy's,
    /// `secret_cell(T, scope, K) :- secret_cell(T, abs, K)`.
    fn secret_cells(&self, book: &Book, chain: &Chain, f: usize, c: &Copying) -> Vec<Stmt> {
        let abs = &c.abs;
        let under = |s: &str| {
            s.strip_prefix(abs.as_str())
                .filter(|r| r.is_empty() || r.starts_with('.'))
                .map(str::to_string)
        };
        let scopes: BTreeSet<String> = book
            .declared
            .iter()
            .map(|d| d.scope.as_str())
            .chain(book.secret_outputs.iter().map(|(s, _)| s.as_str()))
            .filter_map(under)
            .collect();
        let after = Some(From::at(f, Stage::Named));
        let (t, k) = (Term::Var("T".into()), Term::Var("K".into()));
        let cell = |at: Term| {
            ast::atom(
                crate::transform::SECRET_CELL,
                vec![t.clone(), at, k.clone()],
                c.span,
            )
        };
        scopes
            .into_iter()
            .map(|rest| {
                let head = chain.atom_from(cell(str_term(&format!("{}{rest}", c.name))), after);
                let stated = chain.atom(
                    cell(str_term(&format!("{abs}{rest}"))),
                    after,
                    &[after, None, after],
                );
                Stmt::Rule(RuleStmt::new(head, vec![Lit::Pos(stated)]))
            })
            .collect()
    }

    /// The inputs of the definition `def` as the copy `f` reads them: each
    /// refinement under the copy; a dependent input's clause renamed by
    /// it, not put under its scope; a default as its user gives it.
    fn inputs(&self, chain: &Chain, f: usize, def: ItemId) -> Vec<InputDecl> {
        let records = &self.records[chain.frames[f].def.as_str()];
        let env = |from| self.env(chain, from, records);
        let full = env(chain.all());
        let guard = env(Some(From {
            keep: true,
            ..From::at(f, Stage::Rename)
        }));
        let default = env(Some(From::at(f, Stage::Named)));
        self.items(def)
            .iter()
            .filter(|&&i| matches!(self.nodes.items[i].kind, ItemKind::Input { .. }))
            .map(|&i| super::items::input_read(i, full, guard, default))
            .collect()
    }

    /// The relations the definition at `path` (`def`) makes private to
    /// each copy: those its statements define, the copies inside it
    /// instanced, its inputs and their refinements, and the relations it
    /// takes; found by lowering it under a copy that renames nothing.
    fn keys(&mut self, def: ItemId, path: &str) -> Rc<BTreeSet<String>> {
        if let Some(k) = self.keys.get(path) {
            return k.clone();
        }
        let named = self.named;
        let mut scratch = Book::default();
        let mut chain = Chain::default();
        chain.frames.push(Frame {
            item: def,
            scope: String::new(),
            def: path.to_string(),
            vars: true,
            keys: Rc::default(),
            via: None,
            named: None,
            unreached: Default::default(),
        });
        self.expanding.push(path.to_string());
        let (body, _) = self.body(&mut scratch, def, &mut chain, 0, "");
        self.expanding.pop();
        self.named_in.insert(path.to_string(), self.named - named);
        self.named = named;
        let (iface, _) = self.written(def);
        let mut defined = BTreeMap::new();
        modules::defined_preds(&body, &mut defined);
        for i in &iface.inputs {
            defined.insert(i.name.clone(), (1, i.span));
            for l in crate::inputs::leaves(i) {
                defined.insert(modules::refine_pred(&l.name), (1, i.span));
            }
        }
        for e in &iface.relations {
            defined.insert(e.pred.clone(), (e.arity, e.span));
        }
        let keys: Rc<BTreeSet<String>> = Rc::new(
            defined
                .into_keys()
                .filter(|p| modules::is_private(p))
                .collect(),
        );
        self.keys.insert(path.to_string(), keys.clone());
        keys
    }

    /// The copies named by their clause a copy of the definition at
    /// `path` makes, inside it.
    fn named_in(&self, path: &str) -> usize {
        self.named_in.get(path).copied().unwrap_or(0)
    }
}
