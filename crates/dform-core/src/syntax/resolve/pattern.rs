//! Patterns (docs/grammar.md "Patterns", R-58): one production, used on
//! the left of `in`, on the left of `=` in a body or a `let`'s clause, and
//! as a relation's argument.
//!
//! ```text
//! pattern := "_" | NAME | literal | "(" pattern ("," pattern)+ ")" | "{" field ("," field)* ("," ".." NAME)? "}"
//! field   := key (":" pattern)?
//! ```
//!
//! A name binds (or, bound already, compares), `_` matches anything, a
//! literal compares. A tuple needs the exact arity: it unifies with a list
//! of that many elements. An object pattern binds the fields it names and
//! ignores the rest; it lowers to a fresh variable and one field read per
//! field, after the literal that binds the variable. Its last entry may
//! be `..name`, which binds the rest (R-199): the object without the keys
//! the pattern names, the notation a literal's spread is.
//!
//! | written                         | lowers to                                          |
//! |---------------------------------|----------------------------------------------------|
//! | `(k, v) in e`                   | `member(e', K, V)` (an object's keys, a list's indexes) |
//! | `(a, b) = e`                    | `[A, B] = e'`                                      |
//! | `{ host, port: p } = e`         | `Conn = e', Host = __path(Conn, "host"), P = __path(Conn, "port")` |
//! | `{ metadata: m, ..body } = e`   | `Obj = e', M = __path(Obj, "metadata"), Body = __rest(Obj, "metadata")` |
//! | `zone({ name, index })`         | `zone{name: Name, index: Index}`, a record pattern |

use super::*;

