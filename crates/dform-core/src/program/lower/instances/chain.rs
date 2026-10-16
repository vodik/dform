//! The copies a lowering is under, and what each writes differently of
//! the definition's items it lowers: its private relations by its name
//! (`n::p`, `n.inner::p`), its resources, cells and outputs under its
//! scope, and, for a copy named by its clause (R-191), its name per row.
//!
//! A lowering under copies writes each term and atom final when it makes
//! it: each copy's part applied from the innermost out, starting where
//! the part starts (a [`From`]): a name the definition writes, at the
//! innermost copy; a read of an enclosing definition's item (R-186), at
//! the copy of that definition it is taken from; a name of the user's
//! (`Reach::Absolute`), at none.

use crate::ast::{Atom, Resource, Span, Term, Via, str_term};
use crate::modules::{INPUT, LET, LEXICAL_OF, ROWS};
use crate::value::Value;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

/// One copy a lowering is under.
#[derive(Debug)]
pub struct Frame {
    /// The copy item it is of.
    pub item: crate::program::ItemId,
    /// Its name in its user's scope (`n`, `agent-${i}`).
    pub scope: String,
    /// The definition it copies (`backups`, `backups.volume`).
    pub def: String,
    /// A component's copy: a resource written as a variable is its own;
    /// a module's ranges over every resource its user sees.
    pub vars: bool,
    /// The relations its definition makes private to each copy, by the
    /// definition's names.
    pub keys: Rc<BTreeSet<String>>,
    /// The module instance a component is named through (R-186): what
    /// its reads of that module's items read.
    pub via: Option<Via>,
    /// A copy named by its clause (R-191).
    pub named: Option<Named>,
    /// The first read in it of an enclosing definition's item no instance
    /// holds: the definition, the name and where it is read.
    pub unreached: RefCell<Option<(String, String, Span)>>,
}

/// A copy named by its clause: the scope its header writes and the term
/// that names each copy, inside it (the gate's) and on its user's side
/// (the clause's).
#[derive(Debug)]
pub struct Named {
    pub scope: String,
    pub inside: Term,
    pub by_row: Term,
}

/// What a copy does to what it writes, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Its private relations named as its own.
    Rename,
    /// Its resources, cells and outputs put under its scope.
    Rewrite,
    /// Its name per row, for a copy named by its clause.
    Named,
}

/// Where the copies' parts start for a piece of what a lowering writes:
/// at the copy `frame` (an index into the chain, outermost 0) from
/// `stage` on, then every copy outside it. `by_row`: the piece is the
/// user's side of that copy (its inputs, rows, gate), named by the
/// clause's term; `keep`: that copy's [`Stage::Rewrite`] is not applied
/// (what a dependent input's clause reads, renamed only).
#[derive(Debug, Clone, Copy)]
pub struct From {
    pub frame: usize,
    pub stage: Stage,
    pub by_row: bool,
    pub keep: bool,
}

impl From {
    /// At `frame`, from `stage` on.
    pub fn at(frame: usize, stage: Stage) -> From {
        From {
            frame,
            stage,
            by_row: false,
            keep: false,
        }
    }

    /// Whether the copy `f`'s stage `s` applies to the piece.
    fn reaches(&self, f: usize, s: Stage) -> bool {
        f < self.frame
            || (f == self.frame && s >= self.stage && !(self.keep && s == Stage::Rewrite))
    }

    /// Whether the piece is the user's side of the copy `f`.
    fn by_row_at(&self, f: usize) -> bool {
        self.by_row && self.frame == f
    }
}

/// Whether the copy `f`'s stage `s` applies to a piece starting at
/// `from` (none: no copy's does).
fn reached(from: Option<From>, f: usize, s: Stage) -> bool {
    from.is_some_and(|x| x.reaches(f, s))
}

/// How an enclosing definition's item is read under the chain (R-186).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lexical {
    /// As the copy `frame`'s own: it is a copy of the definition.
    Own(usize),
    /// Through the module instance the component copy `frame` is named
    /// through.
    Via(usize),
    /// As the program's: no copy applies.
    Program,
    /// No copy holds it.
    Unreached,
}

/// The copies a lowering is under, outermost first.
#[derive(Debug, Default)]
pub struct Chain {
    pub frames: Vec<Frame>,
}

impl Chain {
    /// Where a piece the innermost copy's definition writes starts.
    pub fn all(&self) -> Option<From> {
        let n = self.frames.len();
        (n > 0).then(|| From::at(n - 1, Stage::Rename))
    }

