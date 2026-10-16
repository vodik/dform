//! A file's layout (R-52): its tree as a [`Doc`] of groups, printed in
//! [`WIDTH`] columns. What fits on its line is on one line, however the
//! author broke it; what does not breaks from the outside in, its
//! outermost bracket first, one element per line. The author's line
//! breaks are not kept; their blank lines between entries and elements
//! are, and so are their comments, where they were written; between
//! statements a blank line is a rule ([`blank_between`]).
//!
//! The groups:
//! - a list, an object, an argument list, a tuple, an object type and a
//!   declaration's columns: one element per line when broken, each with a
//!   trailing comma; a list or an argument list whose only element is a
//!   list or an object hugs it, `[{` .. `}]`, `f({` .. `})`;
//! - a comprehension: `[ item |`, a literal per line with a trailing
//!   comma, `]`;
//! - a block (a resource's, a `set`'s, a type's, an object input's
//!   fields), and a `not { }` body: one entry per line when broken, and no
//!   commas then, the newline separates;
//! - a `where` clause: `where a, b` on the statement's line; else on a
//!   line of its own one step deeper; else `where` ends the statement's
//!   line and a literal per line follows one step deeper, each with a
//!   trailing comma, the last too: an insertion, a deletion or a reorder
//!   is a one-line diff. A literal per line whatever the width when the
//!   body has more than [`INLINE_LITERALS`] literals (R-10), a comment or
//!   an empty line.
//!
//! A chain, an operator and a string have no break in them: a line that
//! is too long after every group broke stays too long. A comment forces
//! the groups around it to break; a string of several lines does too.

use super::doc::{
    Doc, clause, concat, group, hard, has_break, if_break, indent, line, nil, stmt_group, text,
};
use crate::syntax::SyntaxKind::{self, *};
use crate::syntax::{SyntaxElement, SyntaxNode, SyntaxToken};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// The width `fmt` prints in.
pub const WIDTH: usize = 100;

/// A `where` body of more literals than this is a literal per line,
/// whatever the width (R-10).
pub const INLINE_LITERALS: usize = 3;

/// A comment on its own line, and whether an empty line is above it.
struct Cmt {
    text: String,
    blank: bool,
}

/// Where each comment goes: one on its own line belongs to the token after
/// it, one after code to the token before it (a comma aside: a comma the
/// layout prints itself).
#[derive(Default)]
struct Comments {
    leading: HashMap<u32, Vec<Cmt>>,
    trailing: HashMap<u32, Vec<String>>,
    /// Tokens with an empty line above them (and below their comments).
    blank: HashSet<u32>,
    /// Comments after the last token.
    eof: Vec<Cmt>,
}

fn key(t: &SyntaxToken) -> u32 {
    t.text_range().start().into()
}

impl Comments {
    fn of(root: &SyntaxNode) -> Comments {
        let mut c = Comments::default();
        let mut newlines = 0usize;
        let mut prev: Option<SyntaxToken> = None;
        let mut pending: Vec<Cmt> = Vec::new();
        for t in root
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
        {
            match t.kind() {
                WHITESPACE => newlines += t.text().matches('\n').count(),
                COMMENT => {
                    let text = t.text().trim_end().to_string();
                    match &prev {
                        Some(p) if newlines == 0 && pending.is_empty() => {
                            c.trailing.entry(key(p)).or_default().push(text)
                        }
                        _ => pending.push(Cmt {
                            text,
                            blank: newlines > 1,
                        }),
                    }
                    newlines = 0;
                }
                COMMA => newlines = 0,
                _ => {
                    if newlines > 1 {
                        c.blank.insert(key(&t));
                    }
                    if !pending.is_empty() {
                        c.leading.insert(key(&t), std::mem::take(&mut pending));
                    }
                    prev = Some(t.clone());
                    newlines = 0;
                }
            }
        }
        c.eof = pending;
        c
    }
}

