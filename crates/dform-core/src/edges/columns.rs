//! The column types (R-34): every relation's columns are typed, declared
//! by a `decl` or inferred from their uses, per definition (R-211: a
//! copy's private relation is its definition's, every copy's uses its
//! own). What gives a column its type is an edge of the graph whose role
//! types it ([`Role`]): a `decl`'s column types, an extern's, a typed
//! input or output, a function's parameter where a column's variable is
//! an argument and its result where it is the value, a reference, an
//! attribute read into it. A literal is its kind, a string unknown
//! (Postgres's unknown literal) until the column is settled. A variable in
//! two columns makes them one, but a variable given to a let or an output
//! flows into its cell one way (R-213): a declared type is the cell's and
//! what flows in is checked assignable to it, an untyped cell is the
//! wider of what flows in. A column with a hard type checks every literal
//! against it; one with literals only is their kind; one with strings only
//! a string.
//!
//! What a column settles as is its signature (`az(name: string, index:
//! int)`, what the language server hovers), and the type a literal
//! written as its relation's argument is read as (R-31: `"10.0.0.0/8"` in
//! an `inet` column a network).

use super::lattice::{self, Given, Hard, Literal, shown_var};
use super::shape::*;
use super::*;
use crate::infer::{Column, Signature};

/// What a column's type settles from, collected by the walk beside the
/// edges.
#[derive(Default)]
pub(super) struct Typing {
    /// Each literal a column takes the kind of: a relation's argument, a
    /// cell's value, `x = v`, an element of `x in [..]`.
    literals: Vec<Held>,
    /// Each variable given to a let or an output: into its cell, one way
    /// (R-213).
    gives: Vec<Give>,
    /// Values one column, which the readings do not join (the readings
    /// keep the program's lowering's relations apart): a copy's output read
    /// and its definition's cell, a used module's relation read and the
    /// module's own.
    pub(super) joins: Vec<(usize, usize)>,
    /// The columns declared `any`: they take anything and join nothing.
    pub(super) open: BTreeSet<Col>,
    /// The relations a `decl` declares.
    pub(super) decls: BTreeSet<(ScopeId, String, usize)>,
    /// Each declared relation's columns by name, what a record atom's
    /// fields are placed by (`p { a: x }`).
    pub(super) fields: BTreeMap<(ScopeId, String), Vec<String>>,
    /// Each column's name: its `decl`'s, else the variable a head first
    /// writes in it; a let with parameters' last, `value`.
    pub(super) names: BTreeMap<Col, String>,
    /// The relations the program's atoms name, by arity: those with a
    /// signature.
    pub(super) used: BTreeSet<(ScopeId, String, usize)>,
    /// The cells, as a message names them: `let apex`, `output ip`.
    pub(super) cells: BTreeMap<Col, String>,
    /// The column of each site that is a relation's argument, by site.
    pub(super) of_site: BTreeMap<usize, usize>,
    /// The externs the program declares: a provider's data source of one
    /// of their names does not type its columns.
    pub(super) externs: BTreeSet<String>,
}

/// A literal the node `node` holds, written `term` at `span`, read as its
/// site is when it has one.
struct Held {
    node: usize,
    term: Term,
    site: Option<usize>,
    span: Span,
}

/// The variable `name` (its node `var`) given to the cell `cell` at
/// `span`.
#[derive(Clone)]
struct Give {
    var: usize,
    name: String,
    cell: usize,
    span: Span,
}

impl Typing {
    /// The node `node` holds the literal `term`, at `site`.
    pub(super) fn literal(&mut self, node: usize, term: Term, site: Option<usize>, span: Span) {
        self.literals.push(Held {
            node,
            term,
            site,
            span,
        });
    }

    /// The variable `name`, its node `var`, is given to the cell `cell`.
    pub(super) fn give(&mut self, var: usize, name: String, cell: usize, span: Span) {
        self.gives.push(Give {
            var,
            name,
            cell,
            span,
        });
    }

    /// A head writes `t` in the column `col`: a variable names it, unless
    /// something named it first; a reference's, its variable (R-185:
    /// `ref(T, W, "")`).
    pub(super) fn head_name(&mut self, col: Col, t: &Term) {
        let named = match t {
            Term::Func { name, args } if name == crate::address::REF => args.get(1),
            t => Some(t),
        };
        if let Some(Term::Var(v)) = named {
            self.names.entry(col).or_insert_with(|| shown_var(v));
        }
    }
}