    /// The copy an enclosing definition `body`'s item `name` is read
    /// through from inside the chain, read at `span`: the innermost that
    /// copies `body`, or a component's named through an instance of it;
    /// `""` is the program. A component copy outside every instance of
    /// `body` is where the read is an error: recorded there.
    pub fn lexical(&self, body: &str, name: &str, span: Span) -> Lexical {
        if body.is_empty() {
            return Lexical::Program;
        }
        for (i, f) in self.frames.iter().enumerate().rev() {
            if f.def == body {
                return Lexical::Own(i);
            }
            if !f.vars {
                continue;
            }
            if f.via.as_ref().is_some_and(|v| v.module == body) {
                return Lexical::Via(i);
            }
            if !self.frames[..i].iter().any(|o| o.def == body) {
                let mut first = f.unreached.borrow_mut();
                first.get_or_insert_with(|| (body.to_string(), name.to_string(), span));
                return Lexical::Unreached;
            }
        }
        Lexical::Unreached
    }

    /// The relation an enclosing definition's `name` is read by, and
    /// where the copies' parts start for it.
    pub fn lexical_pred(&self, body: &str, name: &str, span: Span) -> (String, Option<From>) {
        match self.lexical(body, name, span) {
            Lexical::Own(i) => (name.to_string(), Some(From::at(i, Stage::Rename))),
            Lexical::Via(i) => {
                let via = self.frames[i]
                    .via
                    .as_ref()
                    .expect("a copy through a module");
                (
                    format!("{}::{name}", via.name),
                    Some(From::at(i, Stage::Named)),
                )
            }
            Lexical::Program | Lexical::Unreached => (name.to_string(), None),
        }
    }

    /// The resource address an enclosing definition's `name` is read by,
    /// and where the copies' parts start for it in an atom's column; a
    /// read through a module instance is that instance's resource, final.
    pub fn lexical_term(&self, body: &str, name: &str, span: Span) -> (Term, Option<From>) {
        match self.lexical(body, name, span) {
            Lexical::Own(i) => (str_term(name), Some(From::at(i, Stage::Rename))),
            Lexical::Via(i) => {
                let via = self.frames[i]
                    .via
                    .as_ref()
                    .expect("a copy through a module");
                let scope = str_term(&via.name);
                let at = (!via.absolute).then_some(From::at(i, Stage::Named));
                let t = scoped(scope, str_term(name));
                (
                    self.scoped_fold(t, at, Some(From::at(i, Stage::Named))),
                    None,
                )
            }
            Lexical::Program | Lexical::Unreached => (str_term(name), None),
        }
    }

    /// `atom` with each copy's part applied from the innermost out: its
    /// relation's name from `pred`, each argument from its own.
    pub fn atom(&self, mut atom: Atom, pred: Option<From>, args: &[Option<From>]) -> Atom {
        let mut at: Vec<Option<From>> = args.to_vec();
        at.resize(atom.args.len(), args.last().copied().flatten());
        for f in (0..self.frames.len()).rev() {
            let frame = &self.frames[f];
            if reached(pred, f, Stage::Rename) {
                atom.pred = frame.rename(&atom.pred);
            }
            frame.rewrite(f, &mut atom, pred, &mut at);
            frame.name_atom(f, &mut atom, pred, &mut at);
        }
        atom
    }

    /// [`Self::atom`] with every part starting at `from`.
    pub fn atom_from(&self, atom: Atom, from: Option<From>) -> Atom {
        let n = atom.args.len();
        self.atom(atom, from, &vec![from; n])
    }

    /// A relation's declaration (`extern`, `mixed`, a mode) of `pred`
    /// `from`: named as each copy's, a column more per copy named by its
    /// clause that owns it.
    pub fn relation(&self, mut e: crate::ast::Extern, from: Option<From>) -> crate::ast::Extern {
        for f in (0..self.frames.len()).rev() {
            let frame = &self.frames[f];
            if reached(from, f, Stage::Rename) {
                e.pred = frame.rename(&e.pred);
            }
            if reached(from, f, Stage::Named)
                && let Some(n) = &frame.named
                && n.owns(&e.pred)
            {
                e.arity += 1;
            }
        }
        e
    }

    /// A resource statement `from`: its name the copy's own at each
    /// (`name`: where its name starts).
    pub fn resource(&self, mut r: Resource, name: Option<From>) -> Resource {
        for f in (0..self.frames.len()).rev() {
            let frame = &self.frames[f];
            if reached(name, f, Stage::Rewrite) {
                r.name = frame.scoped(std::mem::replace(&mut r.name, Term::Wildcard), true);
            }
            if reached(name, f, Stage::Named)
                && let Some(n) = &frame.named
            {
                let copy = n.term(name.is_some_and(|x| x.by_row_at(f)));
                r.typ = n.rename_term(std::mem::replace(&mut r.typ, Term::Wildcard), copy);
                r.name = n.rename_term(std::mem::replace(&mut r.name, Term::Wildcard), copy);
            }
        }
        r
    }

