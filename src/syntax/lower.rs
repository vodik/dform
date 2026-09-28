//! From the lossless tree to `ast`: names become values, keypaths strings,
//! and each E §6 statement its `ast` form (`decl p/N` is an extern).
//! Statements with no semantics yet become `Stmt::Pending`, which lowering
//! rejects naming their ticket.

use super::SyntaxKind::{self, *};
use super::{SyntaxNode, SyntaxToken};
use crate::ast::{
    ApplyPolicy, Atom, AttrDecl, BindArg, Config, Constraint, Contributes, Decl, Export, Extern,
    FieldAssign, FieldOp, Grant, Import, InputDecl, Instance, Lit, Module, OutputDecl, Pending,
    PendingKind, PolicyPack, Program, Rank, Resource, RuleStmt, Settings, Span, Stmt, Term,
    TypeExpr, When,
};
use crate::diag::Diagnostic;
use crate::value::Value;
use std::collections::BTreeMap;

/// The one edition this compiler reads.
pub const EDITION_YEAR: i64 = 2026;

pub struct Lowerer {
    file: u32,
    pub diags: Vec<Diagnostic>,
}

/// An error already recorded in `diags`.
struct Skip;
type L<T> = Result<T, Skip>;

fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}

fn nodes(n: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    n.children()
}

fn node(n: &SyntaxNode, k: SyntaxKind) -> Option<SyntaxNode> {
    n.children().find(|c| c.kind() == k)
}

fn is_term(k: SyntaxKind) -> bool {
    matches!(
        k,
        LITERAL
            | NAME_REF
            | VAR_REF
            | PATH_LIT
            | FIELD_ACCESS
            | ADDR
            | QNAME_VAR
            | CALL
            | LIST
            | OBJECT
            | COMPREHENSION
            | PAREN
            | BIN_EXPR
            | UNARY_EXPR
    )
}

fn terms(n: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    n.children().filter(|c| is_term(c.kind()))
}

/// Names: identifiers, qualified names and keywords standing as names.
fn is_name_tok(k: SyntaxKind) -> bool {
    k.is_name() || k == QNAME
}

