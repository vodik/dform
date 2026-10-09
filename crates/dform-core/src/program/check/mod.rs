//! The migration's differential check (R-211): the resolver's own output
//! and the program's lowering of it compared as one text, [`dump`], that
//! holds every statement, every span with its origin (a span's `==` holds
//! of any two, and an atom's `Debug` leaves it out, so neither alone
//! would see a wrong one) and every diagnostic. `DFORM_CHECK_LOWER=1` runs
//! it in every `lower_stack`, panicking at the first difference;
//! [`collect`] runs it for a test and returns what differs. Deleted with
//! the old path at the migration's end.

mod clauses;
pub use clauses::{clause, fold, literal};

use super::{ItemId, ItemKind, Program};
use crate::ast::{self, Atom, AttrDecl, Config, InputDecl, Lit, Span, Stmt, Term};
use crate::diag::{self, Diagnostic};
use slotmap::SecondaryMap;
use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;

type Lowered = Result<ast::Program, Vec<Diagnostic>>;

/// The switch's name.
pub const SWITCH: &str = "DFORM_CHECK_LOWER";

thread_local! {
    static COLLECTING: RefCell<Option<Collected>> = const { RefCell::new(None) };
}

/// What [`collect`] saw: how many lowerings, terms, literals and clauses
/// it compared, and those that differed.
#[derive(Debug, Default)]
pub struct Collected {
    pub compared: usize,
    pub terms: usize,
    /// Written literals built as goals and lowered back (step 4).
    pub literals: usize,
    /// Clauses, each a statement's body or a `let`'s, the same.
    pub clauses: usize,
    /// Rules folded over their aggregates, the same.
    pub folds: usize,
    /// The kinds of goal built (`Member/Enum`, `Not/helper`, ..): which
    /// forms a corpus reaches.
    pub built: std::collections::BTreeSet<String>,
    /// The items of the programs compared, by kind.
    pub items: std::collections::BTreeMap<String, usize>,
    /// The reads the programs' builders made their own nodes (step 6):
    /// a value's.
    pub value_reads: usize,
    pub differences: Vec<Difference>,
}

impl Collected {
    /// What `other` saw added to this.
    pub fn add(&mut self, other: Collected) {
        self.compared += other.compared;
        self.terms += other.terms;
        self.literals += other.literals;
        self.clauses += other.clauses;
        self.folds += other.folds;
        self.built.extend(other.built);
        for (k, n) in other.items {
            *self.items.entry(k).or_default() += n;
        }
        self.value_reads += other.value_reads;
        self.differences.extend(other.differences);
    }
}

/// The first line two dumps differ at, under the statement it belongs to.
#[derive(Debug)]
pub struct Difference {
    pub statement: String,
    pub old: String,
    pub new: String,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the program lowers differently from the resolver\n  statement: {}\n  resolver:  {}\n  program:   {}",
            self.statement, self.old, self.new
        )
    }
}

/// Whether `lower_stack` compares: the switch is set, or a test collects.
pub fn enabled() -> bool {
    static SET: OnceLock<bool> = OnceLock::new();
    *SET.get_or_init(|| std::env::var(SWITCH).is_ok_and(|v| v == "1"))
        || COLLECTING.with(|c| c.borrow().is_some())
}

/// Compare the resolver's output with the program's: a test that collects
/// records the difference, else it is a panic.
pub fn compare(old: &Lowered, new: &Lowered) {
    let difference = differ(&dump(old), &dump(new));
    record(difference, |c| c.compared += 1);
}

/// The program `program` lowered to `new`: compared with what the
/// resolver lowered it to (`ported`), its
/// items counted by kind.
pub fn program(program: &Program, ported: &Resolved, new: &Lowered) {
    compare(&resolved(program, ported), new);
    let mut kinds = Vec::new();
    let mut walk = program.roots.clone();
    while let Some(id) = walk.pop() {
        let kind = &program.items[id].kind;
        if let ItemKind::Module { items, .. } = kind {
            walk.extend(items.iter().copied());
        }
        kinds.push(super::spell::kind(kind).to_string());
    }
    // A statistic, not output: the arena's order does not matter.
    let values = program
        .exprs
        .values()
        .filter(|e| matches!(e.kind, super::node::ExprKind::Value { .. }))
        .count();
    record(None, |c| {
        for k in kinds {
            *c.items.entry(k).or_default() += 1;
        }
        c.value_reads += values;
    });
}

