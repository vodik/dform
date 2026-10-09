//! A statement as the derivation prints it: its text from the
//! source, the block elided but the entry the fact came from, each name it reads
//! shown with its value as the firing bound it (`Cx`), a call evaluated as the
//! engine would.

use super::{place_in, structured};
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::circuit::Fact;
use crate::engine;
use crate::ir::Address;
use crate::query::Redactor;
use crate::spell;
use crate::syntax::resolve::{Piece, capitalise, pieces};
use crate::syntax::{SyntaxElement, SyntaxKind, SyntaxNode};
use crate::value::Value;
use rowan::NodeOrToken;
use std::collections::BTreeMap;

/// Runs of whitespace as one space: a statement on one line.
pub(super) fn collapse(s: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let mut esc = false;
    let mut space = false;
    for c in s.trim().chars() {
        if in_str {
            out.push(c);
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if c == '"' {
            in_str = true;
        }
        out.push(c);
    }
    out
}

/// The statement the span `start..end` is in, and the block entry it is
/// when it is one (a rule lowered out of a resource's or an instance's
/// `k = v`).
pub(super) fn statement_at(
    root: &SyntaxNode,
    start: usize,
    end: usize,
) -> Option<(SyntaxNode, Option<SyntaxNode>)> {
    let len: usize = root.text_range().end().into();
    if start > len || end > len || start > end {
        return None;
    }
    let range = rowan::TextRange::new((start as u32).into(), (end as u32).into());
    let mut n = match root.covering_element(range) {
        NodeOrToken::Node(n) => n,
        NodeOrToken::Token(t) => t.parent()?,
    };
    loop {
        let parent = n.parent()?;
        match parent.kind() {
            SyntaxKind::SOURCE_FILE | SyntaxKind::STMT_BLOCK => return Some((n, None)),
            SyntaxKind::BLOCK if n.kind() == SyntaxKind::ASSIGN => {
                return Some((parent.parent()?, Some(n)));
            }
            _ => n = parent,
        }
    }
}

/// What of a statement is printed: its text, with the block elided but
/// for the entry that fired and a braced clause as one line, and, in
/// source order, the variables and the computed terms in that text.
#[derive(Default)]
pub(super) struct Shown {
    pub(super) text: String,
    pub(super) vars: Vec<String>,
    /// Each `[_]` of a chain (R-162): how `with` names it and its core
    /// variable (`resolve::each_var`).
    pub(super) each: Vec<(String, String)>,
    pub(super) terms: Vec<SyntaxElement>,
}

impl Shown {
    pub(super) fn render(&mut self, stmt: &SyntaxNode, n: &SyntaxNode, entry: Option<&SyntaxNode>) {
        use SyntaxKind::*;
        for el in n.children_with_tokens() {
            match el {
                NodeOrToken::Token(t) => match t.kind() {
                    WHITESPACE | COMMENT => self.text.push(' '),
                    STRING => {
                        if has_hole(t.text()) && !negated(n, stmt) {
                            self.terms.push(NodeOrToken::Token(t.clone()));
                        }
                        self.text.push_str(t.text());
                    }
                    _ => self.text.push_str(t.text()),
                },
                NodeOrToken::Node(c) => match c.kind() {
                    BLOCK if c.parent().as_ref() == Some(stmt) => {
                        let entries: Vec<SyntaxNode> =
                            c.children().filter(|e| e.kind() == ASSIGN).collect();
                        match entry.and_then(|e| entries.iter().position(|x| x == e)) {
                            Some(i) => {
                                self.text.push_str("{ ");
                                if i > 0 {
                                    self.text.push_str(".. ");
                                }
                                self.node(stmt, &entries[i], entry);
                                if i + 1 < entries.len() {
                                    self.text.push_str(" ..");
                                }
                                self.text.push_str(" }");
                            }
                            None if entries.is_empty() => self.text.push_str("{}"),
                            None => {
                                // Elided, but its terms are the statement's.
                                for e in &entries {
                                    let mut inner = Shown::default();
                                    inner.node(stmt, e, entry);
                                    self.terms.extend(inner.terms);
                                }
                                self.text.push_str("{ .. }");
                            }
                        }
                    }
                    BODY if c.first_token().is_some_and(|t| t.kind() == L_BRACE) => {
                        let lits: Vec<SyntaxNode> = c.children().collect();
                        for (i, l) in lits.iter().enumerate() {
                            if i > 0 {
                                self.text.push_str(", ");
                            }
                            self.node(stmt, l, entry);
                        }
                    }
                    _ => self.node(stmt, &c, entry),
                },
            }
        }
    }

    /// Node `c` of `stmt`: noted (a variable, a computed term), then printed.
    pub(super) fn node(&mut self, stmt: &SyntaxNode, c: &SyntaxNode, entry: Option<&SyntaxNode>) {
        use SyntaxKind::*;
        match c.kind() {
            CHAIN if !is_type_or_target(c) => {
                if let Some(name) = bare_name(c) {
                    self.vars.push(name);
                } else if !negated(c, stmt) {
                    self.terms.push(NodeOrToken::Node(c.clone()));
                }
            }
            CALL if !negated(c, stmt) => self.terms.push(NodeOrToken::Node(c.clone())),
            // `{ env }` and an entry that is only a name take the variable.
            OBJECT_FIELD | BLOCK_PATH if c.children().next().is_none() => {
                let words: Vec<_> = c
                    .children_with_tokens()
                    .filter(|t| !t.kind().is_trivia())
                    .collect();
                let shorthand = c.kind() == OBJECT_FIELD
                    || c.parent().is_some_and(|a| {
                        a.children_with_tokens()
                            .filter(|t| !t.kind().is_trivia())
                            .count()
                            == 1
                    });
                if let [NodeOrToken::Token(t)] = words.as_slice()
                    && t.kind() == IDENT
                    && shorthand
                {
                    self.vars.push(t.text().to_string());
                }
            }
            _ => {}
        }
        // A chain prints as written; only its index terms are noted, and
        // a `[_]` by its path: `k8s.deployment[_]`, then `containers[_]`.
        if c.kind() == CHAIN {
            self.text.push_str(&c.text().to_string());
            let mut segs: Vec<String> = Vec::new();
            for el in c.children_with_tokens() {
                match el {
                    NodeOrToken::Token(t) if t.kind() != DOT && !t.kind().is_trivia() => {
                        segs.push(t.text().to_string());
                    }
                    NodeOrToken::Node(ix) if ix.kind() == INDEX => {
                        let mut ts = ix.children();
                        let each = ts.next().and_then(|t| bare_name(&t)).as_deref() == Some("_")
                            && ts.next().is_none();
                        if !each {
                            if let Some(last) = segs.last_mut() {
                                last.push_str(&ix.text().to_string());
                            }
                            continue;
                        }
                        let name = match self.each.is_empty() {
                            true => segs.join("."),
                            false => segs.last().cloned().unwrap_or_default(),
                        };
                        let var = crate::syntax::resolve::each_var(ix.text_range().start().into());
                        self.each.push((format!("{name}[_]"), var));
                        segs.clear();
                    }
                    _ => {}
                }
            }
            for ix in c.children().filter(|x| x.kind() == INDEX) {
                for t in ix.children() {
                    let mut inner = Shown::default();
                    inner.node(stmt, &t, entry);
                    self.vars.extend(inner.vars);
                    self.terms.extend(inner.terms);
                }
            }
            return;
        }
        self.render(stmt, c, entry);
    }
}

/// A chain that is a type (`x in T`), a function's name, or the target of
/// a `set`: not a value of the statement.
pub(super) fn is_type_or_target(c: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    let Some(parent) = c.parent() else {
        return false;
    };
    let first = parent.children().next().as_ref() == Some(c);
    match parent.kind() {
        CALL | SET => first,
        LIT_IN | LIT_NOT_IN => !first,
        _ => false,
    }
}

/// The name of a chain that is one word: a variable (or a cell read by
/// its name).
pub(super) fn bare_name(c: &SyntaxNode) -> Option<String> {
    let mut words = c.children_with_tokens().filter(|t| !t.kind().is_trivia());
    match (words.next(), words.next()) {
        (Some(NodeOrToken::Token(t)), None) if t.kind() == SyntaxKind::IDENT => {
            Some(t.text().to_string())
        }
        _ => None,
    }
}

/// Under a `not` within `stmt`: what it names was not found, so it has no
/// value to show.
pub(super) fn negated(n: &SyntaxNode, stmt: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    n.ancestors()
        .take_while(|a| a != stmt)
        .any(|a| matches!(a.kind(), LIT_NOT | LIT_NOT_BLOCK | LIT_NOT_IN))
}

/// A refinement an aggregate's value is checked against
/// (`type_refine(T, P, C)`, `attr_refine(T, A, P, C)`): not a
/// contribution.
pub(super) fn is_check(f: &Fact) -> bool {
    matches!(
        (f.pred.as_str(), f.args.len()),
        (crate::refine::TYPE_REFINE, 3) | (crate::refine::ATTR_REFINE, 4)
    )
}

/// A string literal holds an interpolation `${..}` (`$${` is a literal
/// `${`).
pub(super) fn has_hole(text: &str) -> bool {
    pieces(text).is_some_and(|ps| ps.iter().any(|p| matches!(p, Piece::Hole(..))))
}

/// One firing's bindings and its lowered rule: what a term of the
/// statement evaluates against.
pub(super) struct Cx<'a> {
    /// The firing's bindings, borrowed: a firing over a manifest binds
    /// its documents.
    pub(super) env: std::collections::HashMap<&'a str, &'a Value>,
    pub(super) rule: Option<&'a RuleStmt>,
}