/// What the columns settled as: the type each site that is a relation's
/// argument is read at, the signatures, and what is wrong.
pub(super) struct Columns {
    pub(super) of_sites: Vec<Option<Ty>>,
    pub(super) signatures: Signatures,
    pub(super) diags: Vec<Diagnostic>,
}

impl Graph<'_> {
    /// The columns' types, once each site's reading is known (`readings`,
    /// in the order the walk found the sites).
    pub(super) fn columns(&mut self, readings: &[Reading]) -> Columns {
        self.provider_columns();
        for (a, b) in std::mem::take(&mut self.typing.joins) {
            self.union(a, b);
        }
        self.throughs();
        let into = self.own_values();
        let lits = self.literals(readings);
        let checks = std::mem::take(&mut self.checks);
        let mut s = Settle {
            classes: self.classes(),
            g: self,
            readings,
            lits,
            into,
            settled: BTreeMap::new(),
            diags: Vec::new(),
        };
        // The declared columns first, by name, then every value in the
        // order the walk met it: the order what is wrong is said in.
        for n in s.g.declared_nodes().into_iter().chain(0..s.g.parent.len()) {
            let r = s.g.find(n);
            s.settled(r);
        }
        let judged = s.judge(checks);
        s.diags.extend(judged);
        let of_sites = (0..s.g.sites.len()).map(|i| s.of_site(i)).collect();
        Columns {
            of_sites,
            signatures: s.signatures(),
            diags: s.diags,
        }
    }

    /// The columns of each provider's data source the program reads and
    /// does not declare itself (R-106), typed as the schema declares them.
    fn provider_columns(&mut self) {
        let schema = self.schema;
        let root = self.scopes.root;
        let read: Vec<&crate::ast::ExternFn> = schema
            .externs
            .iter()
            .filter(|(p, f)| {
                !self.typing.externs.contains(p.as_str())
                    && self
                        .typing
                        .used
                        .contains(&(root, p.to_string(), f.args.len()))
            })
            .map(|(_, f)| f)
            .collect();
        for f in read {
            self.extern_columns(f, root);
        }
    }

    /// A variable given to a cell that nothing types and nothing flows
    /// into is the cell's own value: one node with it, its literals read
    /// at the cell's type (`let c: inet = x where x = "10.0.0.0/8"`). The
    /// others, by the cell they flow into.
    fn own_values(&mut self) -> BTreeMap<usize, Vec<Give>> {
        let gives = std::mem::take(&mut self.typing.gives);
        let mut kept: Vec<Give> = Vec::new();
        for (i, f) in gives.iter().enumerate() {
            let (src, cell) = (self.find(f.var), self.find(f.cell));
            if src == cell {
                continue;
            }
            let given = gives[i + 1..]
                .iter()
                .chain(&kept)
                .any(|g| self.find(g.cell) == src);
            if self.hard(src).is_empty() && !given {
                self.union(f.var, f.cell);
            } else {
                kept.push(f.clone());
            }
        }
        let mut into: BTreeMap<usize, Vec<Give>> = BTreeMap::new();
        for f in kept {
            let cell = self.find(f.cell);
            into.entry(cell).or_default().push(f);
        }
        into
    }

    /// Each value's literals, by its root: each as its site reads it.
    fn literals(&mut self, readings: &[Reading]) -> BTreeMap<usize, Vec<Literal>> {
        let mut out: BTreeMap<usize, Vec<Literal>> = BTreeMap::new();
        for h in std::mem::take(&mut self.typing.literals) {
            let term = match h.site {
                Some(i) => read_as(&readings[i], h.term),
                None => h.term,
            };
            let Term::Val(value) = term else { continue };
            if lattice::kind(&value).is_some() || matches!(value, Value::Str(_) | Value::Obj(_)) {
                let r = self.find(h.node);
                out.entry(r).or_default().push(Literal {
                    value,
                    span: h.span,
                });
            }
        }
        out
    }

    /// The nodes of the columns a `decl` declares, by the relation's name.
    fn declared_nodes(&self) -> Vec<usize> {
        let mut out: Vec<(&str, usize)> = self
            .columns
            .iter()
            .filter(|(c, _)| self.typing.decls.contains(&(c.0, c.1.clone(), c.2)))
            .map(|(c, n)| (c.1.as_str(), *n))
            .collect();
        out.sort();
        out.into_iter().map(|(_, n)| n).collect()
    }

    /// Each value's columns, by its root, in the order the walk made them.
    fn classes(&mut self) -> BTreeMap<usize, Vec<Col>> {
        let mut out: BTreeMap<usize, Vec<Col>> = BTreeMap::new();
        let mut columns: Vec<(usize, Col)> =
            self.columns.iter().map(|(c, n)| (*n, c.clone())).collect();
        columns.sort_by_key(|(n, _)| *n);
        for (n, c) in columns {
            let r = self.find(n);
            out.entry(r).or_default().push(c);
        }
        out
    }

    /// The types the edges of the value `r` that type its column give it:
    /// what fixed each, for the message.
    fn hard(&mut self, r: usize) -> Vec<Hard> {
        let r = self.find(r);
        let schema = self.schema;
        self.edges[r]
            .iter()
            .filter_map(|e| {
                let prints = match e.role {
                    Role::Both => false,
                    Role::Types { prints } => prints,
                    Role::Reads => return None,
                };
                let ty = e.shape.ty(schema).filter(|t| *t != Ty::Any)?;
                Some(Hard {
                    ty,
                    span: e.span,
                    what: e.what.clone(),
                    col: e.col.clone(),
                    prints,
                })
            })
            .collect()
    }
}