/// An element of a sequence: its doc, with the comments above it and after
/// it.
struct Item {
    leading: Vec<Cmt>,
    /// An empty line between its comments and it.
    blank: bool,
    doc: Doc,
    trailing: Vec<String>,
}

impl Item {
    /// An empty line above the item (above its comments).
    fn blank_above(&self) -> bool {
        self.leading.first().map_or(self.blank, |c| c.blank)
    }

    /// Its comments, the item, `comma`, its trailing comments: from the
    /// start of its line.
    fn docs(self, comma: Doc) -> Vec<Doc> {
        let mut v = Vec::new();
        for (j, c) in self.leading.iter().enumerate() {
            v.push(text(c.text.clone()));
            let blank = match self.leading.get(j + 1) {
                Some(next) => next.blank,
                None => self.blank,
            };
            v.push(hard(blank));
        }
        v.push(self.doc);
        v.push(comma);
        for t in self.trailing {
            v.push(Doc::Suffix(format!(" {t}")));
            v.push(Doc::BreakParent);
        }
        v
    }
}

/// How a broken sequence is punctuated.
#[derive(Clone, Copy, PartialEq)]
enum Commas {
    /// After every element, the last too: lists, objects, arguments.
    Trailing,
    /// Between elements only: where the grammar takes no trailing comma.
    Between,
    /// None: the newline separates (blocks, bodies). On one line, `, `.
    Newline,
}

/// How a sequence prints: `pad` inside its brackets on one line, its
/// commas, whether it must break, and whether it is a statement's group
/// (a block, a body) rather than a term's.
#[derive(Clone, Copy)]
struct Shape {
    pad: &'static str,
    commas: Commas,
    force: bool,
    stmt: bool,
}

impl Shape {
    /// A term's brackets: a list's, an argument list's (`pad` ""), an
    /// object's (" ").
    fn term(pad: &'static str, commas: Commas) -> Shape {
        Shape {
            pad,
            commas,
            force: false,
            stmt: false,
        }
    }

    /// A statement's block or body: entries separated by newlines.
    fn stmt(pad: &'static str) -> Shape {
        Shape {
            pad,
            commas: Commas::Newline,
            force: false,
            stmt: true,
        }
    }

    fn force(self, force: bool) -> Shape {
        Shape {
            force: self.force || force,
            ..self
        }
    }
}

struct Layout {
    c: Comments,
}

fn first_token(n: &SyntaxNode) -> Option<SyntaxToken> {
    n.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| !t.kind().is_trivia())
}

fn last_token(n: &SyntaxNode) -> Option<SyntaxToken> {
    n.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .last()
}

fn significant(n: &SyntaxNode) -> Vec<SyntaxElement> {
    n.children_with_tokens()
        .filter(|e| !e.kind().is_trivia())
        .collect()
}

fn parent_kind(t: &SyntaxToken) -> Option<SyntaxKind> {
    t.parent().map(|p| p.kind())
}

/// An object's `key: value`, the spelling `=` replaced: printed `=`.
fn object_colon(t: &SyntaxToken) -> bool {
    t.kind() == COLON
        && t.parent().is_some_and(|f| {
            f.kind() == OBJECT_FIELD && f.parent().is_some_and(|o| o.kind() == OBJECT)
        })
}

/// A token as `fmt` writes it: an object's `:` is `=`, in a string's
/// holes too.
fn written(t: &SyntaxToken) -> Cow<'_, str> {
    match t.kind() {
        _ if object_colon(t) => Cow::Borrowed("="),
        STRING if t.text().contains("${") => holes_written(t),
        _ => Cow::Borrowed(t.text()),
    }
}