impl Cx<'_> {
    /// The address of the resource the source variable `name` ranges
    /// over (`r in T`), `T["A"]`; `None` for anything else.
    pub(super) fn address_of(&self, name: &str) -> Option<String> {
        let var = capitalise(name);
        let v = *self.env.get(var.as_str())?;
        let typ = self.want_type(&var)?;
        match (typ, v) {
            (Value::Str(t), Value::Str(n)) => Some(
                Address {
                    typ: t,
                    name: n.clone(),
                }
                .to_string(),
            ),
            _ => None,
        }
    }

    /// The type `want(T, var)` in the rule's body gives `var`.
    pub(super) fn want_type(&self, var: &str) -> Option<Value> {
        let named = |t: &Term| matches!(t, Term::Var(x) if x == var);
        self.rule.and_then(|r| {
            r.body.iter().find_map(|l| match l {
                Lit::Pos(a) if a.pred == "want" && a.args.get(1).is_some_and(named) => {
                    self.core(&a.args[0])
                }
                // A relation's reference column taken apart (R-185):
                // `workload(ref(T, W, ""))`.
                Lit::Pos(a) => a.args.iter().find_map(|t| match t {
                    Term::Func { name, args } if name == crate::ir::REF && args.len() == 3 => {
                        named(&args[1]).then(|| self.core(&args[0])).flatten()
                    }
                    _ => None,
                }),
                _ => None,
            })
        })
    }

    /// A variable's value; one that ranges over a type's resources
    /// (`r in T`) as the resource's address.
    pub(super) fn show_var(&self, var: &str, v: &Value, redact: &Redactor) -> String {
        let typ = self.want_type(var);
        match (typ, v) {
            (Some(Value::Str(t)), Value::Str(name)) if !redact.is_secret(v) => {
                crate::report::address(&Address {
                    typ: t,
                    name: name.clone(),
                })
            }
            _ => self.document_row(v).unwrap_or_else(|| redact.surface(v)),
        }
    }

    /// A value a loader's document the rule reads holds whole: its row
    /// and size (R-131), `crds.yml:412  (24.0 KB)`.
    pub(super) fn document_row(&self, v: &Value) -> Option<String> {
        if !structured(v) {
            return None;
        }
        self.found()
            .filter(|a| crate::tables::is_document(&a.pred))
            .find_map(|a| {
                let [.., at, doc] = a.args.as_slice() else {
                    return None;
                };
                let (Some(at), Some(doc)) = (self.bound(at), self.bound(doc)) else {
                    return None;
                };
                let Value::Str(at) = &*at else {
                    return None;
                };
                let at = place_in(at, &doc, v)?;
                let size = serde_json::to_vec(&engine::value_to_json(v)).map_or(0, |b| b.len());
                Some(format!("{at}  ({})", crate::query::size(size)))
            })
    }

    /// [`Cx::core`], a variable's or a literal's value borrowed.
    pub(super) fn bound<'t>(&'t self, t: &'t Term) -> Option<std::borrow::Cow<'t, Value>> {
        use std::borrow::Cow;
        match t {
            Term::Val(v) => Some(Cow::Borrowed(v)),
            Term::Var(x) => self.env.get(x.as_str()).map(|v| Cow::Borrowed(*v)),
            t => self.core(t).map(Cow::Owned),
        }
    }

    /// A lowered term's value under the bindings.
    pub(super) fn core(&self, t: &Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => self.env.get(x.as_str()).map(|v| (*v).clone()),
            Term::Func { name, args } => {
                let args = args
                    .iter()
                    .map(|a| self.core(a))
                    .collect::<Option<Vec<_>>>()?;
                call(name, &args)
            }
            Term::List(xs) => xs
                .iter()
                .map(|x| self.core(x))
                .collect::<Option<_>>()
                .map(Value::List),
            Term::Obj(m) => m
                .iter()
                .map(|(k, x)| Some((k.clone(), self.core(x)?)))
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Value::Obj),
            Term::Wildcard | Term::ListComp { .. } => None,
        }
    }

    /// The rule's positive literals: what its firing found.
    pub(super) fn found(&self) -> impl Iterator<Item = &Atom> {
        self.rule
            .into_iter()
            .flat_map(|r| &r.body)
            .filter_map(|l| match l {
                Lit::Pos(a) => Some(a),
                _ => None,
            })
    }

    pub(super) fn eval_el(&self, el: &SyntaxElement) -> Option<Value> {
        match el {
            NodeOrToken::Node(n) => self.eval(n),
            NodeOrToken::Token(t) if t.kind() == SyntaxKind::STRING => self.string(t.text()),
            NodeOrToken::Token(_) => None,
        }
    }

    /// A source term's value under the firing's bindings: literals,
    /// variables, interpolations and calls computed again; a read or a
    /// lookup is the value the firing found for it.
    pub(super) fn eval(&self, n: &SyntaxNode) -> Option<Value> {
        use SyntaxKind::*;
        match n.kind() {
            LITERAL => {
                let t = n
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .find(|t| !t.kind().is_trivia())?;
                match t.kind() {
                    INT => t.text().parse().ok().map(Value::Int),
                    // A decimal, `0.5`, is a float (R-75).
                    QUANTITY if t.text().bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
                        crate::value::Float::parse(t.text()).ok().map(Value::Float)
                    }
                    STRING => self.string(t.text()),
                    TRUE_KW => Some(Value::Bool(true)),
                    FALSE_KW => Some(Value::Bool(false)),
                    _ => None,
                }
            }
            PAREN => self.eval(&n.children().next()?),
            CHAIN => self.chain(n),
            CALL => {
                let mut kids = n.children();
                let name: String = kids.next()?.text().to_string().split_whitespace().collect();
                let args = kids.next().filter(|a| a.kind() == ARG_LIST)?;
                let args = args
                    .children()
                    .map(|a| (a.kind() != NAMED_ARG).then(|| self.eval(&a)).flatten())
                    .collect::<Option<Vec<_>>>()?;
                call(&name, &args)
            }
            // A spread's value is the lowering's (R-199), not computed again.
            LIST | OBJECT if n.children().any(|c| c.kind() == SPREAD) => None,
            LIST => n
                .children()
                .map(|x| self.eval(&x))
                .collect::<Option<_>>()
                .map(Value::List),
            OBJECT => n
                .children()
                .filter(|f| f.kind() == OBJECT_FIELD)
                .map(|f| {
                    let key = f
                        .children_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .find(|t| t.kind() == IDENT)?;
                    let v = match f.children().next() {
                        Some(v) => self.eval(&v)?,
                        None => (*self.env.get(capitalise(key.text()).as_str())?).clone(),
                    };
                    Some((key.text().to_string(), v))
                })
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Value::Obj),
            _ => None,
        }
    }

    /// A string literal's value, its holes filled.
    pub(super) fn string(&self, text: &str) -> Option<Value> {
        let mut out = String::new();
        for p in pieces(text)? {
            match p {
                Piece::Text(l) => {
                    out.push_str(&crate::syntax::resolve::unescape(&format!("\"{l}\"")).ok()?)
                }
                Piece::Hole(h, _) => {
                    let parse = crate::syntax::parser::parse_term(h);
                    let t = parse.syntax().children().next()?;
                    // A variable `r in T` binds interpolates as its
                    // address, as an untyped reference does (R-42).
                    if let Some(at) = self.address_of(h.trim()) {
                        out.push_str(&at);
                        continue;
                    }
                    match self.eval(&t)? {
                        Value::Str(s) => out.push_str(&s),
                        v if crate::stuck::has_null(&v) => return None,
                        // A reference interpolates as its address (R-42).
                        Value::Ref { typ, name, attr } => {
                            out.push_str(&crate::ir::Address { typ, name }.attr(&attr))
                        }
                        v => out.push_str(&spell::value(&v)),
                    }
                }
            }
        }
        Some(Value::Str(out))
    }

    /// A chain: a variable, a read (`x.p`, `cfg.db.size`, `T[k].p`), or a
    /// lookup (`T[k]`, a relation's `rel[k]`).
    pub(super) fn chain(&self, c: &SyntaxNode) -> Option<Value> {
        enum Seg {
            Name(String),
            Index(Vec<SyntaxNode>),
        }
        let mut segs = Vec::new();
        for el in c.children_with_tokens() {
            match el {
                NodeOrToken::Node(ix) if ix.kind() == SyntaxKind::INDEX => {
                    segs.push(Seg::Index(ix.children().collect()))
                }
                NodeOrToken::Token(t)
                    if !t.kind().is_trivia()
                        && !matches!(t.kind(), SyntaxKind::DOT | SyntaxKind::STRING) =>
                {
                    segs.push(Seg::Name(t.text().to_string()))
                }
                NodeOrToken::Token(t) if t.kind() == SyntaxKind::STRING => {
                    segs.push(Seg::Name(self.string(t.text())?.as_str()?.to_string()))
                }
                _ => {}
            }
        }
        let at = segs.iter().position(|s| matches!(s, Seg::Index(_)));
        let names = |s: &[Seg]| -> Option<Vec<String>> {
            s.iter()
                .map(|x| match x {
                    Seg::Name(n) => Some(n.clone()),
                    Seg::Index(_) => None,
                })
                .collect()
        };
        let Some(at) = at else {
            let names = names(&segs)?;
            let head = names.first()?;
            if names.len() == 1 {
                return self
                    .env
                    .get(capitalise(head).as_str())
                    .map(|v| (*v).clone());
            }
            // A variable bound to an object: its field.
            if let Some(mut v) = self
                .env
                .get(capitalise(head).as_str())
                .copied()
                .filter(|v| matches!(v, Value::Obj(_)))
            {
                for k in &names[1..] {
                    let Value::Obj(m) = v else { return None };
                    v = m.get(k)?;
                }
                return Some(v.clone());
            }
            for i in 1..names.len() {
                let owner = names[..i].join(".");
                let path = names[i..].join(".");
                if let Some(v) = self.read(&path, |a| self.owns(a, &owner)) {
                    return Some(v);
                }
            }
            return None;
        };
        let base = names(&segs[..at])?.join(".");
        let Seg::Index(keys) = &segs[at] else {
            return None;
        };
        let keys = keys
            .iter()
            .map(|k| self.eval(k))
            .collect::<Option<Vec<_>>>()?;
        let rest = names(&segs[at + 1..])?;
        if rest.is_empty() {
            // A relation's row (`zone_index[z]`): its last column.
            let row = self.found().find(|a| {
                (a.pred == base || a.pred.ends_with(&format!("::{base}")))
                    && a.args.len() == keys.len() + 1
                    && a.args
                        .iter()
                        .zip(&keys)
                        .all(|(t, k)| self.core(t).as_ref() == Some(k))
            });
            if let Some(a) = row {
                return self.core(a.args.last()?);
            }
            let [Value::Str(name)] = keys.as_slice() else {
                return None;
            };
            return Some(Value::Ref {
                typ: base,
                name: name.clone(),
                attr: String::new(),
            });
        }
        let [key] = keys.as_slice() else { return None };
        let instance = key.as_str().map(|k| format!("{base}.{k}"));
        let path = rest.join(".");
        let read = self.read(&path, |a| match self.core(a) {
            Some(v) if v == *key => true,
            Some(Value::Str(s)) => Some(&s) == instance.as_ref(),
            _ => false,
        });
        // Not read by the firing: a reference to the attribute, passed on.
        read.or_else(|| {
            Some(Value::Ref {
                typ: base,
                name: key.as_str()?.to_string(),
                attr: path,
            })
        })
    }

    /// The value the firing read at `path` of an owner `owns` accepts.
    pub(super) fn read(&self, path: &str, owns: impl Fn(&Term) -> bool) -> Option<Value> {
        self.found().find_map(|a| match a.args.as_slice() {
            [_, owner, p, v]
                if a.pred == "attr"
                    && self.core(p).as_ref().and_then(Value::as_str) == Some(path)
                    && owns(owner) =>
            {
                self.core(v)
            }
            _ => None,
        })
    }

    /// The lowered owner `a` is what the source calls `name`: its variable,
    /// or the resource, module instance or cell of that name.
    pub(super) fn owns(&self, a: &Term, name: &str) -> bool {
        if matches!(a, Term::Var(x) if *x == capitalise(name)) {
            return true;
        }
        match self.core(a) {
            Some(Value::Str(s)) => s == name || s.ends_with(&crate::ir::scoped("", name)),
            _ => false,
        }
    }
}

/// A function's value at `args`, as the engine computes it; none over a
/// null.
pub(super) fn call(name: &str, args: &[Value]) -> Option<Value> {
    if args.iter().any(crate::stuck::has_null) {
        return None;
    }
    crate::functions::body(name)?(args)
}
