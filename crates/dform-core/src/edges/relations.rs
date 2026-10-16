//! A relation's atom in the walk (R-34, R-192): each argument its
//! column's, a literal held in a column its kind's, a record's fields
//! in their columns, a copy's output its definition's cell.

use super::lattice;
use super::shape::*;
use super::*;

impl Graph<'_> {
    /// The node `n` holds the literal `term` (its site `site`): its kind
    /// is the column's, and what collection it is the value's (R-34).
    pub(super) fn literal(&mut self, n: usize, term: Term, site: Option<usize>, span: Span) {
        self.checks.shape(n, &term, span);
        self.typing.literal(n, term, site, span);
    }

    /// The relation the column `n` is of has a signature: a let's, an
    /// input's (R-34), read or not.
    pub(super) fn uses(&mut self, n: usize) {
        if let Some((scope, pred, arity, _)) = self.col_of[n].clone() {
            self.typing.used.insert((scope, pred, arity));
        }
    }

    /// The node `n` is the cell a message names `name` (`let apex`).
    pub(super) fn cell(&mut self, n: usize, name: String) {
        if let Some(col) = self.col_of[n].clone() {
            self.typing.cells.insert(col, name);
        }
    }

    /// A relation's atom: each argument is its column's. A relation the
    /// program has (not one the compiler writes with values of any type,
    /// [`lattice::untyped`]) takes its arguments' types (R-34), and a
    /// head's variables name its columns; a column declared `any`, and a
    /// compiler's relation's, joins nothing: its arguments only reach
    /// each other's types.
    pub(super) fn relation(&mut self, a: &mut Atom, at: &At, head: bool) {
        if let Some(mut placed) = self.record(a, at.scope) {
            return self.relation(&mut placed, at, head);
        }
        let at = &At {
            atom: a.span,
            ..at.clone()
        };
        let (arity, pred, span) = (a.args.len(), a.pred.clone(), a.span);
        let typed = !lattice::untyped(&pred);
        if typed {
            let (declaring, name) = self.resolve(at.scope, &pred);
            self.typing.used.insert((declaring, name, arity));
        }
        for (i, t) in a.args.iter_mut().enumerate() {
            let n = self.column(at.scope, &pred, arity, i);
            let col = self.col_of[n].clone().expect("a column's node");
            if !typed || self.typing.open.contains(&col) {
                let x = self.node();
                self.reach(x, n);
                self.place(x, Vec::new(), t, at);
                continue;
            }
            if head {
                self.typing.head_name(col, t);
            }
            self.argument(n, t, at, span);
        }
    }

    /// A record atom (`p { a: x }`) with its fields in their columns by
    /// the relation's `decl`, a field it does not write a hole; `None` for
    /// any other, and where no `decl` says the columns.
    pub(super) fn record(&self, a: &Atom, scope: ScopeId) -> Option<Atom> {
        let record = a.record.as_ref().filter(|_| a.args.is_empty())?;
        let fields = self.typing.fields.get(&self.resolve(scope, &a.pred))?;
        let args = fields
            .iter()
            .map(|f| record.get(f).cloned().unwrap_or(Term::Wildcard))
            .collect();
        Some(Atom {
            args,
            record: None,
            ..a.clone()
        })
    }

    /// `t` in the column `n` of an atom at `span`: [`Self::held`], and a
    /// computed value's type the column's (R-34); a literal is read at the
    /// column's type in turn.
    pub(super) fn argument(&mut self, n: usize, t: &mut Term, at: &At, span: Span) {
        let written = unmarked(t);
        match self.held(n, t, at, span) {
            Some(site) => {
                self.typing.of_site.insert(site, n);
            }
            None if lattice::constant(&written).is_none() => {
                self.checks.shape(n, &written, span);
                if let Some((ty, what)) = lattice::term_type(&written) {
                    let role = Role::Types { prints: false };
                    self.edge(n, Shape::Ty(ty), what, span, role);
                }
            }
            None => {}
        }
    }

    /// `t` placed in the column `n` at `span`, and its literal's kind the
    /// column's (R-34), as its site reads it: the site of a literal
    /// written alone.
    pub(super) fn held(&mut self, n: usize, t: &mut Term, at: &At, span: Span) -> Option<usize> {
        let written = unmarked(t);
        let first = self.sites.len();
        self.place(n, Vec::new(), t, at);
        if let Some(site) = self.one_site(first, n) {
            self.literal(n, written, Some(site), span);
            return Some(site);
        }
        if let Some(v) = lattice::constant(&written) {
            self.literal(n, Term::Val(v), None, span);
        }
        None
    }

    /// The site placing a term in `n` made since `first`, when it made
    /// one, the term's own (a literal written alone, `500m`, a range).
    pub(super) fn one_site(&self, first: usize, n: usize) -> Option<usize> {
        let one = self.sites.len() == first + 1;
        (one && self.sites[first].node == n && self.sites[first].path.is_empty()).then_some(first)
    }

    /// `output(C, "k", V)`, the output `k` of the copy `C` read in
    /// `scope`: `V` is the cell of its definition's (R-34), a variable one
    /// column with it, a literal one it holds.
    pub(super) fn copy_output(
        &mut self,
        copy: &Term,
        k: &str,
        v: Term,
        scope: ScopeId,
        span: Span,
    ) {
        let Term::Val(Value::Str(copy)) = copy else {
            return;
        };
        let body = self.scopes.copied_body(scope, copy);
        let Some(body) = body.or_else(|| self.used(scope, copy)) else {
            return;
        };
        let cell = self.column(body, &format!("{OUTPUT}{k}"), 1, 0);
        match v {
            Term::Var(x) => {
                let x = self.var(&x);
                self.typing.joins.push((x, cell));
            }
            v @ Term::Val(_) => self.literal(cell, v, None, span),
            _ => {}
        }
    }
}
