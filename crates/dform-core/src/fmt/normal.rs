//! The normal forms `dform fmt` prints where two spellings of one meaning
//! survive (proposal H section 3): a body on one line when it fits and in
//! braces when it does not, `{ a }` for `{ a: a }`, a header name quoted
//! only when it needs it, `not lit` for `not { lit }`, `==` where both
//! sides are bound, the atom `p(k, i)` for `i = p[k]` with `i` fresh,
//! `env == "prod"` for a value name's atom `env("prod")`, and no `{}` on a
//! `provider` or `instance` with no entries.
//!
//! Each is an edit of the source text, read from the tree; the caller
//! parses the result again and prints it, until nothing changes.

use crate::syntax::SyntaxKind::{self, *};
use crate::syntax::{SyntaxNode, SyntaxToken};
use std::collections::BTreeSet;

/// The widest a line `fmt` joins a body onto.
pub const WIDTH: usize = 100;

fn toks(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}

fn is_word(k: SyntaxKind) -> bool {
    k == IDENT || k.is_keyword()
}

fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && crate::lexer::keyword(s).is_none()
}

/// What the file declares by name: value names (inputs and `let`s),
/// relations its heads define, resources, modules; every name that is not
/// a variable where it stands.
#[derive(Default)]
struct Names {
    values: BTreeSet<String>,
    relations: BTreeSet<String>,
    declared: BTreeSet<String>,
}

impl Names {
    fn of(root: &SyntaxNode) -> Names {
        let mut n = Names::default();
        n.declared
            .extend(["settings", "world", "true", "false", "_"].map(String::from));
        for d in root.descendants() {
            let first_word = || toks(&d).filter(|t| is_word(t.kind())).nth(1);
            match d.kind() {
                INPUT | LET => {
                    if let Some(t) = first_word() {
                        n.values.insert(t.text().to_string());
                    }
                }
                RULE | FACT => {
                    if let Some(c) = d.children().find(|c| c.kind() == CALL)
                        && let Some(ch) = c.children().find(|c| c.kind() == CHAIN)
                    {
                        n.relations.insert(ch.text().to_string().trim().to_string());
                    }
                }
                RESOURCE => {
                    let ts: Vec<SyntaxToken> =
                        toks(&d).filter(|t| t.kind() != RANK).skip(1).collect();
                    if let Some((name, typ)) = ts.split_last() {
                        // A name the clause binds is the clause's variable.
                        let bound = d
                            .children()
                            .filter(|c| c.kind() == CLAUSE)
                            .flat_map(|c| c.descendants_with_tokens())
                            .filter_map(|e| e.into_token())
                            .any(|t| t.text() == name.text());
                        if is_word(name.kind()) && !bound {
                            n.declared.insert(name.text().to_string());
                        }
                        if let Some(first) = typ.first() {
                            n.declared.insert(first.text().to_string());
                        }
                    }
                }
                MODULE | INSTANCE => {
                    if let Some(t) = first_word() {
                        n.declared.insert(t.text().to_string());
                    }
                }
                _ => {}
            }
        }
        n
    }

    fn known(&self, name: &str) -> bool {
        self.values.contains(name) || self.declared.contains(name)
    }
}

/// The names a term uses as the heads of its chains (a variable, a value
/// name, a resource, a root): the call names aside.
fn heads(n: &SyntaxNode, out: &mut Vec<String>) {
    match n.kind() {
        CHAIN => {
            if let Some(t) = toks(n).next() {
                out.push(t.text().to_string());
            }
            for ix in n.children().filter(|c| c.kind() == INDEX) {
                for c in ix.children() {
                    heads(&c, out);
                }
            }
        }
        CALL => {
            for l in n.children().filter(|c| c.kind() == ARG_LIST) {
                for c in l.children() {
                    heads(&c, out);
                }
            }
        }
        OBJECT_FIELD => {
            let has_value = n.children().next().is_some();
            if has_value {
                for c in n.children() {
                    heads(&c, out);
                }
            } else if let Some(k) = toks(n).next() {
                out.push(k.text().to_string());
            }
        }
        // A comprehension binds its own variables.
        COMPREHENSION => {}
        _ => {
            for c in n.children() {
                heads(&c, out);
            }
        }
    }
}

/// The names a literal binds for the literals after it.
fn binds(l: &SyntaxNode, out: &mut BTreeSet<String>) {
    let mut hs = Vec::new();
    match l.kind() {
        LIT_ATOM | LIT_CMP | LIT_IN => {
            for c in l.children() {
                heads(&c, &mut hs);
            }
        }
        _ => {}
    }
    out.extend(hs);
}

struct Ctx<'a> {
    src: &'a str,
    names: Names,
    edits: Vec<(usize, usize, String)>,
}

