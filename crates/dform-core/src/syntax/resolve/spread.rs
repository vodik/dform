//! Spread in literals (docs/grammar.md "Literals and terms", R-199): `..x`
//! leads an object's field or a list's element and gives the fields or
//! the elements of `x` there, in order with the written ones, the last
//! written key winning. An object pattern's rest is the same `..`
//! (`syntax::resolve::pattern`).
//!
//! | written                    | lowers to                                   |
//! |----------------------------|---------------------------------------------|
//! | `{ ..base, replicas: 3 }`  | `__merge(base', {replicas: 3})`             |
//! | `{ a: 1, ..{ b: 2 } }`     | `{a: 1, b: 2}`, folded when every part is written |
//! | `[..a, x, ..b]`            | `__concat(a', [x'], b')`                    |
//! | `[..0..3]`                 | `[0, 1, 2]`, a discrete range's members     |
//!
//! A part whose kind the lowering sees (a literal, a list, a call's
//! declared result) is checked here; any other is checked when it has a
//! value, an error naming it (`engine::spread_error`).

use super::*;
use crate::functions::{CONCAT, MERGE};

/// What a spread spreads into.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Object,
    List,
}

impl Target {
    fn takes(self) -> &'static str {
        match self {
            Target::Object => "an object, which takes an object's fields",
            Target::List => "a list, which takes a list's elements",
        }
    }
}

impl Lowerer<'_> {
    /// `{ a: 1, ..base, b: x }`: the parts in order, folded where each is
    /// written, else `__merge` of them.
    pub(super) fn spread_object(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let mut parts = Vec::new();
        let mut run = Vec::new();
        for c in n.children() {
            match c.kind() {
                OBJECT_FIELD => run.push(c),
                SPREAD => {
                    if !run.is_empty() {
                        parts.push(self.object_fields(rc, &std::mem::take(&mut run), pos, pre)?);
                    }
                    parts.push(self.spread_source(rc, &c, Target::Object, pre)?);
                }
                _ => {}
            }
        }
        if !run.is_empty() {
            parts.push(self.object_fields(rc, &run, pos, pre)?);
        }
        Ok(merged(parts))
    }

    /// `[a, ..xs, b]`: the parts in order, folded where each is written,
    /// else `__concat` of them.
    pub(super) fn list(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let mut parts = Vec::new();
        let mut run = Vec::new();
        for t in terms(n) {
            if t.kind() == SPREAD {
                if !run.is_empty() {
                    parts.push(Term::List(std::mem::take(&mut run)));
                }
                parts.push(self.spread_source(rc, &t, Target::List, pre)?);
            } else {
                run.push(self.term(rc, &t, pos, pre)?);
            }
        }
        if !run.is_empty() || parts.is_empty() {
            parts.push(Term::List(run));
        }
        Ok(concatenated(parts))
    }

    /// The value a spread `..x` gives, read where it stands; an error when
    /// what it is is known and is not what the literal takes.
    fn spread_source(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        into: Target,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let span = self.span(n);
        let Some(inner) = terms(n).next() else {
            let example = match into {
                Target::Object => "{ ..base, replicas: 3 }",
                Target::List => "[..xs, x]",
            };
            return self.error_help(
                span,
                "`..` here spreads nothing: a spread names the value it spreads",
                format!("name it after the dots, `{example}`"),
            );
        };
        let t = self.bind(false, |l| l.term(rc, &inner, Pos::Content, pre))?;
        let src = inner.text().to_string();
        if let (Target::List, Term::Val(Value::Range(r))) = (into, &t) {
            return match r.members() {
                Ok(xs) => Ok(Term::List(xs.into_iter().map(Term::Val).collect())),
                Err(why) => self.error(span, format!("`..{src}`: {why}")),
            };
        }
        let Some(kind) = kind_of(&t) else {
            return Ok(t);
        };
        let fits = match into {
            Target::Object => kind == "object" || kind.starts_with("map"),
            Target::List => kind == "list" || kind.starts_with("list(") || kind == "range",
        };
        if fits {
            return Ok(t);
        }
        let help = match (into, kind.as_str()) {
            (Target::Object, "list") => format!("a list spreads into a list, `[..{src}]`"),
            (Target::Object, _) => format!("a field gives a value a key, `{{ name: {src} }}`"),
            (Target::List, "object") => {
                format!("an object spreads into an object, `{{ ..{src} }}`")
            }
            (Target::List, _) => format!("an element is written without the dots, `[{src}]`"),
        };
        self.error_help(
            span,
            format!(
                "`..{src}` spreads {} into {}",
                crate::value::article(&kind),
                into.takes()
            ),
            help,
        )
    }

    /// `..x` outside an object or a list: an error that says where a
    /// spread goes.
    pub(super) fn misplaced_spread<T>(&mut self, n: &SyntaxNode) -> L<T> {
        let src = terms(n).next().map(|t| t.text().to_string());
        let src = src.as_deref().unwrap_or("x");
        self.error_help(
            self.span(n),
            format!(
                "`{}` is a spread: it leads a field of an object or an element of a list",
                n.text()
            ),
            format!("write the value it makes, `{{ ..{src} }}` or `[..{src}]`"),
        )
    }

    /// A key an object literal writes twice: an error naming both. A key
    /// after a spread is the spread's override, so only written keys
    /// count.
    pub(super) fn written_twice(&mut self, n: &SyntaxNode) -> L<()> {
        let mut seen: BTreeMap<String, SyntaxNode> = BTreeMap::new();
        for f in n.children().filter(|c| c.kind() == OBJECT_FIELD) {
            let Some(k) = tokens(&f).next() else { continue };
            let key = match k.kind() {
                STRING if !self.text && has_hole(k.text()) => continue,
                STRING => self.string(&k)?,
                _ => k.text().to_string(),
            };
            if let Some(first) = seen.get(&key) {
                let d = Diagnostic::error(self.span(&f), format!("key `{key}` given twice"))
                    .with_label(self.span(first), format!("`{key}` is first given here"))
                    .with_help(format!(
                        "keep one `{key}`: an object's key is written once, and a key after \
                         a spread `..x` is what replaces x's"
                    ));
                self.diags.push(d);
                return Err(Skip);
            }
            seen.insert(key, f);
        }
        Ok(())
    }

    /// An error at `span` with its help.
    pub(super) fn error_help<T>(
        &mut self,
        span: Span,
        msg: impl Into<String>,
        help: String,
    ) -> L<T> {
        self.diags
            .push(Diagnostic::error(span, msg).with_help(help));
        Err(Skip)
    }
}