/// The diagnostics a ported statement's builder gave (`new`) and the
/// resolver's (`old`), for the statement at `span`: compared as a
/// lowering's are.
pub fn same_diagnostics(old: &[Diagnostic], new: &[Diagnostic], span: Span) {
    if old.is_empty() && new.is_empty() {
        return;
    }
    let difference =
        differ(&dump(&Err(old.to_vec())), &dump(&Err(new.to_vec()))).map(|d| Difference {
            statement: format!("the diagnostics of the statement at {}", place(span)),
            ..d
        });
    record(difference, |c| c.compared += 1);
}

/// A difference found: a test that collects records it, else a panic.
fn record(difference: Option<Difference>, count: impl FnOnce(&mut Collected)) {
    let unseen = COLLECTING.with(|c| match c.borrow_mut().as_mut() {
        Some(c) => {
            count(c);
            c.differences.extend(difference);
            None
        }
        None => difference,
    });
    if let Some(d) = unseen {
        panic!("{d}\n({SWITCH}=1)");
    }
}

/// A term the resolver lowered (`term`, after the reads it hoisted,
/// `reads`), written at `span`, built as nodes and lowered back: the two
/// compared as [`compare`] does a program. `written`: whether a lowered
/// variable is one the program wrote.
pub fn term(term: &Term, reads: &[Lit], span: Span, written: &dyn Fn(&str) -> bool) {
    let mut program = Program::new();
    let id = super::Builder::new(&mut program, span, written).hoisted(term, reads);
    let (t, rs) = super::lower_expr(&program, id);
    let difference = differ(&term_dump(term, reads), &term_dump(&t, &rs)).map(|d| Difference {
        statement: format!("the term at {}: {term:?}", place(span)),
        ..d
    });
    record(difference, |c| c.terms += 1);
}

/// A term and its reads as one text: the term, then each read and the
/// spans in it.
fn term_dump(term: &Term, reads: &[Lit]) -> String {
    let mut out = format!("term {term:?}\n");
    for r in reads {
        let mut spans = Spans::default();
        spans.lits(std::slice::from_ref(r));
        let _ = writeln!(out, "read {r:?}\n  spans {}", spans.text());
    }
    out
}

/// `f`, every `lower_stack` it runs compared, and what that found.
pub fn collect<T>(f: impl FnOnce() -> T) -> (T, Collected) {
    COLLECTING.with(|c| *c.borrow_mut() = Some(Collected::default()));
    let out = f();
    let seen = COLLECTING
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
    (out, seen)
}

/// What the resolver lowered each ported item to, kept beside the program
/// for [`resolved`].
pub type Resolved = SecondaryMap<ItemId, Vec<Stmt>>;

/// The resolver's own output for `program`: each item's from `ported`,
/// a module's in its module, walked from the roots as the resolver walked the files.
pub fn resolved(program: &Program, ported: &Resolved) -> Lowered {
    if !program.diags.is_empty() {
        return Err(program.diags.clone());
    }
    let mut statements = Vec::new();
    for &id in &program.roots {
        resolved_item(program, ported, id, &mut statements);
    }
    Ok(ast::Program {
        statements,
        stack: program.stack.clone(),
    })
}

fn resolved_item(program: &Program, ported: &Resolved, id: ItemId, out: &mut Vec<Stmt>) {
    let it = &program.items[id];
    match (&it.kind, ported.get(id)) {
        (_, Some(stmts)) => out.extend(stmts.iter().cloned()),
        (
            ItemKind::Module {
                path,
                component,
                items,
                ..
            },
            None,
        ) => {
            let mut body = Vec::new();
            for &i in items {
                resolved_item(program, ported, i, &mut body);
            }
            out.push(Stmt::Module(ast::Module {
                name: path.clone(),
                component: *component,
                body,
                span: it.span,
            }));
        }
        // What carries the parts the resolver read as they are lowers as
        // the resolver did, by construction.
        (
            ItemKind::Decl { .. }
            | ItemKind::Extern { .. }
            | ItemKind::TypeBlock { .. }
            | ItemKind::Doc { .. }
            | ItemKind::OutputRelation { .. },
            None,
        ) => out.extend(super::lower_item(program, id)),
        (kind, None) => panic!(
            "{} was ported with nothing kept of what the resolver lowered it to",
            super::spell::kind(kind)
        ),
    }
}

