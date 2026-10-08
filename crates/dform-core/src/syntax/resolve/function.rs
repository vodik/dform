//! A let with parameters (R-187, docs/grammar.md "Lets"): `let f(a, b) = t
//! [where B]` is the relation `f(a, b, v)`, the rule `f(A, B, V) :- B, V =
//! t`, with the mode `(+, +, -)`: its parameters are bound by whoever reads
//! it, as a provider's table's `+` columns are, and its value is its last
//! column. A call `f(x, y)` in a term is a fresh variable bound through the
//! relation, `f(x', y', F)`, read where the call stands (R-71's chain after
//! a call); `f(x, y, v)` after `where` is the same read, its `+` arguments
//! bound by the body. Named arguments and defaults are std functions'
//! (R-155). `crate::demand` answers each read where it is made.

use super::*;

/// One parameter of a let: `name [: T] [= default]`.
pub(super) struct Param {
    pub(super) name: String,
    ty: Option<SyntaxNode>,
    default: Option<SyntaxNode>,
}

impl Param {
    fn of(n: &SyntaxNode) -> Param {
        Param {
            name: word_text(n, 0),
            ty: node(n, TYPE_EXPR),
            default: terms(n).next(),
        }
    }
}

/// The parameters of a `let` statement, `None` for a let without them.
pub(super) fn params(n: &SyntaxNode) -> Option<Vec<Param>> {
    let list = node(n, PARAMS)?;
    Some(
        list.children()
            .filter(|c| c.kind() == PARAM)
            .map(|p| Param::of(&p))
            .collect(),
    )
}

/// `f(a, b)`: how a message writes the let's call.
fn signature(name: &str, ps: &[Param]) -> String {
    let names: Vec<&str> = ps.iter().map(|p| p.name.as_str()).collect();
    format!("{name}({})", names.join(", "))
}