/// A string whose holes write an object's `key: value`, with `key = value`.
fn holes_written(t: &SyntaxToken) -> Cow<'_, str> {
    let base = usize::from(t.text_range().start());
    let mut colons: Vec<usize> = crate::names::holes(t)
        .into_iter()
        .flat_map(|(at, tree)| {
            tree.descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(object_colon)
                .map(move |c| usize::from(at + c.text_range().start()) - base)
                .collect::<Vec<_>>()
        })
        .collect();
    if colons.is_empty() {
        return Cow::Borrowed(t.text());
    }
    colons.sort_unstable();
    let text = t.text();
    let mut out = String::with_capacity(text.len() + 2 * colons.len());
    let mut from = 0;
    for at in colons {
        out.push_str(&text[from..at]);
        if !out.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        out.push('=');
        if !text[at + 1..].starts_with(char::is_whitespace) {
            out.push(' ');
        }
        from = at + 1;
    }
    out.push_str(&text[from..]);
    Cow::Owned(out)
}

fn is_open(k: SyntaxKind) -> bool {
    matches!(k, L_PAREN | L_BRACKET | L_BRACE)
}

fn is_close(k: SyntaxKind) -> bool {
    matches!(k, R_PAREN | R_BRACKET | R_BRACE)
}

/// The space between two tokens on one line: "" or " ".
fn space(prev: &SyntaxToken, cur: &SyntaxToken) -> &'static str {
    let (p, c) = (prev.kind(), cur.kind());
    let (pp, cp) = (parent_kind(prev), parent_kind(cur));
    // Chains, dotted names and paths: `a.b[e].c`; ranges: `0..n`.
    if matches!(p, DOT | DOT2 | DOT2_EQ) || matches!(c, DOT | DOT2 | DOT2_EQ) {
        return "";
    }
    if c == L_BRACKET && matches!(cp, Some(INDEX | BLOCK_PATH | SELECTOR)) {
        return "";
    }
    if (p == L_BRACKET && matches!(pp, Some(INDEX | BLOCK_PATH | SELECTOR)))
        || (c == R_BRACKET && matches!(cp, Some(INDEX | BLOCK_PATH | SELECTOR)))
    {
        return "";
    }
    if matches!(p, PLUS | MINUS) && matches!(pp, Some(UNARY_EXPR | BIND_ARG)) {
        return "";
    }
    if matches!(c, COMMA | R_PAREN) || c == COLON && !object_colon(cur) {
        return "";
    }
    // Calls, atoms, type applications, declarations, a let's parameters
    // and records hug their name.
    if c == L_PAREN
        && matches!(
            cp,
            Some(ARG_LIST | TYPE_EXPR | DECL | EXTERN | INPUT_RELATION | PARAMS)
        )
    {
        return "";
    }
    // Empty brackets.
    if is_open(p) && is_close(c) {
        return "";
    }
    // Lists and argument lists are tight; comprehensions, objects, records,
    // bodies and blocks breathe.
    if p == L_PAREN || c == R_PAREN {
        return "";
    }
    if p == L_BRACKET && pp == Some(LIST) || c == R_BRACKET && cp == Some(LIST) {
        return "";
    }
    " "
}

/// The layout of a file free of syntax errors.
pub fn layout(root: &SyntaxNode) -> Doc {
    let mut l = Layout {
        c: Comments::of(root),
    };
    let mut v = l.statements(root.children());
    let eof = std::mem::take(&mut l.c.eof);
    for (i, c) in eof.into_iter().enumerate() {
        if !v.is_empty() {
            v.push(hard(i == 0 || c.blank));
        }
        v.push(text(c.text));
    }
    concat(v)
}

/// The blank line rule between a file's or a component's statements: an
/// empty line between statements of different kinds (a fact and a rule
/// are two) and above a comment block; none between consecutive facts of
/// one relation or consecutive `set` lines (a braced `set { }` is a block
/// of its own); else as the author wrote it (`blank`).
fn blank_between(prev: &SyntaxNode, next: &SyntaxNode, it: &Item) -> bool {
    if !it.leading.is_empty() || stmt_kind(prev) != stmt_kind(next) {
        return true;
    }
    let grouped = match next.kind() {
        SET => set_line(prev) && set_line(next),
        FACT => fact_relation(prev) == fact_relation(next),
        _ => false,
    };
    !grouped && it.blank
}