fn differ(old: &str, new: &str) -> Option<Difference> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let at = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i))?;
    let statement = a[..at.min(a.len())]
        .iter()
        .rev()
        .find(|l| !l.trim_start().starts_with("spans "))
        .map_or("(the first)", |l| l.trim());
    let line = |ls: &[&str]| ls.get(at).map_or("(nothing)", |l| l.trim()).to_string();
    Some(Difference {
        statement: statement.to_string(),
        old: line(&a),
        new: line(&b),
    })
}

/// A lowering as one text: each statement (a module's body under it,
/// indented) with its place and the spans in it, the stack's settings,
/// and each diagnostic.
pub fn dump(lowered: &Lowered) -> String {
    let mut out = String::new();
    match lowered {
        Ok(p) => {
            for s in &p.statements {
                stmt(&mut out, s, 0);
            }
            if let Some(c) = &p.stack {
                let mut spans = Spans::default();
                spans.config(c);
                let _ = writeln!(out, "stack {c:?}\n  spans {}", spans.text());
            }
        }
        Err(diags) => {
            for d in diags {
                let mut spans = Spans::default();
                spans.span(d.span);
                d.labels.iter().for_each(|(s, _)| spans.span(*s));
                d.fixes
                    .iter()
                    .flat_map(|f| &f.edits)
                    .for_each(|(s, _)| spans.span(*s));
                let _ = writeln!(
                    out,
                    "error {}: {d:?}\n  spans {}",
                    place(d.span),
                    spans.text()
                );
            }
        }
    }
    out
}

fn stmt(out: &mut String, s: &Stmt, depth: usize) {
    let pad = "  ".repeat(depth);
    let mut spans = Spans::default();
    spans.stmt(s);
    let at = place(super::head_span(s));
    match s {
        Stmt::Module(m) => {
            let what = if m.component { "component" } else { "module" };
            let _ = writeln!(out, "{pad}{at}: {what} {}", m.name);
            let _ = writeln!(out, "{pad}  spans {}", spans.text());
            for b in &m.body {
                stmt(out, b, depth + 1);
            }
        }
        s => {
            let _ = writeln!(out, "{pad}{at}: {s:?}");
            let _ = writeln!(out, "{pad}  spans {}", spans.text());
        }
    }
}

/// Where `span` is, for a difference's statement.
pub fn place(span: Span) -> String {
    diag::place(span).unwrap_or_else(|| "(compiler)".into())
}

/// Every span in a statement, in the order a walk meets them.
#[derive(Default)]
struct Spans(Vec<Span>);