impl Lowerer<'_> {
    /// The let with parameters `name` in `scope`, looked up outward: the
    /// scope that declares it and its first statement.
    pub(super) fn function_def(&self, scope: usize, name: &str) -> Option<(usize, SyntaxNode)> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.decls.scopes[s]
                .functions
                .get(name)
                .map(|n| (s, n.clone()))
        })
    }

    /// How many leading columns of the relation `name` a read in `scope`
    /// must bind: a let's parameters (R-187).
    pub(super) fn function_inputs(&self, scope: usize, name: &str) -> Option<usize> {
        let (_, def) = self.function_def(scope, name)?;
        Some(params(&def)?.len())
    }

    /// `let f(a, b) [: T] = t [where B]`: the rule `f(A, B, t') :- B,
    /// reads` and its mode.
    pub(super) fn function_stmt(
        &mut self,
        n: &SyntaxNode,
        scope: usize,
        outer: &Rc,
    ) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let ps = params(n).unwrap_or_default();
        if ps.is_empty() {
            return self.error(
                span,
                format!("`let {name}()` has no parameters: a value is `let {name} = ..`"),
            );
        }
        if self.rank_tok(n)?.is_some() {
            return self.error(
                span,
                format!(
                    "a rank orders a value's rows: `let {}` is a relation, whose rows are not \
                     ranked",
                    signature(&name, &ps)
                ),
            );
        }
        let mut seen = BTreeSet::new();
        for p in &ps {
            if !seen.insert(p.name.as_str()) {
                return self.error(
                    span,
                    format!("`let {name}` names its parameter `{}` twice", p.name),
                );
            }
        }
        // Every declaration of the name takes the same parameters (R-104).
        if let Some((_, first)) = self.function_def(scope, &name)
            && first != *n
            && node(&first, PARAMS).map(|p| p.text().to_string())
                != node(n, PARAMS).map(|p| p.text().to_string())
        {
            let at = crate::diag::at(self.span(&first)).unwrap_or_default();
            return self.error(
                span,
                format!(
                    "`let {name}` takes other parameters here than at {at}: one relation has \
                     one set of columns"
                ),
            );
        }
        // The parameters are bound around the body, as an enclosing
        // statement's variables are.
        let mut inner = outer.clone();
        let mut vars = Vec::new();
        for p in &ps {
            let mut v = capitalise(&p.name);
            while inner.vars.values().any(|w| *w == v) {
                v.push('_');
            }
            inner.vars.insert(p.name.clone(), v.clone());
            vars.push(var(&v));
        }
        let mut rc = self.rc(n, scope, &inner);
        // The parameters are bound by the demand of the literal that reads
        // the relation (`crate::demand`), first in the body, so a helper
        // of the clause (`not { }`) is bound by it too.
        let demand = Lit::Pos(atom_at(&crate::demand::of(&name), vars.clone(), span));
        let mut body = match node(n, BODY) {
            Some(b) => {
                let lits: Vec<SyntaxNode> = b.children().collect();
                self.lits_after(&mut rc, &lits, vec![demand])?
            }
            None => vec![demand],
        };
        let t = terms(n).next().ok_or(Skip)?;
        let declared = node(n, TYPE_EXPR).map(|t| self.type_expr(&t));
        let value = self.let_value(&mut rc, &t, &mut body)?;
        let value = match declared.as_ref().map(crate::types::of_expr) {
            Some(ty) => self.let_typed(scope, &name, &ty, &t, value)?,
            None => value,
        };
        let mut args = vars;
        args.push(value);
        let head = atom_at(&name, args, span);
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        let arity = head.args.len();
        // The demand is a relation the readers feed, declared so that a
        // module's copy names it its own with the let.
        let demand = Extern {
            pred: crate::demand::of(&name),
            arity: arity - 1,
            span,
        };
        Ok(vec![
            Stmt::Rule(RuleStmt { head, body }),
            Stmt::Extern(demand),
            Stmt::Mode(Extern {
                pred: name,
                arity,
                span,
            }),
        ])
    }

    /// The let with parameters a call names in `rc`, when it names one:
    /// its relation as the read writes it, its declaring scope and its
    /// statement. `f` in scope, `m.f` of a used module or of the module the
    /// read is in, `super.f`.
    fn called_function(
        &mut self,
        rc: &Rc,
        n: &SyntaxNode,
    ) -> L<Option<(String, usize, SyntaxNode)>> {
        let Some(c) = n.children().find_map(|c| Chain::of(&c)) else {
            return Ok(None);
        };
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) || rc.vars.contains_key(&c.head) {
            return Ok(None);
        }
        let fields = c.fields();
        let found = match fields.as_slice() {
            [f] => self
                .function_def(rc.scope, f)
                .map(|(s, def)| (self.relation_pred(rc.scope, rc.scope, f), s, def)),
            [m, f] if m == "super" => {
                let span = self.span_of(c.range);
                let (from, rest) = self.super_chain(rc, &c, span)?;
                self.function_def(from, &rest.head)
                    .map(|(s, def)| (self.relation_pred(rc.scope, from, &rest.head), s, def))
                    .filter(|_| rest.ops.is_empty() && rest.head == *f)
            }
            [m, f] => {
                if let Some(path) = self.use_in(rc.scope, m) {
                    let module = self.decls.modules.get(&path).map(|d| d.scope);
                    module.and_then(|s| {
                        let def = self.decls.scopes[s].functions.get(f.as_str())?;
                        Some((format!("{m}::{f}"), s, def.clone()))
                    })
                } else if let Some(from) = self.self_module(rc.scope, m) {
                    let def = self.decls.scopes[from].functions.get(f.as_str()).cloned();
                    def.map(|def| (self.relation_pred(rc.scope, from, f), from, def))
                } else {
                    None
                }
            }
            _ => None,
        };
        Ok(found)
    }

    /// A call of a let with parameters in a term: a fresh variable, the
    /// value column of the relation read with the arguments, once per rule
    /// (R-71's chain). `None` when the call names no let.
    pub(super) fn function_call(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> Option<L<Term>> {
        let (pred, scope, def) = match self.called_function(rc, n) {
            Ok(found) => found?,
            Err(e) => return Some(Err(e)),
        };
        Some(self.call_function(rc, n, &pred, scope, &def, pre))
    }

    fn call_function(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pred: &str,
        scope: usize,
        def: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let span = self.span(n);
        let name = word_text(def, 1);
        let ps = params(def).unwrap_or_default();
        let sig = signature(&name, &ps);
        let list = node(n, ARG_LIST);
        let positional: Vec<SyntaxNode> = list
            .as_ref()
            .map(|l| terms(l).collect())
            .unwrap_or_default();
        if positional.len() > ps.len() {
            return self.error(
                span,
                format!(
                    "`{name}` takes {} argument{}, not {}: `{sig}`",
                    ps.len(),
                    if ps.len() == 1 { "" } else { "s" },
                    positional.len()
                ),
            );
        }
        let mut given: Vec<Option<(Term, Span)>> = (0..ps.len()).map(|_| None).collect();
        for (i, t) in positional.iter().enumerate() {
            let term = self.bind(false, |l| l.term(rc, t, Pos::Content, pre))?;
            given[i] = Some((term, self.span(t)));
        }
        let named = list
            .iter()
            .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG));
        for a in named {
            let k = word_text(&a, 0);
            let Some(i) = ps.iter().position(|p| p.name == k) else {
                let names: Vec<&str> = ps.iter().map(|p| p.name.as_str()).collect();
                return self.error(
                    self.span(&a),
                    format!(
                        "`{name}` has no parameter `{k}`: its parameters are {}",
                        names.join(", ")
                    ),
                );
            };
            if given[i].is_some() {
                return self.error(self.span(&a), format!("`{name}`'s `{k}` is given twice"));
            }
            let t = terms(&a).next().ok_or(Skip)?;
            let term = self.bind(false, |l| l.term(rc, &t, Pos::Content, pre))?;
            given[i] = Some((term, self.span(&t)));
        }
        let mut args = Vec::new();
        for (p, g) in ps.iter().zip(given) {
            let (term, at) = match (g, &p.default) {
                (Some(g), _) => g,
                (None, Some(d)) => {
                    let mut at_def = Rc {
                        scope,
                        ..Rc::default()
                    };
                    (self.constant(&mut at_def, d)?, self.span(d))
                }
                (None, None) => {
                    return self.error(
                        span,
                        format!(
                            "`{name}`'s `{}` is left out, and has no default: give it (`{sig}`)",
                            p.name
                        ),
                    );
                }
            };
            let term = match &p.ty {
                Some(t) => {
                    let te = self.type_expr(t);
                    let ty = crate::types::of_expr(&te);
                    match crate::types::literal(&ty, term) {
                        Ok(t) => t,
                        Err(why) => {
                            return self.error(at, format!("`{name}`'s `{}` {why}", p.name));
                        }
                    }
                }
                None => term,
            };
            args.push(term);
        }
        let key = format!("fn {pred} {args:?}");
        if let Some(v) = rc.reads.get(&key) {
            return Ok(v.clone());
        }
        let v = var(&fresh(rc, &capitalise(&name)));
        args.push(v.clone());
        pre.push(Lit::Pos(atom_at(pred, args, span)));
        rc.reads.insert(key, v.clone());
        Ok(v)
    }

    /// `f` named and not called: a let with parameters is a relation whose
    /// arguments its reader binds, so it has no value alone and `in`
    /// enumerates nothing of it.
    pub(super) fn not_called(&mut self, span: Span, name: &str, def: &SyntaxNode) -> L<Res> {
        let ps = params(def).unwrap_or_default();
        self.error(
            span,
            format!(
                "`{name}` is a let with parameters: call it, `{}`",
                signature(name, &ps)
            ),
        )
    }

    /// `x in f`: the error that says why nothing enumerates `f`.
    pub(super) fn in_function(&mut self, span: Span, name: &str, def: &SyntaxNode) -> L<Lit> {
        let ps = params(def).unwrap_or_default();
        let d = Diagnostic::error(
            span,
            format!(
                "`in` over `{name}`: a let with parameters is a relation whose arguments must be \
                 bound, so nothing enumerates it"
            ),
        )
        .with_help(format!(
            "call it with its arguments, `{}`; `x in {}` reads a list it returns",
            signature(name, &ps),
            signature(name, &ps)
        ));
        self.diags.push(d);
        Err(Skip)
    }
}