/// The term a site's literal `t` is read as, its reading `reading`: what
/// the second walk writes, where it says nothing.
pub(super) fn read_as(reading: &Reading, t: Term) -> Term {
    match reading {
        Reading::At(ty, _) => types::literal(ty, t.clone()).unwrap_or(t),
        Reading::Untyped | Reading::Disagree(..) => t,
    }
}

/// The types a literal written as a relation's argument is read at
/// (R-31): a network, an address, a float, a quantity, a time, a version,
/// a range.
fn read_at(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(s) if matches!(s.as_str(),
        "inet" | "ip" | "float" | "bytes" | "cpu" | "duration" | "time"
        | "semver") || crate::range::element(s).is_some())
}

/// The columns being settled: each value's type, once, after what flows
/// into it.
pub(super) struct Settle<'g, 'a> {
    pub(super) g: &'g mut Graph<'a>,
    /// Each site's reading.
    pub(super) readings: &'g [Reading],
    /// Each value's columns and literals, and the variables given to it,
    /// by its root.
    classes: BTreeMap<usize, Vec<Col>>,
    lits: BTreeMap<usize, Vec<Literal>>,
    into: BTreeMap<usize, Vec<Give>>,
    pub(super) settled: BTreeMap<usize, Option<Ty>>,
    diags: Vec<Diagnostic>,
}

impl Settle<'_, '_> {
    /// The type of the value `r`, settled once, after what flows into it.
    fn settled(&mut self, r: usize) -> Option<Ty> {
        if let Some(t) = self.settled.get(&r) {
            return t.clone();
        }
        // A cycle of flows reads nothing from itself.
        self.settled.insert(r, None);
        let mut given = Vec::new();
        for f in self.into.get(&r).cloned().unwrap_or_default() {
            let src = self.g.find(f.var);
            let Some(ty) = self.settled(src) else {
                continue;
            };
            let hard = self.g.hard(src);
            let what = hard
                .iter()
                .find(|h| h.ty == ty)
                .or(hard.first())
                .map(|h| h.what.clone())
                .or_else(|| self.cell_name(src))
                .unwrap_or_else(|| "given it".into());
            given.push(Given {
                ty,
                var: shown_var(&f.name),
                what,
                span: f.span,
            });
        }
        let t = self.settle(r, &given);
        self.settled.insert(r, t.clone());
        t
    }

    /// Settle the value `r`, given `given` by the rules that flow into it:
    /// its type, or the errors that say why it has none.
    fn settle(&mut self, r: usize, given: &[Given]) -> Option<Ty> {
        let mut hard = self.g.hard(r);
        // A value type given to a builtin's `string` parameter is its print
        // there (R-133: `str.starts_with(c.image, "ghcr.io/")` over an `oci`).
        if hard.iter().any(|h| !h.prints && lattice::printed(&h.ty)) {
            hard.retain(|h| !h.prints);
        }
        let lits = self.lits.get(&r).cloned().unwrap_or_default();
        let col = self.column_name(r);
        let from = match self.joined(hard, &col) {
            Ok(from) => from,
            Err(d) => {
                self.diags.push(*d);
                return None;
            }
        };
        if let Some(f) = from {
            return lattice::declared(f, &col, given, &lits, &mut self.diags);
        }
        if let Some(g) = given.first() {
            return lattice::widened(&col, g, &given[1..], &lits, &mut self.diags);
        }
        lattice::of_literals(&col, &lits, &mut self.diags)
    }

