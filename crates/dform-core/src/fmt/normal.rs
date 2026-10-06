//! The normal forms `dform fmt` prints where two spellings of one meaning
//! survive (proposal H section 3): `{ a }` for `{ a: a }`, a header name quoted
//! only when it needs it, `not lit` for `not { lit }`, `==` where both
//! sides are bound, the atom `p(k, i)` for `i = p[k]` with `i` fresh,
//! `env == "prod"` for a value name's atom `env("prod")`, no `{}` on a
//! `use` or `instance` with no entries, and `k` for the entry `k = k`.
//!
//! Each is an edit of the source text, read from the tree; the caller
//! parses the result again and prints it, until nothing changes.

use crate::syntax::SyntaxKind::*;
use crate::syntax::{SyntaxNode, SyntaxToken, tokens};
use std::collections::BTreeSet;

fn is_name(s: &str) -> bool {
    crate::lexer::is_word(s) && crate::lexer::keyword(s).is_none()
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
        // A component signature's inputs (R-104) are no scope's names.
        for d in root
            .descendants()
            .filter(|d| !d.ancestors().any(|a| a.kind() == SIGNATURE))
        {
            let first_word = || tokens(&d).filter(|t| t.kind().is_word()).nth(1);
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
                        tokens(&d).filter(|t| t.kind() != RANK).skip(1).collect();
                    if let Some((name, typ)) = ts.split_last() {
                        // A name the clause binds is the clause's variable.
                        let bound = d
                            .children()
                            .filter(|c| c.kind() == CLAUSE)
                            .flat_map(|c| c.descendants_with_tokens())
                            .filter_map(|e| e.into_token())
                            .any(|t| t.text() == name.text());
                        if name.kind().is_word() && !bound {
                            n.declared.insert(name.text().to_string());
                        }
                        if let Some(first) = typ.first() {
                            n.declared.insert(first.text().to_string());
                        }
                    }
                }
                // A component's name, the first segment of an instance's
                // path, and the name a `use` binds.
                COMPONENT | INSTANCE => {
                    if let Some(t) = first_word() {
                        n.declared.insert(t.text().to_string());
                    }
                }
                USE => {
                    n.declared.insert(crate::syntax::resolve::use_parts(&d).1);
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
            if let Some(t) = tokens(n).next() {
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
            } else if let Some(k) = tokens(n).next() {
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

struct Ctx {
    names: Names,
    edits: Vec<(usize, usize, String)>,
}

impl Ctx {
    fn text(&self, n: &SyntaxNode) -> String {
        n.text().to_string().trim().to_string()
    }

    fn put(&mut self, n: &SyntaxNode, s: String) {
        let r = n.text_range();
        self.edits.push((r.start().into(), r.end().into(), s));
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
    }

    fn lit(&mut self, l: &SyntaxNode, bound: &BTreeSet<String>) {
        match l.kind() {
            LIT_CMP => {
                let ops: Vec<SyntaxToken> = tokens(l).collect();
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
                // `=` binds and `==` compares; fmt never trades one for the
                // other. A `=` with both sides bound is the resolver's
                // error (R-10), not a spelling fmt corrects.
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
                } else if a.kind() == CHAIN && tokens(a).count() == 1 {
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
        let vt: Vec<SyntaxToken> = tokens(v).collect();
        let [var] = vt.as_slice() else { return None };
        let var = var.text();
        if bound.contains(var) || self.names.known(var) || var == "_" {
            return None;
        }
        let ct: Vec<SyntaxToken> = tokens(c).collect();
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

    fn stmt(&mut self, n: &SyntaxNode) {
        match n.kind() {
            RULE | LET | SET | OUTPUT_DECL | CHECK => {
                if let Some(b) = n.children().find(|c| c.kind() == BODY) {
                    self.body(&b, &BTreeSet::new());
                }
            }
            RESOURCE | INSTANCE | USE => {
                if let Some(c) = n.children().find(|c| c.kind() == CLAUSE)
                    && let Some(b) = c.children().find(|x| x.kind() == BODY)
                {
                    self.body(&b, &BTreeSet::new());
                }
                if !matches!(n.kind(), INSTANCE | USE) {
                    self.header(n);
                }
            }
            COMPONENT => {
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
        let Some(name) = tokens(n).filter(|t| t.kind() != RANK).last() else {
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
    /// A `use` or `instance` block with no entries is left out (R-26):
    /// `use aws {}` is `use aws`.
    fn empty_blocks(&mut self, root: &SyntaxNode) {
        for b in root.descendants().filter(|n| n.kind() == BLOCK) {
            if !b
                .parent()
                .is_some_and(|p| matches!(p.kind(), INSTANCE | USE))
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

    /// A block entry whose value is its path's last segment is the pun
    /// (R-33): `zone = zone` is `zone`, `spec.selector.color = color` is
    /// `spec.selector.color`. A provider's `source` is a constant, never a
    /// pun.
    fn entry_puns(&mut self, root: &SyntaxNode) {
        for a in root.descendants().filter(|n| n.kind() == ASSIGN) {
            let (Some(path), Some(value)) = (
                a.children().find(|c| c.kind() == BLOCK_PATH),
                a.children().find(|c| c.kind() == CHAIN),
            ) else {
                continue;
            };
            let Some(seg) = tokens(&path).last() else {
                continue;
            };
            let named = seg.kind().is_word()
                && !matches!(
                    seg.kind(),
                    NOT_KW | IN_KW | HAS_KW | WHERE_KW | IF_KW | TRUE_KW | FALSE_KW
                );
            let assign = tokens(&a).any(|t| t.kind() == EQ);
            let source = path.text() == "source"
                && a.parent()
                    .and_then(|b| b.parent())
                    .is_some_and(|s| crate::syntax::resolve::maybe_provider_use(&s).is_some());
            if named && assign && !source && self.text(&value) == seg.text() {
                let (start, end) = (path.text_range().end(), value.text_range().end());
                self.edits.push((start.into(), end.into(), String::new()));
            }
        }
    }

    /// A resource block's leaves under one parent path are one entry
    /// (R-52, amended): `metadata.name = "a"` and `metadata.namespace = n`
    /// are `metadata = { name: "a", namespace: n }` when the parent's
    /// entries are two or more, each a leaf (a value that is not an
    /// object) written with `=` at one rank, and nothing sets a path below
    /// them; in source order, at the first one's place. A parent with an
    /// object under it, or one leaf, stays dotted.
    fn entry_folds(&mut self, root: &SyntaxNode) {
        for b in root.descendants().filter(|n| n.kind() == BLOCK) {
            if b.parent().is_none_or(|p| p.kind() != RESOURCE)
                || b.descendants_with_tokens().any(|e| e.kind() == COMMENT)
            {
                continue;
            }
            let entries: Vec<Entry> = b.children().filter_map(|a| Entry::of(&a)).collect();
            let all = b.children().filter(|c| c.kind() == ASSIGN).count();
            if entries.len() != all {
                continue;
            }
            let parents: BTreeSet<&[String]> = entries
                .iter()
                .filter(|e| e.path.len() > 1)
                .map(|e| &e.path[..e.path.len() - 1])
                .collect();
            for parent in parents {
                let under = |e: &&Entry| e.path.len() > parent.len() && e.path.starts_with(parent);
                let kids: Vec<&Entry> = entries.iter().filter(under).collect();
                let leaves = kids
                    .iter()
                    .all(|e| e.path.len() == parent.len() + 1 && e.leaf);
                let ranks: BTreeSet<&Option<String>> = kids.iter().map(|e| &e.rank).collect();
                let whole = entries.iter().any(|e| parent.starts_with(&e.path));
                if kids.len() < 2 || !leaves || ranks.len() != 1 || whole {
                    continue;
                }
                let fields: Vec<String> = kids
                    .iter()
                    .map(|e| {
                        let k = e.path.last().map_or("", String::as_str);
                        match &e.value {
                            Some(v) if v != k || !is_name(k) => format!("{}: {v}", key_text(k)),
                            _ => k.to_string(),
                        }
                    })
                    .collect();
                let rank = kids[0]
                    .rank
                    .as_ref()
                    .map_or(String::new(), |r| format!(" {r}"));
                let path: Vec<String> = parent.iter().map(|s| key_text(s)).collect();
                let folded = format!("{} = {{ {} }}{rank}", path.join("."), fields.join(", "));
                self.put(&kids[0].node, folded);
                for e in &kids[1..] {
                    // From the end of the entry before it: its separator too.
                    let start = e
                        .node
                        .prev_sibling()
                        .map_or(e.node.text_range().start(), |p| p.text_range().end());
                    self.edits.push((
                        start.into(),
                        e.node.text_range().end().into(),
                        String::new(),
                    ));
                }
            }
        }
    }

    fn objects(&mut self, root: &SyntaxNode) {
        for f in root.descendants().filter(|n| n.kind() == OBJECT_FIELD) {
            if f.parent().is_some_and(|p| p.kind() != OBJECT) {
                continue;
            }
            let Some(key) = tokens(&f).next() else {
                continue;
            };
            let Some(value) = f.children().next() else {
                continue;
            };
            if value.kind() == CHAIN && key.kind().is_word() && self.text(&value) == key.text() {
                self.put(&f, key.text().to_string());
            }
        }
    }
}

/// A resource block's entry, for [`Ctx::entry_folds`]: `path = value
/// [rank]`, or the pun `path [rank]` (`value` none).
struct Entry {
    node: SyntaxNode,
    path: Vec<String>,
    value: Option<String>,
    /// Not an object.
    leaf: bool,
    rank: Option<String>,
}

impl Entry {
    /// A block's `=` entry with a path of names and strings; `None` for
    /// `+=` or a path with an index.
    fn of(a: &SyntaxNode) -> Option<Entry> {
        let path = a.children().find(|c| c.kind() == BLOCK_PATH)?;
        let segs: Vec<SyntaxToken> = tokens(&path).filter(|t| t.kind() != DOT).collect();
        if segs
            .iter()
            .any(|t| !t.kind().is_word() && t.kind() != STRING)
        {
            return None;
        }
        if tokens(a).any(|t| t.kind() == PLUS_EQ) {
            return None;
        }
        let value = a.children().find(|c| c.kind() != BLOCK_PATH);
        Some(Entry {
            node: a.clone(),
            path: segs
                .iter()
                .map(|t| t.text().trim_matches('"').to_string())
                .collect(),
            leaf: value.as_ref().is_none_or(|v| v.kind() != OBJECT),
            value: value.map(|v| v.text().to_string().trim().to_string()),
            rank: tokens(a)
                .find(|t| t.kind() == RANK)
                .map(|t| t.text().to_string()),
        })
    }
}

/// An object key or a path segment as written: bare when it is a name,
/// else quoted.
fn key_text(k: &str) -> String {
    if crate::lexer::is_word(k) {
        k.to_string()
    } else {
        format!("{k:?}")
    }
}

/// The source with its normal forms, or `None` when it is in them.
pub fn normalize(root: &SyntaxNode, src: &str) -> Option<String> {
    let mut c = Ctx {
        names: Names::of(root),
        edits: Vec::new(),
    };
    for s in root.children() {
        c.stmt(&s);
    }
    c.objects(root);
    c.empty_blocks(root);
    c.entry_puns(root);
    c.entry_folds(root);
    apply(src, c.edits)
}

/// `src` with `edits` (byte ranges and their text) made, the outer of two
/// that overlap winning; `None` when that changes nothing.
pub(super) fn apply(src: &str, mut edits: Vec<(usize, usize, String)>) -> Option<String> {
    edits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out = String::new();
    let mut at = 0;
    for (s, e, t) in edits {
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