/// The kind of value `t` is when the lowering sees it: a literal's, a
/// list's or an object's, a call's declared result; `None` when only its
/// value says.
fn kind_of(t: &Term) -> Option<String> {
    let kind = match t {
        Term::Val(Value::Null { .. }) => return None,
        Term::Val(v) => crate::value::type_name(v).to_string(),
        Term::List(_) | Term::ListComp { .. } => "list".into(),
        Term::Obj(_) => "object".into(),
        Term::Func { name, .. } if name == MERGE || name == crate::functions::OBJECT => {
            "object".into()
        }
        Term::Func { name, .. } if name == CONCAT => "list".into(),
        Term::Func { name, .. } => {
            let ret = crate::functions::get(name)?
                .ret
                .trim_end_matches('?')
                .to_string();
            match ret.as_str() {
                "any" | "number" => return None,
                _ => ret,
            }
        }
        Term::Var(_) | Term::Wildcard => return None,
    };
    Some(kind)
}

/// An object's parts merged: adjacent written ones folded into one, and
/// one written object alone is itself.
fn merged(parts: Vec<Term>) -> Term {
    let mut out: Vec<Term> = Vec::new();
    for p in parts {
        let p = match p {
            Term::Val(Value::Obj(m)) => {
                Term::Obj(m.into_iter().map(|(k, v)| (k, Term::Val(v))).collect())
            }
            p => p,
        };
        match (out.last_mut(), p) {
            (Some(Term::Obj(a)), Term::Obj(b)) => a.extend(b),
            (_, p) => out.push(p),
        }
    }
    match out.as_slice() {
        [Term::Obj(_)] => out.pop().unwrap(),
        _ => func(MERGE, out),
    }
}

/// A list's parts concatenated: adjacent written ones folded into one,
/// and one written list alone is itself.
fn concatenated(parts: Vec<Term>) -> Term {
    let mut out: Vec<Term> = Vec::new();
    for p in parts {
        let p = match p {
            Term::Val(Value::List(xs)) => Term::List(xs.into_iter().map(Term::Val).collect()),
            p => p,
        };
        match (out.last_mut(), p) {
            (Some(Term::List(a)), Term::List(b)) => a.extend(b),
            (_, p) => out.push(p),
        }
    }
    match out.as_slice() {
        [Term::List(_)] => out.pop().unwrap(),
        _ => func(CONCAT, out),
    }
}