/// A statement's kind as its reader sees it: `input k`, `key k` and
/// `input r from ..` are inputs.
fn stmt_kind(n: &SyntaxNode) -> SyntaxKind {
    match n.kind() {
        INPUT_RELATION => INPUT,
        k => k,
    }
}

/// A `set` of one target, not a braced block of them.
fn set_line(n: &SyntaxNode) -> bool {
    n.children().all(|c| c.kind() != BLOCK)
}

/// The relation a fact is a row of.
fn fact_relation(n: &SyntaxNode) -> Option<String> {
    let call = n.children().find(|c| c.kind() == CALL)?;
    let chain = call.children().find(|c| c.kind() == CHAIN)?;
    Some(chain.text().to_string())
}

impl Layout {
    fn take_leading(&mut self, t: &SyntaxToken) -> Vec<Cmt> {
        self.c.leading.remove(&key(t)).unwrap_or_default()
    }

    fn take_trailing(&mut self, t: &SyntaxToken) -> Vec<String> {
        self.c.trailing.remove(&key(t)).unwrap_or_default()
    }

    /// A token, with any comment no element took: one above it on its own
    /// line, one after it at the end of its line.
    fn tok(&mut self, t: &SyntaxToken) -> Doc {
        let mut v = Vec::new();
        for c in self.take_leading(t) {
            v.push(Doc::Fresh);
            v.push(text(c.text));
            v.push(hard(false));
        }
        v.push(text(written(t).into_owned()));
        for c in self.take_trailing(t) {
            v.push(Doc::Suffix(format!(" {c}")));
            v.push(Doc::BreakParent);
        }
        concat(v)
    }

    /// An element from `first` to `last`, its comments taken before what
    /// is inside it can take them.
    fn item(
        &mut self,
        first: &SyntaxToken,
        last: &SyntaxToken,
        build: impl FnOnce(&mut Self) -> Doc,
    ) -> Item {
        let leading = self.take_leading(first);
        let trailing = self.take_trailing(last);
        let blank = self.c.blank.contains(&key(first));
        let doc = build(self);
        Item {
            leading,
            blank,
            doc,
            trailing,
        }
    }

    /// Statements, one per line, an empty line between two by
    /// [`blank_between`].
    fn statements(&mut self, nodes: impl IntoIterator<Item = SyntaxNode>) -> Vec<Doc> {
        let mut v = Vec::new();
        let mut prev: Option<SyntaxNode> = None;
        for n in nodes {
            let it = self.item_node(&n);
            if let Some(p) = &prev {
                v.push(hard(blank_between(p, &n, &it)));
            }
            v.extend(it.docs(nil()));
            prev = Some(n);
        }
        v
    }

    fn item_node(&mut self, n: &SyntaxNode) -> Item {
        match (first_token(n), last_token(n)) {
            (Some(f), Some(l)) => self.item(&f, &l, |s| s.node(n)),
            _ => Item {
                leading: Vec::new(),
                blank: false,
                doc: nil(),
                trailing: Vec::new(),
            },
        }
    }

    /// `open` elements `close` (the token `close`, printed by `close_doc`)
    /// as a group.
    fn seq(
        &mut self,
        open: Doc,
        items: Vec<Item>,
        close: Option<&SyntaxToken>,
        close_doc: impl FnOnce(&mut Self) -> Doc,
        shape: Shape,
    ) -> Doc {
        let Shape {
            pad,
            commas,
            force,
            stmt,
        } = shape;
        let dangling = close.map(|t| self.take_leading(t)).unwrap_or_default();
        let close = close_doc(self);
        if items.is_empty() && dangling.is_empty() && !has_break(&open) {
            return concat(vec![open, close]);
        }
        let n = items.len();
        let mut force = force;
        let mut inner = Vec::new();
        for (i, it) in items.into_iter().enumerate() {
            let blank = i > 0 && it.blank_above();
            force |= blank;
            inner.push(Doc::Line {
                flat: if i == 0 { pad } else { " " },
                hard: false,
                blank,
            });
            let last = i + 1 == n;
            let comma = match (commas, last) {
                (Commas::Trailing, true) => if_break(text(","), nil()),
                (Commas::Between | Commas::Trailing, false) => text(","),
                (Commas::Between | Commas::Newline, true) => nil(),
                (Commas::Newline, false) => if_break(nil(), text(",")),
            };
            inner.extend(it.docs(comma));
        }
        for c in dangling {
            inner.push(hard(c.blank && n > 0));
            inner.push(text(c.text));
        }
        let d = concat(vec![open, indent(concat(inner)), line(pad), close]);
        if stmt {
            stmt_group(d, force)
        } else {
            group(d, force)
        }
    }