    /// The let or the output the value `r` is the cell of, as the program
    /// names it: `let apex`, `output ip`.
    fn cell_name(&self, r: usize) -> Option<String> {
        let cols = self.classes.get(&r)?;
        cols.iter()
            .find_map(|c| self.g.typing.cells.get(c).cloned())
    }

    /// The value `r` as an error names it: its cell, else its column that
    /// a declaration names, else any.
    fn column_name(&self, r: usize) -> String {
        self.cell_name(r)
            .or_else(|| {
                let cols = self.classes.get(&r)?;
                cols.iter()
                    .filter(|c| !c.1.contains("__"))
                    .min_by_key(|c| !self.g.typing.decls.contains(&(c.0, c.1.clone(), c.2)))
                    .or(cols.first())
                    .map(|c| self.shown_col(c))
            })
            .unwrap_or_else(|| "this value".into())
    }

    /// The value `r`'s column of a relation the program has, as a message
    /// names it: one a declaration names first.
    pub(super) fn relation_column(&self, r: usize) -> Option<String> {
        let cols = self.classes.get(&r)?;
        let col = cols
            .iter()
            .filter(|c| !lattice::untyped(&c.1) && lattice::function(&c.1).is_none())
            .filter(|c| !c.1.starts_with(OUTPUT))
            .min_by_key(|c| !self.g.typing.decls.contains(&(c.0, c.1.clone(), c.2)))?;
        Some(self.shown_col(col))
    }

    /// How a message names a column: `` `az`'s column `index` ``, or by
    /// its position.
    fn shown_col(&self, c: &Col) -> String {
        let (_, pred, _, i) = c;
        match self.g.typing.names.get(c) {
            Some(n) => format!("`{pred}`'s column `{n}`"),
            None => format!("`{pred}`'s column {}", i + 1),
        }
    }

    /// The types `hard` says the column `col` is, joined: the narrower of
    /// two compatible, a reference to either of two references (R-185, a
    /// column a rule per type fills); two that are not is the error.
    fn joined(&self, hard: Vec<Hard>, col: &str) -> Result<Option<Hard>, Box<Diagnostic>> {
        let mut from: Option<Hard> = None;
        for h in hard {
            let Some(f) = &from else {
                from = Some(h);
                continue;
            };
            let ty = match (&f.ty, &h.ty) {
                (Ty::Ref(a), Ty::Ref(b)) => Ty::ref_union(a, b),
                (fty, hty) if lattice::compatible(fty, hty) => lattice::narrower(fty.clone(), hty),
                _ => return Err(Box::new(self.disjoint(&h, f, col))),
            };
            from = Some(Hard { ty, ..f.clone() });
        }
        Ok(from)
    }

    /// The error of the column `col` that `h` says is one type and `f`
    /// another.
    fn disjoint(&self, h: &Hard, f: &Hard, col: &str) -> Diagnostic {
        let named = |h: &Hard| match &h.col {
            Some(c) => self.shown_col(c),
            None => col.to_string(),
        };
        lattice::disjoint(h, f, named(h), named(f))
    }

    /// The type the site `i` is read at as its column's (R-34), if it is a
    /// relation's argument and its column settled as a type literals are
    /// read as.
    fn of_site(&mut self, i: usize) -> Option<Ty> {
        let n = *self.g.typing.of_site.get(&i)?;
        let r = self.g.find(n);
        self.settled.get(&r).cloned().flatten().filter(read_at)
    }

    /// Each relation's signature: the program's relations an atom names
    /// at an arity (not a function's call, nor a table's), each column's
    /// name and settled type (`any` where it is open).
    fn signatures(&mut self) -> Signatures {
        let mut out = Signatures::default();
        let used: Vec<(ScopeId, String, usize)> = self.g.typing.used.iter().cloned().collect();
        for (scope, pred, arity) in used {
            if lattice::function(&pred).is_some() || pred.starts_with("table.") {
                continue;
            }
            let columns = (0..arity)
                .map(|i| self.column((scope, pred.clone(), arity, i)))
                .collect();
            let def = self.g.scopes.definition(scope).map(|(p, _)| p.clone());
            out.insert(def.unwrap_or_default(), arity, Signature { pred, columns });
        }
        out
    }

    /// The column `c`: its name and its settled type.
    fn column(&mut self, c: Col) -> Column {
        let ty = if self.g.typing.open.contains(&c) {
            Some(Ty::Any)
        } else {
            let n = self.g.columns.get(&c).copied();
            n.and_then(|n| {
                let r = self.g.find(n);
                self.settled.get(&r).cloned().flatten()
            })
        };
        Column {
            name: self.g.typing.names.get(&c).cloned(),
            ty,
        }
    }
}