impl Lowerer<'_> {
    /// Is `n` a pattern that is not also a term: a tuple, or an object on
    /// the left of `=` or `in`.
    pub(super) fn is_pattern(n: &SyntaxNode) -> bool {
        matches!(n.kind(), TUPLE | OBJECT)
    }

    /// Lower pattern `n`: the term that unifies, the reads its values make
    /// into `pre`, and an object pattern's field reads into `self.after`
    /// (appended after the literal that binds it).
    pub(super) fn pattern(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<Term> {
        self.bind(true, |l| l.pattern1(rc, n, pre))
    }

    fn pattern1(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<Term> {
        match n.kind() {
            TUPLE => {
                let mut out = Vec::new();
                for t in terms(n) {
                    if t.kind() == SPREAD {
                        return self.tuple_rest(&t);
                    }
                    out.push(self.pattern1(rc, &t, pre)?);
                }
                Ok(Term::List(out))
            }
            OBJECT => {
                let at = self.span(n);
                let whole = var(&fresh(rc, "Obj"));
                let mut after = Vec::new();
                let mut keys = vec![whole.clone()];
                let mut rest = None;
                for f in n
                    .children()
                    .filter(|c| matches!(c.kind(), OBJECT_FIELD | SPREAD))
                {
                    if rest.is_some() {
                        return self.rest_not_last(n, &f);
                    }
                    if f.kind() == SPREAD {
                        rest = Some(self.rest(rc, &f, pre)?);
                        continue;
                    }
                    let k = tokens(&f).next().ok_or(Skip)?;
                    let key = if k.kind() == STRING {
                        self.string(&k)?
                    } else {
                        k.text().to_string()
                    };
                    if key.contains('.') {
                        return self.error(
                            self.span(&f),
                            format!("an object pattern names a field, and `{key}` is a path"),
                        );
                    }
                    let field = match terms(&f).next() {
                        Some(p) => self.pattern1(rc, &p, pre)?,
                        // `{ a }` binds `a` to the field `a`.
                        None => self.pun(rc, &k, pre)?,
                    };
                    // A nested object's reads follow its own binding.
                    let nested = std::mem::take(&mut self.after);
                    after.push(Lit::Eq(
                        field,
                        func("__path", vec![whole.clone(), str_term(&key)]),
                    ));
                    after.extend(nested);
                    keys.push(str_term(&key));
                }
                if let Some(rest) = rest {
                    after.push(Lit::Eq(rest, func(crate::functions::REST, keys)));
                }
                if after.is_empty() {
                    return self.error(at, "an object pattern names at least one field");
                }
                self.after.extend(after);
                Ok(whole)
            }
            SPREAD => self.misplaced_spread(n),
            LIST => {
                let d = Diagnostic::error(
                    self.span(n),
                    format!("`{}` is a list: a pattern is a tuple", n.text()),
                )
                .with_help(format!(
                    "write `({})`: it matches a list of exactly that many elements",
                    terms(n)
                        .map(|t| t.text().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                self.diags.push(d);
                Err(Skip)
            }
            CHAIN if Chain::of(n).is_some_and(|c| c.is_bare() && c.head == "_") => {
                Ok(Term::Wildcard)
            }
            _ => self.term(rc, n, Pos::Content, pre),
        }
    }

    /// `..name` ending an object pattern (R-199): the name the rest binds.
    /// `..` alone is an error: an object pattern ignores what it does not
    /// name already.
    fn rest(&mut self, rc: &mut Rc, f: &SyntaxNode, pre: &mut Vec<Lit>) -> L<Term> {
        let pattern = f.parent().map(|p| p.text().to_string()).unwrap_or_default();
        let Some(name) = terms(f).next() else {
            let without = pattern
                .replace(", ..}", "}")
                .replace(", .. }", " }")
                .replace(",..}", "}");
            return self.error_help(
                self.span(f),
                format!(
                    "`..` in `{pattern}` ignores the rest, which an object pattern does already"
                ),
                format!("leave it out, `{without}`; to bind the rest, name it, `..rest`"),
            );
        };
        if !Chain::of(&name).is_some_and(|c| c.is_bare()) {
            return self.error_help(
                self.span(f),
                format!(
                    "`{}` in an object pattern binds the rest to a name",
                    f.text()
                ),
                "a name after the dots binds the rest, `..rest`; match what it holds with \
                 another pattern, `{ a, ..rest } = x, { b } = rest`"
                    .to_string(),
            );
        }
        self.pattern1(rc, &name, pre)
    }

    /// An entry after an object pattern's rest.
    fn rest_not_last<T>(&mut self, n: &SyntaxNode, f: &SyntaxNode) -> L<T> {
        let rest = n
            .children()
            .find(|c| c.kind() == SPREAD)
            .map(|c| c.text().to_string())
            .unwrap_or_default();
        self.error_help(
            self.span(f),
            format!(
                "`{}` follows the rest `{rest}`: the rest is a pattern's last entry",
                f.text()
            ),
            format!("move `{rest}` to the end of the pattern"),
        )
    }

    /// `..` in a tuple pattern: a tuple has an exact arity, and a list is
    /// walked with `in`.
    fn tuple_rest<T>(&mut self, t: &SyntaxNode) -> L<T> {
        self.error_help(
            self.span(t),
            format!(
                "`{}` in a tuple pattern: a tuple matches a list of exactly as many elements as it names",
                t.text()
            ),
            "a list's elements are each `(i, x) in xs`, its first `xs[0]`".to_string(),
        )
    }

    /// Whether relation `pred` has several columns, each named: what a
    /// record pattern `p({ a, b })` matches.
    pub(super) fn named_columns(&self, scope: ScopeId, pred: &str) -> bool {
        let (scopes, name) = match pred.split_once("::") {
            Some((m, p)) => (
                self.use_in(scope, m)
                    .and_then(|path| self.module_at(&path))
                    .map(|m| vec![m.0])
                    .unwrap_or_default(),
                p,
            ),
            None => (self.program.scopes.ids().collect(), pred),
        };
        let mut arities = BTreeSet::new();
        for s in scopes {
            arities.extend(self.names(s).arities(name));
        }
        !arities.is_empty() && !arities.contains(&1)
    }

    /// `p({ a, b: pat })` in a body: the record pattern `p{a: A, b: pat}`,
    /// the columns it does not name ignored.
    pub(super) fn record_pattern(
        &mut self,
        rc: &mut Rc,
        o: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<BTreeMap<String, Term>> {
        let mut fields = BTreeMap::new();
        for f in o.children().filter(|c| c.kind() == OBJECT_FIELD) {
            let k = tokens(&f).next().ok_or(Skip)?;
            let key = k.text().to_string();
            if k.kind() == STRING {
                return self.error(
                    self.span(&f),
                    format!("a column is named by a name, not a string: `{}`", k.text()),
                );
            }
            let value = match terms(&f).next() {
                Some(p) => self.pattern(rc, &p, pre)?,
                None => self.pun(rc, &k, pre)?,
            };
            if fields.insert(key.clone(), value).is_some() {
                return self.error(self.span(&f), format!("column `{key}` given twice"));
            }
        }
        Ok(fields)
    }

    /// `{ a }`: the name `a`, binding (or, a name already, comparing).
    fn pun(&mut self, rc: &mut Rc, k: &SyntaxToken, pre: &mut Vec<Lit>) -> L<Term> {
        let c = Chain {
            head: k.text().to_string(),
            head_kind: k.kind(),
            call: None,
            range: k.text_range(),
            ops: Vec::new(),
        };
        let span = self.span_of(k.text_range());
        self.bind(true, |l| {
            let res = l.resolve(rc, &c, pre)?;
            l.realize(rc, res, Pos::Content, pre, span)
        })
    }

    /// A tuple where a value is wanted: an error that says where a pattern
    /// goes.
    pub(super) fn tuple_value<T>(&mut self, n: &SyntaxNode) -> L<T> {
        let items: Vec<String> = terms(n).map(|t| t.text().to_string()).collect();
        let d = Diagnostic::error(
            self.span(n),
            format!("`{}` is a pattern, not a value", n.text()),
        )
        .with_help(format!(
            "a tuple matches: after `in` (`(k, v) in obj`), on the left of `=` (`(a, b) = \
             pair`) or as a relation's argument; a list value is `[{}]`",
            items.join(", ")
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `P = e` with `P` a tuple or an object: `e` matched by the pattern.
    pub(super) fn pattern_eq(
        &mut self,
        rc: &mut Rc,
        p: &SyntaxNode,
        e: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> L<()> {
        let value = self.bind(false, |l| l.term(rc, e, Pos::Content, out))?;
        let pat = self.pattern(rc, p, out)?;
        out.push(Lit::Eq(pat, value));
        Ok(())
    }

    /// `(k, v) in e`, `(i, x) in e`: each key and value of an object, each
    /// index and element of a list.
    pub(super) fn pattern_in(
        &mut self,
        rc: &mut Rc,
        lhs: &SyntaxNode,
        list: Term,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<Lit> {
        let parts: Vec<SyntaxNode> = terms(lhs).collect();
        let [k, v] = parts.as_slice() else {
            return self.error(
                span,
                format!(
                    "`{}` has {} parts: after `in`, a tuple is `(key, value)` of an object or \
                     `(index, element)` of a list",
                    lhs.text(),
                    parts.len()
                ),
            );
        };
        let k = self.pattern(rc, k, out)?;
        let v = self.pattern(rc, v, out)?;
        Ok(Lit::Pos(atom_at("member", vec![list, k, v], span)))
    }
}