    /// The elements of `elems` between the bracket at `open` and its
    /// closer, as a sequence; the index after the closer.
    fn bracketed(&mut self, elems: &[SyntaxElement], open: usize, shape: Shape) -> (Doc, usize) {
        let SyntaxElement::Token(o) = &elems[open] else {
            unreachable!("a bracket is a token")
        };
        let closer = match o.kind() {
            L_PAREN => R_PAREN,
            L_BRACKET => R_BRACKET,
            _ => R_BRACE,
        };
        let end = elems[open + 1..]
            .iter()
            .position(|e| e.kind() == closer)
            .map_or(elems.len(), |i| open + 1 + i);
        let open_doc = self.tok(o);
        let items: Vec<Item> = elems[open + 1..end]
            .iter()
            .filter_map(|e| e.as_node().cloned())
            .map(|n| self.item_node(&n))
            .collect();
        let close = elems.get(end).and_then(|e| e.as_token().cloned());
        let doc = self.seq(
            open_doc,
            items,
            close.as_ref(),
            |s| close.as_ref().map_or(nil(), |t| s.tok(t)),
            shape,
        );
        (doc, end + 1)
    }

    fn node(&mut self, n: &SyntaxNode) -> Doc {
        let elems = significant(n);
        match n.kind() {
            // `[a, b]`, `f(a, b)`; `[{ .. }]`, `f([ .. ])`.
            LIST | ARG_LIST => self.hug(n, &elems).unwrap_or_else(|| {
                self.bracketed(&elems, 0, Shape::term("", Commas::Trailing))
                    .0
            }),
            OBJECT => {
                self.bracketed(&elems, 0, Shape::term(" ", Commas::Trailing))
                    .0
            }
            COMPREHENSION => self.comprehension(&elems),
            TUPLE => {
                self.bracketed(&elems, 0, Shape::term("", Commas::Trailing))
                    .0
            }
            BODY => self.body(n, &elems),
            CLAUSE => match (elems.first(), n.children().find(|c| c.kind() == BODY)) {
                (Some(SyntaxElement::Token(w)), Some(b)) => self.clause(w, &b),
                _ => self.generic(n, &elems),
            },
            BLOCK => {
                // An entry with a body runs into the next on one line.
                let force = n.children().any(|e| e.children().any(|c| c.kind() == BODY));
                self.bracketed(&elems, 0, Shape::stmt(" ").force(force)).0
            }
            STMT_BLOCK => self.stmt_block(&elems),
            _ => self.generic(n, &elems),
        }
    }