impl Spans {
    fn text(&self) -> String {
        self.0
            .iter()
            .map(|s| format!("{}:{}..{}@{}", s.file, s.start, s.end, s.origin))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn span(&mut self, s: Span) {
        self.0.push(s);
    }

    /// A module's own span only: its body is dumped statement by statement.
    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Fact(a) => self.atom(a),
            Stmt::Rule(r) => {
                self.atom(&r.head);
                self.lits(&r.body);
            }
            Stmt::Module(m) => self.span(m.span),
            Stmt::Instance(i) | Stmt::Use(i) => {
                self.span(i.span);
                for (_, t, at) in &i.inputs {
                    self.term(t);
                    self.span(*at);
                }
                i.rows.iter().for_each(|r| self.stmt(r));
                i.body.iter().for_each(|b| self.lits(b));
                i.clause.iter().for_each(|b| self.lits(b));
                i.named.iter().for_each(|t| self.term(t));
                i.via.iter().for_each(|v| self.term(&v.scope));
            }
            Stmt::Input(i) => self.input(i),
            Stmt::RelationInput(e) | Stmt::Extern(e) | Stmt::Mixed(e) | Stmt::Mode(e) => {
                self.span(e.span)
            }
            Stmt::Output(o) => {
                self.span(o.span);
                o.value.iter().for_each(|t| self.term(t));
            }
            Stmt::Provider(c) => self.config(c),
            Stmt::Resource(r) => {
                self.span(r.span);
                self.term(&r.typ);
                self.term(&r.name);
                for f in &r.fields {
                    self.span(f.span);
                    self.term(&f.value);
                }
                r.body.iter().for_each(|b| self.lits(b));
            }
            Stmt::Decl(d) => self.span(d.span),
            Stmt::ExternFn(e) => self.span(e.span),
            Stmt::Pending(p) => {
                self.span(p.span);
                let ast::PendingKind::TypeDecl { attrs, .. } = &p.kind;
                attrs.iter().for_each(|a| self.attr(a));
            }
        }
    }

    fn input(&mut self, i: &InputDecl) {
        self.span(i.span);
        i.default.iter().for_each(|t| self.term(t));
        self.lits(&i.refinement);
        self.lits(&i.guard);
        i.fields.iter().for_each(|f| self.input(f));
    }

    fn attr(&mut self, a: &AttrDecl) {
        self.span(a.span);
        self.lits(&a.refinement);
        a.children.iter().for_each(|c| self.attr(c));
    }

    fn config(&mut self, c: &Config) {
        self.span(c.span);
        for (_, t, at) in &c.config {
            self.term(t);
            self.span(*at);
        }
    }

    fn lits(&mut self, ls: &[Lit]) {
        for l in ls {
            match l {
                Lit::Pos(a) | Lit::Not(a) => self.atom(a),
                l => l.terms().for_each(|t| self.term(t)),
            }
        }
    }

    fn atom(&mut self, a: &Atom) {
        self.span(a.span);
        a.args.iter().for_each(|t| self.term(t));
        a.record
            .iter()
            .flat_map(|r| r.values())
            .for_each(|t| self.term(t));
    }

    fn term(&mut self, t: &Term) {
        match t {
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| self.term(a)),
            Term::Obj(m) => m.values().for_each(|a| self.term(a)),
            Term::ListComp { item, body } => {
                self.term(item);
                self.lits(body);
            }
            Term::Val(_) | Term::Var(_) | Term::Wildcard => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two lowerings equal but for one atom's span differ in the dump,
    /// which names the statement and both texts.
    #[test]
    fn a_span_alone_is_a_difference() {
        let old = crate::parser::parse_program("\np(1)\nq(2) where p(2)\n").unwrap();
        let mut new = old.clone();
        let Stmt::Rule(r) = &mut new.statements[1] else {
            panic!("not a rule");
        };
        let Lit::Pos(a) = &mut r.body[0] else {
            panic!("not an atom");
        };
        a.span.origin = 7;
        let (old, new) = (Ok(old), Ok(new));
        assert_eq!(
            differ(&dump(&old), &dump(&old)).map(|d| d.to_string()),
            None
        );
        let d = differ(&dump(&old), &dump(&new)).expect("a difference");
        assert!(d.statement.contains("Rule"), "{d}");
        assert!(d.old.contains("@0") && d.new.contains("@7"), "{d}");
    }

    /// A rule the compiler writes beside the program's carries its flag
    /// (R-212) in the dump, so a builder that makes the rule and forgets
    /// the flag is a difference (R-214).
    #[test]
    fn a_helpers_flag_alone_is_a_difference() {
        let old = crate::parser::parse_program("q(2) where p(2)\n").unwrap();
        let mut new = old.clone();
        let Stmt::Rule(r) = &mut new.statements[0] else {
            panic!("not a rule");
        };
        r.helper = Some(ast::Helper::Negation);
        let d = differ(&dump(&Ok(old)), &dump(&Ok(new))).expect("a difference");
        assert!(
            d.old.contains("helper: None") && d.new.contains("helper: Some(Negation)"),
            "{d}"
        );
    }
}
