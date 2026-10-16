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
use crate::program::node::ObjPart;

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

/// A part of an object or a list being spread into, as the lowering folds
/// it: a run of written fields or elements, or another value spread.
enum Part<T> {
    Run(Vec<T>),
    Spread(ExprId),
}

impl Lowerer<'_> {
    /// `{ a: 1, ..base, b: x }`: the parts in order, folded where each is
    /// written (a written object spread is its fields), a run of fields
    /// with a computed key its object spread.
    pub(super) fn spread_object(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let mut parts = Vec::new();
        let mut run = Vec::new();
        for c in n.children() {
            match c.kind() {
                OBJECT_FIELD => run.push(c),
                SPREAD => {
                    if !run.is_empty() {
                        let fields = std::mem::take(&mut run);
                        parts.push(self.field_run(rc, &fields, pos)?);
                    }
                    let id = self.spread_source(rc, &c, Target::Object)?;
                    parts.push(self.object_part(id));
                }
                _ => {}
            }
        }
        if !run.is_empty() {
            parts.push(self.field_run(rc, &run, pos)?);
        }
        let parts = folded(parts);
        let parts = match parts.as_slice() {
            [Part::Run(_)] => match parts.into_iter().next() {
                Some(Part::Run(fields)) => fields,
                _ => unreachable!("one run"),
            },
            _ => self.unfolded(
                parts,
                self.span(n),
                |parts| ExprKind::Object { parts },
                ObjPart::Spread,
            ),
        };
        Ok(self.node(self.span(n), ExprKind::Object { parts }))
    }

    /// `[a, ..xs, b]`: the parts in order, folded where each is written (a
    /// written list spread is its elements, a known range its members).
    pub(super) fn list(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let mut parts = Vec::new();
        let mut run = Vec::new();
        for t in terms(n) {
            if t.kind() == SPREAD {
                if !run.is_empty() {
                    parts.push(Part::Run(std::mem::take(&mut run)));
                }
                let id = self.spread_source(rc, &t, Target::List)?;
                parts.push(self.list_part(id));
            } else {
                run.push(ListPart::Elem(self.expr(rc, &t, pos)?));
            }
        }
        if !run.is_empty() || parts.is_empty() {
            parts.push(Part::Run(run));
        }
        let parts = folded(parts);
        let parts = match parts.as_slice() {
            [Part::Run(_)] => match parts.into_iter().next() {
                Some(Part::Run(elems)) => elems,
                _ => unreachable!("one run"),
            },
            _ => self.unfolded(
                parts,
                self.span(n),
                |parts| ExprKind::List { parts },
                ListPart::Spread,
            ),
        };
        Ok(self.node(self.span(n), ExprKind::List { parts }))
    }

    /// A run of written fields: its fields, or, a key computed, the
    /// object they make spread.
    fn field_run(&mut self, rc: &mut Rc, fields: &[SyntaxNode], pos: Pos) -> L<Part<ObjPart>> {
        let parts = self.object_fields(rc, fields, pos)?;
        if !parts.iter().any(|p| matches!(p, ObjPart::Computed { .. })) {
            return Ok(Part::Run(parts));
        }
        let span = self.span(&fields[0]);
        Ok(Part::Spread(self.node(span, ExprKind::Object { parts })))
    }

    /// What a value spread into an object is: the fields of a written
    /// object or of a constant one, else the value.
    fn object_part(&mut self, id: ExprId) -> Part<ObjPart> {
        let e = &self.program.exprs[id];
        let span = e.span;
        match (&e.kind, e.hoisted) {
            (ExprKind::Object { parts }, None)
                if parts.iter().all(|p| matches!(p, ObjPart::Field { .. })) =>
            {
                Part::Run(parts.clone())
            }
            (ExprKind::Lit(Value::Obj(m)), None) => {
                let m = m.clone();
                let fields = m.into_iter().map(|(key, v)| ObjPart::Field {
                    key,
                    key_span: span,
                    value: self.node(span, ExprKind::Lit(v)),
                    pun: false,
                });
                Part::Run(fields.collect())
            }
            _ => Part::Spread(id),
        }
    }

    /// What a value spread into a list is: the elements of a written list
    /// or of a constant one, else the value.
    fn list_part(&mut self, id: ExprId) -> Part<ListPart> {
        let e = &self.program.exprs[id];
        let span = e.span;
        match (&e.kind, e.hoisted) {
            (ExprKind::List { parts }, None)
                if parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                Part::Run(parts.clone())
            }
            (ExprKind::Lit(Value::List(xs)), None) => {
                let xs = xs.clone();
                let elems = xs
                    .into_iter()
                    .map(|v| ListPart::Elem(self.node(span, ExprKind::Lit(v))));
                Part::Run(elems.collect())
            }
            _ => Part::Spread(id),
        }
    }

    /// Parts that do not fold into one run: each run its fields or
    /// elements, an empty one (a written `{}` or `[]` spread) a spread of
    /// itself.
    fn unfolded<T>(
        &mut self,
        parts: Vec<Part<T>>,
        span: Span,
        empty: impl Fn(Vec<T>) -> ExprKind,
        spread: impl Fn(ExprId) -> T,
    ) -> Vec<T> {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Part::Run(xs) if xs.is_empty() => {
                    out.push(spread(self.node(span, empty(xs))));
                }
                Part::Run(xs) => out.extend(xs),
                Part::Spread(id) => out.push(spread(id)),
            }
        }
        out
    }

    /// The value a spread `..x` gives, read where it stands; an error when
    /// what it is is known and is not what the literal takes. A known
    /// range spread into a list is its members.
    fn spread_source(&mut self, rc: &mut Rc, n: &SyntaxNode, into: Target) -> L<ExprId> {
        let span = self.span(n);
        let Some(inner) = terms(n).next() else {
            let example = match into {
                Target::Object => "{ ..base, replicas = 3 }",
                Target::List => "[..xs, x]",
            };
            return self.error_help(
                span,
                "`..` here spreads nothing: a spread names the value it spreads",
                format!("name it after the dots, `{example}`"),
            );
        };
        let id = self.bind(false, |l| l.expr(rc, &inner, Pos::Content))?;
        let t = self.term_of(id);
        let src = inner.text().to_string();
        if let (Target::List, Term::Val(Value::Range(r))) = (into, &t) {
            return match r.members() {
                Ok(xs) => {
                    let parts = xs
                        .into_iter()
                        .map(|v| ListPart::Elem(self.node(span, ExprKind::Lit(v))))
                        .collect();
                    Ok(self.node(span, ExprKind::List { parts }))
                }
                Err(why) => self.error(span, format!("`..{src}`: {why}")),
            };
        }
        let Some(kind) = kind_of(&t) else {
            return Ok(id);
        };
        let fits = match into {
            Target::Object => kind == "object" || kind.starts_with("map"),
            Target::List => kind == "list" || kind.starts_with("list(") || kind == "range",
        };
        if fits {
            return Ok(id);
        }
        let help = match (into, kind.as_str()) {
            (Target::Object, "list") => format!("a list spreads into a list, `[..{src}]`"),
            (Target::Object, _) => format!("a field gives a value a key, `{{ name = {src} }}`"),
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

/// Adjacent runs folded into one.
fn folded<T>(parts: Vec<Part<T>>) -> Vec<Part<T>> {
    let mut out: Vec<Part<T>> = Vec::new();
    for p in parts {
        match (out.last_mut(), p) {
            (Some(Part::Run(a)), Part::Run(b)) => a.extend(b),
            (_, p) => out.push(p),
        }
    }
    out
}