    /// Tokens and nodes in a row, spaced by [`space`].
    fn generic(&mut self, n: &SyntaxNode, elems: &[SyntaxElement]) -> Doc {
        let mut out = Vec::new();
        let mut prev: Option<SyntaxToken> = None;
        let mut i = 0;
        while i < elems.len() {
            let e = &elems[i];
            let k = e.kind();
            // A clause prints the space before it, or the line break.
            let clause_body = match (e, elems.get(i + 1)) {
                (SyntaxElement::Token(w), Some(SyntaxElement::Node(b)))
                    if k == WHERE_KW && b.kind() == BODY =>
                {
                    Some((w.clone(), b.clone()))
                }
                _ => None,
            };
            let is_clause = clause_body.is_some() || k == CLAUSE;
            let (doc, next) = match (n.kind(), k) {
                _ if let Some((w, b)) = &clause_body => (self.clause(w, b), i + 2),
                // Fields of a type, an object input or output.
                (TYPE_DECL | INPUT | OUTPUT_DECL | ATTR_DECL, L_BRACE) => {
                    self.bracketed(elems, i, Shape::stmt(" "))
                }
                (TYPE_EXPR, L_BRACE) => {
                    self.bracketed(elems, i, Shape::term(" ", Commas::Trailing))
                }
                (DECL, L_PAREN) => self.bracketed(elems, i, Shape::term("", Commas::Trailing)),
                (EXTERN | TYPE_EXPR | PARAMS, L_PAREN) => {
                    self.bracketed(elems, i, Shape::term("", Commas::Between))
                }
                _ => {
                    let doc = match e {
                        SyntaxElement::Token(t) => self.tok(t),
                        SyntaxElement::Node(c) => self.node(c),
                    };
                    (doc, i + 1)
                }
            };
            let first = match e {
                SyntaxElement::Token(t) => Some(t.clone()),
                SyntaxElement::Node(c) => first_token(c),
            };
            if let (Some(p), Some(f)) = (&prev, &first)
                && !is_clause
            {
                out.push(text(space(p, f)));
            }
            prev = match &elems[next - 1] {
                SyntaxElement::Token(t) => Some(t.clone()),
                SyntaxElement::Node(c) => last_token(c).or(prev),
            };
            out.push(doc);
            i = next;
        }
        concat(out)
    }

    /// `outer`'s brackets hugging their only element, `[{` .. `}]`,
    /// `f([` .. `])`, when it is a list or an object with something in it
    /// and no comment sits between them: its own group breaks, the
    /// brackets stay on its outer lines. A keyed element (an object's
    /// field, a named argument) is not hugged: its key is on its own line.
    fn hug(&mut self, outer: &SyntaxNode, elems: &[SyntaxElement]) -> Option<Doc> {
        let mut items = outer.children();
        let (Some(sole), None) = (items.next(), items.next()) else {
            return None;
        };
        if !matches!(sole.kind(), OBJECT | LIST)
            || sole.children().next().is_none()
            || self.commented_around(outer, &sole)
        {
            return None;
        }
        let (SyntaxElement::Token(o), Some(SyntaxElement::Token(c))) = (&elems[0], elems.last())
        else {
            unreachable!("a list or an argument list is bracketed")
        };
        Some(concat(vec![self.tok(o), self.node(&sole), self.tok(c)]))
    }

    /// Whether a comment no element took is on `t`.
    fn commented(&self, t: &SyntaxToken) -> bool {
        self.c.leading.contains_key(&key(t)) || self.c.trailing.contains_key(&key(t))
    }

    /// Whether a comment sits between `outer`'s brackets and `inner`, its
    /// only element.
    fn commented_around(&self, outer: &SyntaxNode, inner: &SyntaxNode) -> bool {
        let (Some(o), Some(c)) = (first_token(outer), last_token(outer)) else {
            return true;
        };
        let (Some(f), Some(l)) = (first_token(inner), last_token(inner)) else {
            return true;
        };
        self.c.trailing.contains_key(&key(&o))
            || self.c.leading.contains_key(&key(&f))
            || self.c.trailing.contains_key(&key(&l))
            || self.c.leading.contains_key(&key(&c))
    }