impl Ctx<'_> {
    fn text(&self, n: &SyntaxNode) -> String {
        n.text().to_string().trim().to_string()
    }

    fn put(&mut self, n: &SyntaxNode, s: String) {
        let r = n.text_range();
        self.edits.push((r.start().into(), r.end().into(), s));
    }

    /// The column `at` is at in the source, and its line's text.
    fn line(&self, at: usize) -> (usize, &str) {
        let start = self.src[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = self.src[at..].find('\n').map_or(self.src.len(), |i| at + i);
        (at - start, &self.src[start..end])
    }

    fn bound(&self, n: &SyntaxNode, bound: &BTreeSet<String>) -> bool {
        let mut hs = Vec::new();
        heads(n, &mut hs);
        hs.iter()
            .all(|h| bound.contains(h) || self.names.known(h) || h.contains('.'))
    }

    /// One body: its literals in order, what each binds, and the normal
    /// forms of its literals.
    fn body(&mut self, body: &SyntaxNode, bound0: &BTreeSet<String>) {
        let mut bound = bound0.clone();
        for l in body.children() {
            self.lit(&l, &bound);
            binds(&l, &mut bound);
        }
        self.width(body);
    }

    fn lit(&mut self, l: &SyntaxNode, bound: &BTreeSet<String>) {
        match l.kind() {
            LIT_CMP => {
                let ops: Vec<SyntaxToken> = toks(l).collect();
                let sides: Vec<SyntaxNode> = l.children().collect();
                if ops.len() != 1 || sides.len() != 2 {
                    return;
                }
                let op = &ops[0];
                // `i = p[k]` with `i` fresh: the atom.
                if op.kind() == EQ {
                    for (v, c) in [(&sides[0], &sides[1]), (&sides[1], &sides[0])] {
                        if let Some(atom) = self.lookup_atom(v, c, bound) {
                            self.put(l, atom);
                            return;
                        }
                    }
                }
                // `=` where both sides are bound tests: `==`.
                if op.kind() == EQ && self.bound(&sides[0], bound) && self.bound(&sides[1], bound) {
                    let r = op.text_range();
                    self.edits
                        .push((r.start().into(), r.end().into(), "==".to_string()));
                }
            }
            LIT_ATOM => {
                // `env("prod")` for a value name: the comparison.
                let Some(call) = l.children().find(|c| c.kind() == CALL) else {
                    return;
                };
                let Some(chain) = call.children().find(|c| c.kind() == CHAIN) else {
                    return;
                };
                let name = self.text(&chain);
                if !self.names.values.contains(&name) || self.names.relations.contains(&name) {
                    return;
                }
                let Some(args) = call.children().find(|c| c.kind() == ARG_LIST) else {
                    return;
                };
                let args: Vec<SyntaxNode> = args.children().collect();
                let [a] = args.as_slice() else { return };
                if a.kind() == NAMED_ARG {
                    return;
                }
                let at = self.text(a);
                if self.bound(a, bound) {
                    self.put(l, format!("{name} == {at}"));
                } else if a.kind() == CHAIN && toks(a).count() == 1 {
                    self.put(l, format!("{at} = {name}"));
                }
            }
            LIT_NOT_BLOCK => {
                // `not { lit }`: `not lit`, when the literal introduces no
                // name of its own.
                let Some(body) = l.children().find(|c| c.kind() == BODY) else {
                    return;
                };
                let lits: Vec<SyntaxNode> = body.children().collect();
                let [one] = lits.as_slice() else { return };
                if !matches!(
                    one.kind(),
                    LIT_ATOM | LIT_TRUTH | LIT_CMP | LIT_IN | LIT_HAS
                ) {
                    return;
                }
                if !self.bound(one, bound)
                    || body.descendants_with_tokens().any(|e| e.kind() == COMMENT)
                {
                    return;
                }
                self.put(l, format!("not {}", self.text(one)));
            }
            _ => {}
        }
    }

    /// `v = p[k]` with `v` a fresh name and `p` a relation of the file:
    /// `p(k, v)`.
    fn lookup_atom(
        &self,
        v: &SyntaxNode,
        c: &SyntaxNode,
        bound: &BTreeSet<String>,
    ) -> Option<String> {
        if v.kind() != CHAIN || c.kind() != CHAIN {
            return None;
        }
        let vt: Vec<SyntaxToken> = toks(v).collect();
        let [var] = vt.as_slice() else { return None };
        let var = var.text();
        if bound.contains(var) || self.names.known(var) || var == "_" {
            return None;
        }
        let ct: Vec<SyntaxToken> = toks(c).collect();
        let [p] = ct.as_slice() else { return None };
        let ix: Vec<SyntaxNode> = c.children().filter(|x| x.kind() == INDEX).collect();
        let [ix] = ix.as_slice() else { return None };
        let p = p.text();
        if !self.names.relations.contains(p) || self.names.values.contains(p) {
            return None;
        }
        let args: Vec<String> = ix.children().map(|a| self.text(&a)).collect();
        Some(format!("{p}({}, {var})", args.join(", ")))
    }

    /// A body on one line when it fits, in braces when it does not.
    fn width(&mut self, body: &SyntaxNode) {
        let lits: Vec<SyntaxNode> = body.children().collect();
        let braced = toks(body).next().is_some_and(|t| t.kind() == L_BRACE);
        if lits.is_empty() || body.descendants_with_tokens().any(|e| e.kind() == COMMENT) {
            return;
        }
        let texts: Vec<String> = lits.iter().map(|l| self.text(l)).collect();
        if texts.iter().any(|t| t.contains('\n')) {
            return;
        }
        let r = body.text_range();
        let (start, end): (usize, usize) = (r.start().into(), r.end().into());
        let (col, line) = self.line(start);
        let one = texts.join(", ");
        let rest = self.src[end..].split('\n').next().unwrap_or("").trim_end();
        if braced {
            // The line up to the body, the literals, and what follows the
            // closing brace.
            let fits = col + one.len() + rest.len() <= WIDTH;
            if fits && !self.src[start..end].contains("\n\n") {
                self.edits.push((start, end, one));
            }
        } else if line.trim_end().len() > WIDTH && lits.len() > 1 {
            self.edits
                .push((start, end, format!("{{\n{}\n}}", texts.join("\n"))));
        }
    }

    fn stmt(&mut self, n: &SyntaxNode) {
        match n.kind() {
            RULE | LET | SET | OUTPUT_DECL | CHECK => {
                if let Some(b) = n.children().find(|c| c.kind() == BODY) {
                    self.body(&b, &BTreeSet::new());
                }
            }
            RESOURCE | SETTINGS | INSTANCE => {
                if let Some(c) = n.children().find(|c| c.kind() == CLAUSE)
                    && let Some(b) = c.children().find(|x| x.kind() == BODY)
                {
                    self.body(&b, &BTreeSet::new());
                }
                if n.kind() != INSTANCE {
                    self.header(n);
                }
            }
            MODULE | POLICY | SCENARIO => {
                if let Some(b) = n.children().find(|c| c.kind() == STMT_BLOCK) {
                    for s in b.children() {
                        self.stmt(&s);
                    }
                }
            }
            _ => {}
        }
    }

    /// A header name is quoted only when it needs it: not a name, a
    /// keyword, a hole, or a name the clause binds.
    fn header(&mut self, n: &SyntaxNode) {
        let Some(name) = toks(n).filter(|t| t.kind() != RANK).last() else {
            return;
        };
        if name.kind() != STRING {
            return;
        }
        let text = name.text();
        let inner = &text[1..text.len() - 1];
        if !is_name(inner) {
            return;
        }
        let bound = n
            .children()
            .filter(|c| c.kind() == CLAUSE)
            .flat_map(|c| c.descendants_with_tokens())
            .filter_map(|e| e.into_token())
            .any(|t| t.text() == inner);
        if !bound {
            let r = name.text_range();
            self.edits
                .push((r.start().into(), r.end().into(), inner.to_string()));
        }
    }

    /// `{ a: a }` as `{ a }`.
    /// A `provider` or `instance` block with no entries is left out
    /// (R-26): `provider aws {}` is `provider aws`.
    fn empty_blocks(&mut self, root: &SyntaxNode) {
        for b in root.descendants().filter(|n| n.kind() == BLOCK) {
            if !b
                .parent()
                .is_some_and(|p| matches!(p.kind(), PROVIDER | INSTANCE))
            {
                continue;
            }
            let empty = b
                .children_with_tokens()
                .all(|e| matches!(e.kind(), L_BRACE | R_BRACE | WHITESPACE));
            if !empty {
                continue;
            }
            let start = match b.prev_sibling_or_token() {
                Some(w) if w.kind() == WHITESPACE => w.text_range().start(),
                _ => b.text_range().start(),
            };
            self.edits
                .push((start.into(), b.text_range().end().into(), String::new()));
        }
    }

    fn objects(&mut self, root: &SyntaxNode) {
        for f in root.descendants().filter(|n| n.kind() == OBJECT_FIELD) {
            if f.parent().is_some_and(|p| p.kind() != OBJECT) {
                continue;
            }
            let Some(key) = toks(&f).next() else { continue };
            let Some(value) = f.children().next() else {
                continue;
            };
            if value.kind() == CHAIN && is_word(key.kind()) && self.text(&value) == key.text() {
                self.put(&f, key.text().to_string());
            }
        }
    }
}

/// The source with its normal forms, or `None` when it is in them.
pub fn normalize(root: &SyntaxNode, src: &str) -> Option<String> {
    let mut c = Ctx {
        src,
        names: Names::of(root),
        edits: Vec::new(),
    };
    for s in root.children() {
        c.stmt(&s);
    }
    c.objects(root);
    c.empty_blocks(root);
    if c.edits.is_empty() {
        return None;
    }
    // Outer edits win.
    c.edits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out = String::new();
    let mut at = 0;
    for (s, e, t) in c.edits {
        if s < at {
            continue;
        }
        out.push_str(&src[at..s]);
        out.push_str(&t);
        at = e;
    }
    out.push_str(&src[at..]);
    (out != src).then_some(out)
}