    /// `__ref(typ, addr, path)` (a resource given as a value), its
    /// address the copy's own at each from `addr`'s start.
    pub fn reference(&self, typ: Term, addr: Term, at: Option<From>, path: Term) -> Term {
        let mut addr = addr;
        for f in (0..self.frames.len()).rev() {
            let frame = &self.frames[f];
            if reached(at, f, Stage::Rewrite) {
                addr = frame.scoped(addr, frame.vars);
            }
            if reached(at, f, Stage::Named)
                && let Some(n) = &frame.named
            {
                addr = n.rename_term(addr, n.term(at.is_some_and(|x| x.by_row_at(f))));
            }
        }
        Term::Func {
            name: crate::address::REF.into(),
            args: vec![typ, addr, path],
        }
    }

    /// A scoped name (`scoped(S, n)`) made where its scope starts at
    /// `scope_at`, its whole at `at`: each copy outside puts its scope in
    /// front of `S`, and names it per row.
    pub fn scoped_fold(&self, t: Term, scope_at: Option<From>, at: Option<From>) -> Term {
        let Term::Func { name, mut args } = t else {
            unreachable!("a scoped name")
        };
        for f in (0..self.frames.len()).rev() {
            let frame = &self.frames[f];
            if reached(scope_at, f, Stage::Rewrite) {
                args[0] = frame.prefix(std::mem::replace(&mut args[0], Term::Wildcard));
            }
            if reached(at, f, Stage::Named)
                && let Some(n) = &frame.named
            {
                let copy = n.term(at.is_some_and(|x| x.by_row_at(f)));
                args = args.into_iter().map(|a| n.rename_term(a, copy)).collect();
            }
        }
        Term::Func { name, args }
    }
}

/// `scoped(scope, name)`.
pub fn scoped(scope: Term, name: Term) -> Term {
    Term::Func {
        name: crate::address::SCOPED.into(),
        args: vec![scope, name],
    }
}

impl Frame {
    /// `p` as this copy names it: `n::p` when its definition makes it
    /// private, `n.inner::p` for the private `inner::p` of a copy inside.
    pub fn rename(&self, p: &str) -> String {
        match self.keys.contains(p) {
            true => private_name(&self.scope, p),
            false => p.to_string(),
        }
    }

    /// A scope relative to this copy made absolute: `n.inner`, `n` for
    /// the copy's own `""`.
    fn prefix(&self, inner: Term) -> Term {
        match inner {
            Term::Val(Value::Str(s)) => str_term(&crate::types::dotted(&self.scope, &s)),
            t => Term::Func {
                name: crate::address::FORMAT.into(),
                args: vec![str_term(&format!("{}.%s", self.scope)), t],
            },
        }
    }

    /// A resource's name in this copy (`scoped(n, name)`): a name a copy
    /// inside scoped, `scoped(inner, x)`, is `scoped(n.inner, x)`; a name
    /// that is already an address is itself; with `vars` false (a
    /// module's), only a name written out is its own.
    fn scoped(&self, name: Term, vars: bool) -> Term {
        match name {
            Term::Func { name: f, mut args } if f == crate::address::SCOPED => {
                args[0] = self.prefix(std::mem::replace(&mut args[0], Term::Wildcard));
                Term::Func { name: f, args }
            }
            Term::Val(Value::Str(s)) if crate::address::is_scoped(&s) => Term::Val(Value::Str(s)),
            name if !vars && !matches!(name, Term::Val(_)) => name,
            name => scoped(str_term(&self.scope), name),
        }
    }

    /// This copy's scope put on the columns of `atom` that hold one:
    /// a cell's, a resource's address, an output's, a copy's record.
    fn rewrite(&self, f: usize, atom: &mut Atom, pred: Option<From>, at: &mut Vec<Option<From>>) {
        let on = |i: usize, at: &[Option<From>]| reached(at[i], f, Stage::Rewrite);
        let pseudo = |a: &Term| matches!(a, Term::Val(Value::Str(s)) if s == INPUT || s == LET || s == "output");
        let args = &mut atom.args;
        let take = |args: &mut Vec<Term>, i: usize| std::mem::replace(&mut args[i], Term::Wildcard);
        match (atom.pred.as_str(), args.len()) {
            ("arg" | "attr", 4 | 5) | (crate::refine::ATTR_REFINE, 4) if pseudo(&args[0]) => {
                if on(1, at) {
                    args[1] = self.prefix(take(args, 1));
                }
            }
            ("want", 2) | ("arg" | "attr", 4) | ("arg", 5) | ("arg_add", 4) => {
                if on(1, at) {
                    args[1] = self.scoped(take(args, 1), self.vars);
                }
            }
            ("output", 2) | (ROWS, 2) if reached(pred, f, Stage::Rewrite) => {
                args.insert(0, str_term(&self.scope));
                at.insert(0, Some(From::at(f, Stage::Named)));
            }
            (LEXICAL_OF, 3) => {
                for i in [0, 2] {
                    if on(i, at) {
                        args[i] = self.prefix(take(args, i));
                    }
                }
            }
            (crate::modules::INSTANCE_OF, 3) | (crate::transform::SECRET_CELL, 3) => {
                if on(1, at) {
                    args[1] = self.prefix(take(args, 1));
                }
            }
            ("output", 3) | (ROWS, 3) if on(0, at) => {
                args[0] = self.prefix(take(args, 0));
            }
            _ => {}
        }
    }