fn str_term(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

/// `PeerNetwork` -> `peer_network`: a decl variable as a record field.
fn snake(var: &str) -> String {
    let mut out = String::new();
    for (i, c) in var.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

impl Lowerer {
    pub fn new(file: u32) -> Self {
        Lowerer {
            file,
            diags: Vec::new(),
        }
    }

    fn span_of(&self, r: rowan::TextRange) -> Span {
        Span {
            file: self.file,
            start: r.start().into(),
            end: r.end().into(),
            origin: 0,
        }
    }

    fn span(&self, n: &SyntaxNode) -> Span {
        self.span_of(n.text_range())
    }

    fn error<T>(&mut self, span: Span, msg: impl Into<String>) -> L<T> {
        self.diags.push(Diagnostic::error(span, msg));
        Err(Skip)
    }

    fn not_yet(&mut self, n: &SyntaxNode, what: &str, ticket: Option<&str>) -> Skip {
        let note = match ticket {
            Some(t) => format!("it parses; its semantics land with {t}"),
            None => "it parses; no WORK.org ticket gives it semantics yet".to_string(),
        };
        let d =
            Diagnostic::error(self.span(n), format!("{what} is not yet supported")).with_note(note);
        self.diags.push(d);
        Skip
    }

    /// The whole file. With `require_edition`, the first statement must be
    /// `edition 2026.`.
    pub fn file(&mut self, root: &SyntaxNode, require_edition: bool) -> Program {
        let mut statements = Vec::new();
        let mut first = true;
        // A pragma that is only misplaced is one error, not two.
        let edition = nodes(root).any(|n| n.kind() == EDITION);
        for n in nodes(root) {
            match n.kind() {
                ERROR => {}
                EDITION => {
                    let year = tokens(&n).find(|t| t.kind() == INT);
                    let ok = year
                        .as_ref()
                        .is_some_and(|t| t.text() == EDITION_YEAR.to_string());
                    if !ok {
                        let d = Diagnostic::error(
                            self.span(&n),
                            format!(
                                "unknown edition {}: this compiler reads edition {EDITION_YEAR}",
                                year.map(|t| t.text().to_string()).unwrap_or_default()
                            ),
                        );
                        self.diags.push(d);
                    } else if !first {
                        let d = Diagnostic::error(
                            self.span(&n),
                            "the edition pragma must be the first statement of the file",
                        );
                        self.diags.push(d);
                    }
                }
                _ => {
                    if first && require_edition && !edition {
                        let at = self.span(&n);
                        let d = Diagnostic::error(
                            Span {
                                end: at.start,
                                ..at
                            },
                            format!("missing the edition pragma `edition {EDITION_YEAR}.`"),
                        )
                        .with_help(format!(
                            "every .df file starts with `edition {EDITION_YEAR}.` (docs/grammar.md)"
                        ));
                        self.diags.push(d);
                    }
                    if let Ok(s) = self.stmt(&n) {
                        statements.push(s);
                    }
                }
            }
            first = false;
        }
        if first && require_edition {
            let d = Diagnostic::error(
                Span {
                    file: self.file,
                    start: 0,
                    end: 0,
                    origin: 0,
                },
                format!("missing the edition pragma `edition {EDITION_YEAR}.`"),
            )
            .with_help(format!(
                "every .df file starts with `edition {EDITION_YEAR}.`"
            ));
            self.diags.push(d);
        }
        Program { statements }
    }

    fn stmts(&mut self, block: Option<SyntaxNode>) -> Vec<Stmt> {
        let Some(block) = block else {
            return Vec::new();
        };
        nodes(&block)
            .filter(|n| n.kind() != ERROR)
            .filter_map(|n| self.stmt(&n).ok())
            .collect()
    }

    fn name_text(&self, n: &SyntaxNode, skip: usize) -> String {
        tokens(n)
            .filter(|t| is_name_tok(t.kind()) || t.kind() == VAR)
            .nth(skip)
            .map(|t| t.text().to_string())
            .unwrap_or_default()
    }

    fn stmt(&mut self, n: &SyntaxNode) -> L<Stmt> {
        let span = self.span(n);
        let pending = |kind| Ok(Stmt::Pending(Pending { kind, span }));
        match n.kind() {
            IMPORT => {
                let path = tokens(n).find(|t| t.kind() == STRING).unwrap();
                let path = self.string(&path)?;
                if tokens(n).filter(|t| t.kind().is_name()).nth(1).is_some() {
                    return self.error(
                        span,
                        "`import ... as` is gone: an import is a file include; wrap reusable \
                         rules in a `module` and instantiate it (E DR-3)",
                    );
                }
                Ok(Stmt::Import(Import { path, span }))
            }
            PROVIDER | STACK => {
                let name = tokens(n)
                    .filter(|t| is_name_tok(t.kind()))
                    .nth(1)
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let config = self.assigns(node(n, BLOCK).as_ref())?;
                let config = config
                    .into_iter()
                    .map(|f| (f.key, f.value, f.span))
                    .collect();
                let c = Config { name, config, span };
                Ok(if n.kind() == PROVIDER {
                    Stmt::Provider(c)
                } else {
                    Stmt::Stack(c)
                })
            }
            INPUT => {
                let name = self.name_text(n, 1);
                let ty = self.type_expr(&node(n, TYPE_EXPR).unwrap());
                let default = terms(n).next().map(|t| self.term(&t)).transpose()?;
                let refinement = self.where_clause(n)?;
                Ok(Stmt::Input(InputDecl {
                    name,
                    ty,
                    default,
                    refinement,
                    span,
                }))
            }
            OUTPUT_DECL => {
                let name = self.name_text(n, 1);
                let ty = node(n, TYPE_EXPR).map(|t| self.type_expr(&t));
                let value = terms(n).next().map(|t| self.term(&t)).transpose()?;
                Ok(Stmt::Output(OutputDecl {
                    name,
                    ty,
                    value,
                    span,
                }))
            }
            EXPORT => {
                let pred = self.name_text(n, 1);
                let arity = self.arity(n)?;
                Ok(Stmt::Export(Export { pred, arity, span }))
            }
            CONTRIBUTES => {
                let toks: Vec<SyntaxToken> = tokens(n).skip(1).collect();
                let grant = if toks.len() > 1 {
                    let pat = |t: &SyntaxToken| (t.text() != "_").then(|| t.text().to_string());
                    Grant::Arg {
                        typ: pat(&toks[2]),
                        path: pat(&toks[4]).map(|p| p.trim_start_matches('.').to_string()),
                    }
                } else {
                    Grant::Pred(toks[0].text().to_string())
                };
                Ok(Stmt::Contributes(Contributes { grant, span }))
            }
            EXTERN => {
                let name = self.name_text(n, 1);
                let args = n
                    .children()
                    .filter(|c| c.kind() == BIND_ARG)
                    .map(|b| BindArg {
                        input: tokens(&b).next().is_some_and(|t| t.kind() == PLUS),
                        name: self.name_text(&b, 0),
                        ty: node(&b, TYPE_EXPR).map(|t| self.type_expr(&t)),
                    })
                    .collect();
                let persist = tokens(n).any(|t| t.kind() == PERSIST_KW);
                pending(PendingKind::ExternFn {
                    name,
                    args,
                    persist,
                })
            }
            TYPE_DECL => {
                let name = self.name_text(n, 1);
                let attrs = self.attr_decls(n)?;
                pending(PendingKind::TypeDecl { name, attrs })
            }
            DECL => self.decl(n, span),
            MODULE | POLICY | SCENARIO => {
                let name = self.name_text(n, 1);
                let body = self.stmts(node(n, STMT_BLOCK));
                Ok(match n.kind() {
                    MODULE => Stmt::Module(Module { name, body, span }),
                    POLICY => Stmt::PolicyPack(PolicyPack { name, body, span }),
                    _ => Stmt::Pending(Pending {
                        kind: PendingKind::Scenario { name, body },
                        span,
                    }),
                })
            }
            APPLY => Ok(Stmt::ApplyPolicy(ApplyPolicy {
                name: self.name_text(n, 1),
                span,
            })),
            INSTANCE => {
                let module = self.name_text(n, 1);
                let name = self.name_text(n, 2);
                let mut inputs = Vec::new();
                for f in self.assigns(node(n, BLOCK).as_ref())? {
                    if matches!(f.op, FieldOp::Add) {
                        return self.error(f.span, "an instance input is set with `=`, not `+=`");
                    }
                    if f.rank.is_some() {
                        return self.error(f.span, "an instance input takes no rank");
                    }
                    inputs.push((f.key, f.value, f.span));
                }
                let body = self.opt_body(n)?;
                Ok(Stmt::Instance(Instance {
                    module,
                    name,
                    inputs,
                    body,
                    span,
                }))
            }
            WHEN => {
                let guard_node = n.children().find(|c| c.kind() != STMT_BLOCK).ok_or(Skip)?;
                let mut guard = self.lit(&guard_node)?;
                if guard.len() != 1 {
                    return self.error(self.span(&guard_node), "a when guard is one literal");
                }
                let body = self.stmts(node(n, STMT_BLOCK));
                Ok(Stmt::When(When {
                    guard: guard.remove(0),
                    body,
                    span,
                }))
            }
            RESOURCE => {
                let mut names = tokens(n).skip(1);
                let typ = names.next().ok_or(Skip)?;
                let name = names.next().ok_or(Skip)?;
                Ok(Stmt::Resource(Resource {
                    typ: str_term(typ.text()),
                    name: self.name_term(&name),
                    rank: self.rank_tok(n)?,
                    fields: self.assigns(node(n, BLOCK).as_ref())?,
                    body: self.opt_body(n)?,
                    span,
                }))
            }
            SETTINGS => {
                let env = tokens(n).nth(1).ok_or(Skip)?;
                Ok(Stmt::Settings(Settings {
                    env: self.name_term(&env),
                    rank: self.rank_tok(n)?,
                    fields: self.assigns(node(n, BLOCK).as_ref())?,
                    body: self.opt_body(n)?,
                    span,
                }))
            }
            RULE | FACT => self.rule(n),
            k => self.error(span, format!("unexpected {k:?}")),
        }
    }

    /// `decl p/N` is today's extern; `decl p(V: type, ...)` a record
    /// declaration; the other two forms are pending.
    fn decl(&mut self, n: &SyntaxNode, span: Span) -> L<Stmt> {
        let toks: Vec<SyntaxToken> = tokens(n).collect();
        if toks.get(1).is_some_and(|t| t.kind() == TYPE_KW) {
            return Ok(Stmt::Pending(Pending {
                kind: PendingKind::DeclOpenType {
                    name: toks[2].text().to_string(),
                },
                span,
            }));
        }
        let pred = toks[1].text().to_string();
        if toks.get(2).is_some_and(|t| t.kind() == SLASH) {
            let arity = self.arity(n)?;
            if toks.last().is_some_and(|t| t.text() == "mixed") {
                return Ok(Stmt::Pending(Pending {
                    kind: PendingKind::DeclMixed { pred, arity },
                    span,
                }));
            }
            return Ok(Stmt::Extern(Extern { pred, arity, span }));
        }
        let fields = n
            .children()
            .filter(|c| c.kind() == BIND_ARG)
            .map(|b| snake(&self.name_text(&b, 0)))
            .collect();
        Ok(Stmt::Decl(Decl { pred, fields, span }))
    }

    fn arity(&mut self, n: &SyntaxNode) -> L<usize> {
        let t = tokens(n).find(|t| t.kind() == INT).ok_or(Skip)?;
        match t.text().parse() {
            Ok(a) => Ok(a),
            Err(_) => self.error(self.span_of(t.text_range()), "arity out of range"),
        }
    }

    fn rank_tok(&mut self, n: &SyntaxNode) -> L<Option<Rank>> {
        let Some(t) = tokens(n).find(|t| t.kind() == RANK) else {
            return Ok(None);
        };
        match t.text() {
            "@default" => Ok(Some(Rank::Default)),
            "@override" => Ok(Some(Rank::Override)),
            other => self.error(
                self.span_of(t.text_range()),
                format!("unknown rank `{other}`: a rank is `@default` or `@override`"),
            ),
        }
    }

    fn name_term(&self, t: &SyntaxToken) -> Term {
        match t.kind() {
            VAR if t.text() == "_" => Term::Wildcard,
            VAR => Term::Var(t.text().to_string()),
            _ => str_term(t.text()),
        }
    }

    fn opt_body(&mut self, n: &SyntaxNode) -> L<Option<Vec<Lit>>> {
        node(n, BODY).map(|b| self.body(&b)).transpose()
    }

    fn where_clause(&mut self, n: &SyntaxNode) -> L<Vec<Lit>> {
        match node(n, WHERE_CLAUSE).and_then(|w| node(&w, BODY)) {
            Some(b) => self.body(&b),
            None => Ok(Vec::new()),
        }
    }

    fn attr_decls(&mut self, n: &SyntaxNode) -> L<Vec<AttrDecl>> {
        let mut out = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let path = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let ty = node(&a, TYPE_EXPR).map(|t| self.type_expr(&t));
            let flags = tokens(&a)
                .filter(|t| t.kind() == IDENT)
                .map(|t| t.text().to_string())
                .collect();
            let refinement = self.where_clause(&a)?;
            let children = self.attr_decls(&a)?;
            out.push(AttrDecl {
                path,
                ty,
                flags,
                refinement,
                children,
            });
        }
        Ok(out)
    }

    fn type_expr(&mut self, n: &SyntaxNode) -> TypeExpr {
        let first = tokens(n).next();
        match first.as_ref().map(|t| t.kind()) {
            Some(STRING) => {
                let t = first.unwrap();
                TypeExpr::Str(self.string(&t).unwrap_or_default())
            }
            Some(L_BRACE) => TypeExpr::Object(
                n.children()
                    .filter(|c| c.kind() == OBJECT_FIELD)
                    .map(|f| {
                        let key = tokens(&f).next().map(|t| t.text().to_string());
                        let ty = node(&f, TYPE_EXPR).map(|t| self.type_expr(&t));
                        (
                            key.unwrap_or_default(),
                            ty.unwrap_or(TypeExpr::Name(String::new())),
                        )
                    })
                    .collect(),
            ),
            _ => {
                let name = first.map(|t| t.text().to_string()).unwrap_or_default();
                let args: Vec<TypeExpr> = n
                    .children()
                    .filter(|c| c.kind() == TYPE_EXPR)
                    .map(|c| self.type_expr(&c))
                    .collect();
                if args.is_empty() {
                    TypeExpr::Name(name)
                } else {
                    TypeExpr::Apply(name, args)
                }
            }
        }
    }

    /// The assignments of a `{ ... }` block.
    fn assigns(&mut self, block: Option<&SyntaxNode>) -> L<Vec<FieldAssign>> {
        let Some(block) = block else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let op = if tokens(&a).any(|t| t.kind() == PLUS_EQ) {
                FieldOp::Add
            } else {
                FieldOp::Assign
            };
            let value = self.term(&terms(&a).next().ok_or(Skip)?)?;
            out.push(FieldAssign {
                key,
                op,
                value,
                rank: self.rank_tok(&a)?,
                span: self.span(&a),
            });
        }
        Ok(out)
    }

    /// A block path or keypath as today's dotted string: `a.b`, `a[0].b`,
    /// quoted segments unquoted.
    fn block_path(&mut self, n: &SyntaxNode) -> L<String> {
        let mut out = String::new();
        for t in tokens(n) {
            match t.kind() {
                DOT => out.push('.'),
                L_BRACKET | R_BRACKET | INT => out.push_str(t.text()),
                STRING => out.push_str(&self.segment(&t)?),
                PATH => out.push_str(&self.keypath(&t)?),
                _ => out.push_str(t.text()),
            }
        }
        Ok(out)
    }

    fn segment(&mut self, t: &SyntaxToken) -> L<String> {
        let s = self.string(t)?;
        if s.contains(['.', '[', ']']) {
            return self.error(
                self.span_of(t.text_range()),
                format!(
                    "the key {s:?} holds `.`, `[` or `]`, which today's dotted paths cannot carry"
                ),
            );
        }
        Ok(s)
    }

    /// `.a."b-c"[0]` as `.a.b-c[0]` (leading dot kept): quoted segments
    /// unquoted.
    fn keypath(&mut self, t: &SyntaxToken) -> L<String> {
        let text = t.text();
        let mut out = String::new();
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            if c == '"' {
                let mut end = 1;
                let bytes = rest.as_bytes();
                while bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                let lit = &rest[..=end];
                let s = unescape(lit).map_err(|e| {
                    self.diags
                        .push(Diagnostic::error(self.span_of(t.text_range()), e));
                    Skip
                })?;
                if s.contains(['.', '[', ']']) {
                    return self.error(
                        self.span_of(t.text_range()),
                        format!(
                            "the key {s:?} holds `.`, `[` or `]`, which today's dotted paths cannot carry"
                        ),
                    );
                }
                out.push_str(&s);
                rest = &rest[end + 1..];
            } else {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        Ok(out)
    }

    fn string(&mut self, t: &SyntaxToken) -> L<String> {
        match unescape(t.text()) {
            Ok(s) => Ok(s),
            Err(e) => self.error(self.span_of(t.text_range()), e),
        }
    }

    // --- rules ------------------------------------------------------------

    fn rule(&mut self, n: &SyntaxNode) -> L<Stmt> {
        let span = self.span(n);
        let head_node = n
            .children()
            .find(|c| matches!(c.kind(), ATOM | RECORD_ATOM))
            .ok_or(Skip)?;
        let mut head = self.atom(&head_node)?;
        let body = match node(n, BODY) {
            Some(b) => Some(self.body(&b)?),
            None => None,
        };
        if let Some(rank) = self.rank_tok(n)? {
            if head.pred != "arg" || head.args.len() != 4 || head.record.is_some() {
                return self.error(
                    span,
                    "a rank applies to an `arg(T, A, Path, Value)` head only",
                );
            }
            head.args.push(str_term(rank.name()));
        }
        if head.pred == "constraint" {
            let [Term::Val(Value::Str(message))] = head.args.as_slice() else {
                return self.error(span, "a constraint head is `constraint(\"message\")`");
            };
            let Some(body) = body else {
                return self.error(
                    span,
                    "a constraint needs a body: `constraint(\"...\") :- ...`",
                );
            };
            return Ok(Stmt::Constraint(Constraint {
                message: message.clone(),
                body,
                span,
            }));
        }
        Ok(match body {
            Some(body) => Stmt::Rule(RuleStmt { head, body }),
            None => Stmt::Fact(head),
        })
    }

    fn body(&mut self, n: &SyntaxNode) -> L<Vec<Lit>> {
        let mut out = Vec::new();
        let mut failed = false;
        for l in n.children() {
            match self.lit(&l) {
                Ok(ls) => out.extend(ls),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    fn lit(&mut self, n: &SyntaxNode) -> L<Vec<Lit>> {
        let atom_of = |s: &mut Self| -> L<Atom> {
            let a = n
                .children()
                .find(|c| matches!(c.kind(), ATOM | RECORD_ATOM))
                .ok_or(Skip)?;
            s.atom(&a)
        };
        match n.kind() {
            LIT_ATOM => Ok(vec![Lit::Pos(atom_of(self)?)]),
            LIT_NOT => Ok(vec![Lit::Not(atom_of(self)?)]),
            LIT_NOT_EXISTS => Err(self.not_yet(n, "`not exists(...)`", None)),
            LIT_IN | LIT_NOT_IN => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let item = self.term(&ts[0])?;
                let list = self.term(&ts[1])?;
                let member = Atom {
                    pred: "member".to_string(),
                    args: vec![list, item],
                    record: None,
                    span: self.span(n),
                };
                Ok(vec![if n.kind() == LIT_IN {
                    Lit::Pos(member)
                } else {
                    Lit::Not(member)
                }])
            }
            LIT_CMP => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
                let mut out = Vec::new();
                for (i, op) in ops.iter().enumerate() {
                    let a = self.term(&ts[i])?;
                    let b = self.term(&ts[i + 1])?;
                    out.push(match op {
                        EQ | EQ2 => Lit::Eq(a, b),
                        NEQ => Lit::Neq(a, b),
                        LT => Lit::Lt(a, b),
                        LE => Lit::Le(a, b),
                        GT => Lit::Gt(a, b),
                        _ => Lit::Ge(a, b),
                    });
                }
                Ok(out)
            }
            k => self.error(self.span(n), format!("unexpected {k:?} in a body")),
        }
    }

    fn atom(&mut self, n: &SyntaxNode) -> L<Atom> {
        let pred = tokens(n).next().ok_or(Skip)?.text().to_string();
        if n.kind() == RECORD_ATOM {
            let mut fields = BTreeMap::new();
            for f in n.children().filter(|c| c.kind() == RECORD_FIELD) {
                let key = tokens(&f).next().ok_or(Skip)?.text().to_string();
                let value = self.term(&terms(&f).next().ok_or(Skip)?)?;
                if fields.insert(key.clone(), value).is_some() {
                    return self.error(self.span(&f), format!("field `{key}` given twice"));
                }
            }
            return Ok(Atom {
                pred,
                args: Vec::new(),
                record: Some(fields),
                span: self.span(n),
            });
        }
        let args = self.args(n)?;
        Ok(Atom {
            pred,
            args,
            record: None,
            span: self.span(n),
        })
    }

    fn args(&mut self, n: &SyntaxNode) -> L<Vec<Term>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        terms(&list).map(|t| self.term(&t)).collect()
    }

    // --- terms ------------------------------------------------------------

    fn term(&mut self, n: &SyntaxNode) -> L<Term> {
        let first = || tokens(n).next().unwrap();
        match n.kind() {
            LITERAL => {
                let t = first();
                match t.kind() {
                    INT => match t.text().parse::<i64>() {
                        Ok(i) => Ok(Term::Val(Value::Int(i))),
                        Err(_) => self.error(self.span(n), "integer out of range"),
                    },
                    STRING => Ok(Term::Val(Value::Str(self.string(&t)?))),
                    TRUE_KW => Ok(Term::Val(Value::Bool(true))),
                    FALSE_KW => Ok(Term::Val(Value::Bool(false))),
                    _ => Err(self.not_yet(n, "the `null` literal (E DR-6)", None)),
                }
            }
            VAR_REF => Ok(self.name_term(&first())),
            NAME_REF => Ok(str_term(first().text())),
            PATH_LIT => {
                let p = self.keypath(&first())?;
                Ok(str_term(&p[1..]))
            }
            FIELD_ACCESS => {
                let text = first().text().to_string();
                let (var, path) = text.split_once('.').unwrap();
                Ok(Term::Func {
                    name: "__path".to_string(),
                    args: vec![Term::Var(var.to_string()), str_term(path)],
                })
            }
            ADDR => {
                let toks: Vec<SyntaxToken> = tokens(n).collect();
                Ok(Term::Func {
                    name: "scoped".to_string(),
                    args: vec![str_term(toks[0].text()), str_term(toks[2].text())],
                })
            }
            // `network.I`: the instance scope named by a bound variable
            // (E §7.1's `output(network.IA, vpc, A)`).
            QNAME_VAR => {
                let toks: Vec<SyntaxToken> = tokens(n).collect();
                Ok(Term::Func {
                    name: "format".into(),
                    args: vec![
                        str_term(&format!("{}.%s", toks[0].text())),
                        Term::Var(toks[2].text().to_string()),
                    ],
                })
            }
            CALL => Ok(Term::Func {
                name: first().text().to_string(),
                args: self.args(n)?,
            }),
            LIST => Ok(Term::List(
                terms(n).map(|t| self.term(&t)).collect::<L<Vec<_>>>()?,
            )),
            OBJECT => {
                let mut m = BTreeMap::new();
                for f in n.children().filter(|c| c.kind() == OBJECT_FIELD) {
                    let k = tokens(&f).next().ok_or(Skip)?;
                    let key = if k.kind() == STRING {
                        self.string(&k)?
                    } else {
                        k.text().to_string()
                    };
                    let v = self.term(&terms(&f).next().ok_or(Skip)?)?;
                    if m.insert(key.clone(), v).is_some() {
                        return self.error(self.span(&f), format!("key `{key}` given twice"));
                    }
                }
                Ok(Term::Obj(m))
            }
            COMPREHENSION => {
                if tokens(n).any(|t| t.text() == "ordered") {
                    return Err(self.not_yet(n, "an ordered comprehension", None));
                }
                let item = self.term(&terms(n).next().ok_or(Skip)?)?;
                let body = self.body(&node(n, BODY).ok_or(Skip)?)?;
                Ok(Term::ListComp {
                    item: Box::new(item),
                    body,
                })
            }
            PAREN => self.term(&terms(n).next().ok_or(Skip)?),
            BIN_EXPR => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let op = tokens(n).next().ok_or(Skip)?;
                // `us-east` or `a/b` with no spaces: a name, not arithmetic.
                if matches!(op.kind(), MINUS | SLASH)
                    && ts.iter().all(|t| t.kind() == NAME_REF)
                    && ts[0].text_range().end() == op.text_range().start()
                    && op.text_range().end() == ts[1].text_range().start()
                {
                    let d = Diagnostic::error(
                        self.span(n),
                        format!("`{}` is arithmetic on two symbols", n.text()),
                    )
                    .with_help(format!(
                        "`-` and `/` are always operators; a name with them is a string: \"{}\"",
                        n.text()
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                let name = match op.kind() {
                    PLUS => "add",
                    MINUS => "sub",
                    STAR => "mul",
                    SLASH => "div",
                    _ => "mod",
                };
                Ok(Term::Func {
                    name: name.to_string(),
                    args: vec![self.term(&ts[0])?, self.term(&ts[1])?],
                })
            }
            UNARY_EXPR => {
                let inner = terms(n).next().ok_or(Skip)?;
                Ok(match self.term(&inner)? {
                    Term::Val(Value::Int(i)) if inner.kind() == LITERAL => {
                        Term::Val(Value::Int(-i))
                    }
                    t => Term::Func {
                        name: "sub".to_string(),
                        args: vec![Term::Val(Value::Int(0)), t],
                    },
                })
            }
            k => self.error(self.span(n), format!("unexpected {k:?} as a term")),
        }
    }
}

/// A string literal's value: escapes `\"` `\\` `\n` `\t` `\u{...}`.
pub fn unescape(lit: &str) -> Result<String, String> {
    let inner = &lit[1..lit.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                let rest: String = chars.by_ref().take_while(|c| *c != '}').collect();
                let hex = rest.strip_prefix('{').ok_or("expected `\\u{...}`")?;
                let c = u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("bad unicode escape `\\u{{{hex}}}`"))?;
                out.push(c);
            }
            Some(o) => return Err(format!("unknown escape `\\{o}`")),
            None => return Err("a string ends in `\\`".to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::ast::{Lit, Stmt, Term};
    use crate::parser::parse_program;
    use crate::partition::fmt_rule;

    /// The one rule of `src`, printed as the strata report prints it.
    fn rule(src: &str) -> String {
        match parse_program(src).unwrap().statements.as_slice() {
            [Stmt::Rule(r)] => fmt_rule(r),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn names_paths_and_strings_are_one_value() {
        assert_eq!(
            rule("p(a.b, .c.d, \"e-f\", .\"g-h\".i[0]) :- q(X)."),
            "p(\"a.b\", \"c.d\", \"e-f\", \"g-h.i[0]\") :- q(X)"
        );
    }

    #[test]
    fn fields_addresses_and_operators_lower_to_builtins() {
        assert_eq!(
            rule("p(V, A) :- q(X), V = X.a.b, A = ref(net.vpc, m.i/vpc, .id)."),
            "p(V, A) :- q(X), V = __path(X, \"a.b\"), A = ref(\"net.vpc\", scoped(\"m.i\", \"vpc\"), \"id\")"
        );
        assert_eq!(
            rule("p(Y) :- q(X), Y = -X + 2 * -3, X in [1], X not in [2]."),
            "p(Y) :- q(X), Y = add(sub(0, X), mul(2, -3)), member([1], X), not member([2], X)"
        );
        assert_eq!(
            rule("p(X) :- q(X), 1 <= X <= 3."),
            "p(X) :- q(X), 1 <= X, X <= 3"
        );
    }

    #[test]
    fn a_rank_on_an_arg_head_is_its_fifth_argument() {
        assert_eq!(
            rule("arg(T, A, .x, 1) @override :- want(T, A)."),
            "arg(T, A, \"x\", 1, \"override\") :- want(T, A)"
        );
    }

    #[test]
    fn statements_lower_to_todays_equivalents() {
        let p = parse_program(
            "module m { p(a). }. instance m i { k = 1 }. policy q { r(b). }. apply q.
             decl ext/2. decl rec(FirstName: string, B: int).
             constraint(\"no\") :- p(z). when env(prod) { s(c). }.",
        )
        .unwrap();
        let kinds: Vec<String> = p
            .statements
            .iter()
            .map(|s| match s {
                Stmt::Module(d) => format!("def {}", d.name),
                Stmt::Instance(u) => format!("use {} {} {:?}", u.module, u.name, u.inputs[0].0),
                Stmt::PolicyPack(p) => format!("pack {}", p.name),
                Stmt::ApplyPolicy(a) => format!("apply {}", a.name),
                Stmt::Extern(e) => format!("extern {}/{}", e.pred, e.arity),
                Stmt::Decl(d) => format!("decl {} {:?}", d.pred, d.fields),
                Stmt::Constraint(c) => format!("constraint {}", c.message),
                Stmt::When(w) => format!("when {}", matches!(w.guard, Lit::Pos(_))),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "def m",
                "use m i \"k\"",
                "pack q",
                "apply q",
                "extern ext/2",
                "decl rec [\"first_name\", \"b\"]",
                "constraint no",
                "when true",
            ]
        );
    }

    #[test]
    fn resource_names_are_symbols_or_variables() {
        let p = parse_program("resource net.vpc main {}. resource net.vpc N {} :- n(N).").unwrap();
        let names: Vec<&Term> = p
            .statements
            .iter()
            .map(|s| match s {
                Stmt::Resource(r) => &r.name,
                _ => unreachable!(),
            })
            .collect();
        assert!(matches!(names[0], Term::Val(_)));
        assert!(matches!(names[1], Term::Var(v) if v == "N"));
    }
}