    /// Whether a comment no element took is inside `n`.
    fn comments_in(&self, n: &SyntaxNode) -> bool {
        n.descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| self.commented(&t))
    }

    /// `[ item | a, b ]`: broken, `[ item |`, a literal per line, `]`.
    fn comprehension(&mut self, elems: &[SyntaxElement]) -> Doc {
        let mut open = Vec::new();
        let mut prev: Option<SyntaxToken> = None;
        let mut body = None;
        let mut close = None;
        for e in elems {
            match e {
                SyntaxElement::Node(b) if b.kind() == BODY => body = Some(b.clone()),
                SyntaxElement::Token(t) if t.kind() == R_BRACKET => close = Some(t.clone()),
                _ => {
                    let first = match e {
                        SyntaxElement::Token(t) => Some(t.clone()),
                        SyntaxElement::Node(c) => first_token(c),
                    };
                    if let (Some(p), Some(f)) = (&prev, &first) {
                        open.push(text(space(p, f)));
                    }
                    prev = match e {
                        SyntaxElement::Token(t) => Some(t.clone()),
                        SyntaxElement::Node(c) => last_token(c),
                    };
                    open.push(match e {
                        SyntaxElement::Token(t) => self.tok(t),
                        SyntaxElement::Node(c) => self.node(c),
                    });
                }
            }
        }
        let items: Vec<Item> = body
            .iter()
            .flat_map(|b| b.children())
            .map(|l| self.item_node(&l))
            .collect();
        self.seq(
            concat(open),
            items,
            close.as_ref(),
            |s| close.as_ref().map_or(nil(), |t| s.tok(t)),
            Shape::term(" ", Commas::Trailing),
        )
    }

    /// A body not a clause's: a refinement's on its line, `not { .. }`'s
    /// braced.
    fn body(&mut self, n: &SyntaxNode, elems: &[SyntaxElement]) -> Doc {
        if n.parent().map(|p| p.kind()) == Some(LIT_NOT_BLOCK) {
            let many = n.children().count() > INLINE_LITERALS;
            let shape = Shape::term(" ", Commas::Newline).force(many);
            return self.bracketed(elems, 0, shape).0;
        }
        self.generic(n, elems)
    }

    /// `where` and its body, in the first layout that fits
    /// ([`Doc::Clause`]): on the statement's line, on a hanging line, or a
    /// literal per line, each with its comma. Only the last when the body
    /// has more than [`INLINE_LITERALS`] literals, a comment or an empty
    /// line.
    fn clause(&mut self, word: &SyntaxToken, body: &SyntaxNode) -> Doc {
        let lits: Vec<SyntaxNode> = body.children().collect();
        let broken_only = lits.len() > INLINE_LITERALS
            || self.commented(word)
            || self.comments_in(body)
            || lits
                .iter()
                .skip(1)
                .filter_map(first_token)
                .any(|t| self.c.blank.contains(&key(&t)));
        let one_line = (!broken_only).then(|| {
            let mut v = Vec::new();
            for (i, l) in lits.iter().enumerate() {
                if i > 0 {
                    v.push(text(", "));
                }
                v.push(self.node(l));
            }
            concat(v)
        });
        let mut inner = Vec::new();
        for (i, l) in lits.iter().enumerate() {
            let it = self.item_node(l);
            inner.push(hard(i > 0 && it.blank_above()));
            inner.extend(it.docs(text(",")));
        }
        let broken = concat(vec![text(" "), self.tok(word), indent(concat(inner))]);
        clause(one_line, broken)
    }

    /// A component's statements: a statement per line, always.
    fn stmt_block(&mut self, elems: &[SyntaxElement]) -> Doc {
        let (Some(SyntaxElement::Token(o)), Some(SyntaxElement::Token(c))) =
            (elems.first(), elems.last())
        else {
            unreachable!("a statement block is braced")
        };
        let open = self.tok(o);
        let stmts = self.statements(
            elems[1..elems.len() - 1]
                .iter()
                .filter_map(|e| e.as_node().cloned()),
        );
        let dangling = self.take_leading(c);
        let close = self.tok(c);
        if stmts.is_empty() && dangling.is_empty() {
            return concat(vec![open, close]);
        }
        let any = !stmts.is_empty();
        let mut inner = Vec::new();
        if any {
            inner.push(hard(false));
            inner.extend(stmts);
        }
        // A comment block below the last statement has an empty line above.
        for (i, c) in dangling.into_iter().enumerate() {
            inner.push(hard(any && (i == 0 || c.blank)));
            inner.push(text(c.text));
        }
        concat(vec![open, indent(concat(inner)), hard(false), close])
    }
}
