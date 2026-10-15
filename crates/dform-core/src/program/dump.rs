//! A lowering as one text (R-211): every statement with every span in it
//! and its origin (a span's `==` holds of any two, and an atom's `Debug`
//! leaves it out, so neither alone would see a wrong one), the stack's
//! settings, and every diagnostic. It is the printer of the
//! dump-and-compare check the migration's steps are verified by: a hook
//! kept out of the tree writes each `lower_stack` and `modules::expand`
//! output as [`dump`] prints it, and the suite's dumps before and after a
//! change are compared as sets.

use crate::ast::{self, Atom, AttrDecl, Config, InputDecl, Lit, Span, Stmt, Term};
use crate::diag::{self, Diagnostic};
use std::fmt::Write as _;

/// A lowering as one text: each statement (a module's body under it,
/// indented) with its place and the spans in it, the stack's settings,
/// and each diagnostic.
pub fn dump(lowered: &Result<ast::Program, Vec<Diagnostic>>) -> String {
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

/// Where `span` is, as a dump prints a statement's place.
fn place(span: Span) -> String {
    diag::place(span).unwrap_or_else(|| "(compiler)".into())
}

/// Every span in a statement, in the order a walk meets them.
#[derive(Default)]
pub(crate) struct Spans(Vec<Span>);

impl Spans {
    pub(crate) fn text(&self) -> String {
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

    pub(crate) fn lits(&mut self, ls: &[Lit]) {
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

    /// Two lowerings equal but for one atom's span dump differently.
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
        let (old, new) = (dump(&Ok(old)), dump(&Ok(new)));
        assert_ne!(old, new);
        assert!(old.contains("@0") && new.contains("@7"), "{old}\n{new}");
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
        let (old, new) = (dump(&Ok(old)), dump(&Ok(new)));
        assert!(
            old.contains("helper: None") && new.contains("helper: Some(Negation)"),
            "{old}\n{new}"
        );
    }
}