    /// For a copy named by its clause, `atom` per row: each argument
    /// this stage reaches with the copy's name in place of its scope, and
    /// a relation private to it given the copy's name first.
    fn name_atom(&self, f: usize, atom: &mut Atom, pred: Option<From>, at: &mut Vec<Option<From>>) {
        let Some(n) = &self.named else {
            return;
        };
        let copy = n.term(pred.is_some_and(|x| x.by_row_at(f)));
        for (a, from) in atom.args.iter_mut().zip(at.iter()) {
            if reached(*from, f, Stage::Named) {
                *a = n.rename_term(std::mem::replace(a, Term::Wildcard), copy);
            }
        }
        if reached(pred, f, Stage::Named) && n.owns(&atom.pred) {
            atom.args.insert(0, copy.clone());
            at.insert(0, None);
        }
    }
}

/// `p` private to `scope`: `scope::p`, or `scope.inner::p` for a name
/// already private to a copy inside it.
pub fn private_name(scope: &str, p: &str) -> String {
    match p.contains("::") {
        true => format!("{scope}.{p}"),
        false => format!("{scope}::{p}"),
    }
}

impl Named {
    /// The term that names the copy: the clause's on its user's side,
    /// the gate's inside it.
    pub fn term(&self, by_row: bool) -> &Term {
        match by_row {
            true => &self.by_row,
            false => &self.inside,
        }
    }

    /// What `s` says below the scope: `""` for the scope itself, `.x` for
    /// a scope or an address under it; `None` for any other text.
    fn under<'s>(&self, s: &'s str) -> Option<&'s str> {
        let rest = s.strip_prefix(self.scope.as_str())?;
        (rest.is_empty() || rest.starts_with('.')).then_some(rest)
    }

    /// Whether `pred` is private to the scope (`agent-${i}::p`, or
    /// `agent-${i}.inner::p` of a copy inside it).
    pub fn owns(&self, pred: &str) -> bool {
        pred.split_once("::")
            .is_some_and(|(scope, _)| self.under(scope).is_some())
    }

    /// `rest` under the copy's name: the name itself, or `format("%s.x",
    /// name, args..)`.
    fn scope_term(&self, rest: &str, args: Vec<Term>, copy: &Term) -> Term {
        if rest.is_empty() && args.is_empty() {
            return copy.clone();
        }
        let mut all = vec![Term::Val(Value::Str(format!("%s{rest}"))), copy.clone()];
        all.extend(args);
        Term::Func {
            name: crate::address::FORMAT.into(),
            args: all,
        }
    }

    /// `t` with each scope it writes under the copy's named by `copy`.
    fn rename_term(&self, t: Term, copy: &Term) -> Term {
        match t {
            Term::Val(Value::Str(s)) => match self.under(&s) {
                Some(rest) => self.scope_term(rest, Vec::new(), copy),
                None => Term::Val(Value::Str(s)),
            },
            // A template a copy inside this one scoped, `format(
            // "agent-${i}.%s", N)`: `format("%s.%s", copy, N)`.
            Term::Func { name, mut args } if name == crate::address::FORMAT => {
                let rest = match args.first() {
                    Some(Term::Val(Value::Str(tmpl))) => self.under(tmpl).map(str::to_string),
                    _ => None,
                };
                if rest.is_some() {
                    args.remove(0);
                }
                let args: Vec<Term> = args
                    .into_iter()
                    .map(|t| self.rename_term(t, copy))
                    .collect();
                match rest {
                    Some(rest) => self.scope_term(&rest, args, copy),
                    None => Term::Func { name, args },
                }
            }
            Term::Func { name, args } => Term::Func {
                name,
                args: args
                    .into_iter()
                    .map(|t| self.rename_term(t, copy))
                    .collect(),
            },
            Term::List(xs) => {
                Term::List(xs.into_iter().map(|t| self.rename_term(t, copy)).collect())
            }
            Term::Obj(m) => Term::Obj(
                m.into_iter()
                    .map(|(k, v)| (k, self.rename_term(v, copy)))
                    .collect(),
            ),
            other => other,
        }
    }
}
