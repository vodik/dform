//! From the lossless tree to `ast`: name resolution (docs/grammar.md
//! "Names") and lowering to today's AST, so `transform` and everything after
//! it see exactly what the core has always seen.
//!
//! Resolution is program-wide: a module, a resource or an input declared in
//! one file is named in another. `lower` takes every file of the program
//! (each with the files its imports resolved to), collects the
//! declarations, then lowers the entry files, inlining each import where it
//! stands. A chain (`a.b[e]/c.d`) is resolved in this order: a `let`
//! alias, a value name (an input or a value rule), `settings`, `world`, a
//! resource, a module instance, a relation or extern lookup, a type; a bare
//! name that is none of these is a variable, and may not take the name of a
//! resource, a module or a type namespace in scope.
//!
//! Where a read lands: in a rule body, just before the literal that holds
//! it; in a head, a field or an instance input, appended to the body (the
//! block's one shared body: a read in any field gates the whole block).

use super::SyntaxKind::{self, *};
use super::parse;
use super::{SyntaxNode, SyntaxToken};
use crate::ast::{
    ApplyPolicy, Atom, AttrDecl, BindArg, Config, Constraint, Contributes, Decl, Export, Extern,
    ExternFn, FieldAssign, FieldOp, Grant, Import, InputDecl, InputRelation, Instance, Lit, Module,
    OutputDecl, Pending, PendingKind, PolicyPack, Program, Rank, Resource, RuleStmt, Scenario,
    Settings, Span, Stmt, Term, TypeExpr, When,
};
use crate::diag::Diagnostic;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The one edition this compiler reads.
pub const EDITION_YEAR: i64 = 2026;

/// One parsed file of a program.
pub struct Unit {
    /// `diag` source id.
    pub file: u32,
    pub root: SyntaxNode,
    /// The unit each `import` statement of the file loads, in order; `None`
    /// for a file already loaded. `None` for the whole list: the imports
    /// stay `Stmt::Import` statements (a file parsed on its own).
    pub imports: Option<Vec<Option<usize>>>,
}

/// Lower `entries` (and, through their imports, the rest of `units`).
/// `require_edition`: every file must start with the edition pragma.
/// `lenient`: every name that is not a variable is its own text (a
/// refinement or a constraint written as text).
pub fn lower(
    units: &[Unit],
    entries: &[usize],
    require_edition: bool,
    lenient: bool,
) -> Result<Program, Vec<Diagnostic>> {
    let mut l = Lowerer::new(units, lenient);
    let mut statements = Vec::new();
    for &e in entries {
        statements.extend(l.unit(e, require_edition));
    }
    if l.diags.is_empty() {
        Ok(Program { statements })
    } else {
        Err(l.diags)
    }
}

// --- declarations ---------------------------------------------------------

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    /// Inputs and value rules: names read bare.
    values: BTreeSet<String>,
    /// Resources with a static name: name -> the types declaring it.
    resources: BTreeMap<String, Vec<String>>,
    /// `let` aliases: name -> the chain it names.
    lets: BTreeMap<String, SyntaxNode>,
    /// A module's `output k: T`: `Some(T)` when T is a resource type.
    outputs: BTreeMap<String, Option<String>>,
}

#[derive(Default)]
struct Decls {
    scopes: Vec<Scope>,
    /// The scope of each file's top level, by source id.
    files: BTreeMap<u32, usize>,
    /// The scope of a module, policy or scenario block, by (file, offset).
    blocks: BTreeMap<(u32, u32), usize>,
    modules: BTreeMap<String, usize>,
    instances: BTreeMap<String, BTreeSet<String>>,
    /// Resource types: every resource header's, every `type` block's.
    types: BTreeSet<String>,
    /// First segments of the types: no variable may take one.
    namespaces: BTreeSet<String>,
    /// Relations: rule and fact heads, `decl`s, input relations.
    relations: BTreeSet<String>,
    /// `extern` relations: their columns, `(input, name)`.
    externs: BTreeMap<String, Vec<(bool, String)>>,
}

const PROGRAM: usize = 0;

fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}

fn node(n: &SyntaxNode, k: SyntaxKind) -> Option<SyntaxNode> {
    n.children().find(|c| c.kind() == k)
}

fn is_term(k: SyntaxKind) -> bool {
    matches!(
        k,
        LITERAL
            | PATH_LIT
            | CHAIN
            | CALL
            | RECORD_ATOM
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

fn is_word(k: SyntaxKind) -> bool {
    k == IDENT || k.is_keyword()
}

/// The first word token of a node after `skip` others.
fn word_text(n: &SyntaxNode, skip: usize) -> String {
    tokens(n)
        .filter(|t| is_word(t.kind()))
        .nth(skip)
        .map(|t| t.text().to_string())
        .unwrap_or_default()
}

/// `a.b.c` from the leading words and dots of a node: a type, an extern,
/// a stack name.
fn dotted_text(n: &SyntaxNode, skip_words: usize) -> String {
    let mut out = String::new();
    let mut words = 0;
    let mut started = false;
    for t in tokens(n) {
        if is_word(t.kind()) {
            if words >= skip_words {
                if started && !out.ends_with('.') {
                    break;
                }
                out.push_str(t.text());
                started = true;
            }
            words += 1;
        } else if t.kind() == DOT && started {
            out.push('.');
        } else if started {
            break;
        }
    }
    out
}

fn str_term(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

fn var(s: &str) -> Term {
    Term::Var(s.to_string())
}

fn func(name: &str, args: Vec<Term>) -> Term {
    Term::Func {
        name: name.to_string(),
        args,
    }
}

/// `vpc_net` -> `VpcNet`, `_c` -> `_C`: a variable as the core prints it.
pub fn capitalise(s: &str) -> String {
    let lead = s.len() - s.trim_start_matches('_').len();
    let mut out = "_".repeat(lead);
    for part in s[lead..].split('_') {
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

/// A part of a chain after its head.
#[derive(Clone)]
enum Op {
    Field(String),
    Index(Vec<SyntaxNode>, rowan::TextRange),
    Slash(String, rowan::TextRange),
}

/// A chain: its head word and the parts after it.
#[derive(Clone)]
struct Chain {
    head: String,
    head_kind: SyntaxKind,
    range: rowan::TextRange,
    ops: Vec<Op>,
}

impl Chain {
    fn of(n: &SyntaxNode) -> Option<Chain> {
        if n.kind() != CHAIN {
            return None;
        }
        let mut it = n
            .children_with_tokens()
            .filter(|e| e.as_token().is_none_or(|t| !t.kind().is_trivia()));
        let head = it.next()?.into_token()?;
        let mut ops = Vec::new();
        let mut pending: Option<SyntaxKind> = None;
        for e in it {
            match e {
                rowan::NodeOrToken::Node(ix) if ix.kind() == INDEX => {
                    ops.push(Op::Index(terms(&ix).collect(), ix.text_range()));
                }
                rowan::NodeOrToken::Node(_) => {}
                rowan::NodeOrToken::Token(t) => match (pending, t.kind()) {
                    (None, DOT | SLASH) => pending = Some(t.kind()),
                    (Some(DOT), STRING) => {
                        let s = crate::syntax::resolve::unescape(t.text()).unwrap_or_default();
                        ops.push(Op::Field(s));
                        pending = None;
                    }
                    (Some(DOT), _) => {
                        ops.push(Op::Field(t.text().to_string()));
                        pending = None;
                    }
                    (Some(SLASH), _) => {
                        ops.push(Op::Slash(t.text().to_string(), t.text_range()));
                        pending = None;
                    }
                    _ => {}
                },
            }
        }
        Some(Chain {
            head: head.text().to_string(),
            head_kind: head.kind(),
            range: n.text_range(),
            ops,
        })
    }

    /// The leading words: the head and the `.name` parts right after it.
    fn fields(&self) -> Vec<String> {
        let mut out = vec![self.head.clone()];
        for op in &self.ops {
            match op {
                Op::Field(f) => out.push(f.clone()),
                _ => break,
            }
        }
        out
    }

    fn is_bare(&self) -> bool {
        self.ops.is_empty()
    }
}

/// A resolved path segment.
#[derive(Clone, Debug)]
enum Seg {
    F(String),
    I(Term),
}

/// What a chain denotes.
#[derive(Clone, Debug)]
enum Res {
    /// A value: a variable, a constant, an instance scope.
    Val(Term),
    /// A resource reference and a path into it.
    Ref {
        typ: Term,
        addr: Term,
        path: Vec<Seg>,
    },
    /// A settings row and a path (always read, by its full key).
    Settings { addr: Term, path: Vec<Seg> },
    /// An instance output.
    Output {
        inst: Term,
        key: String,
        path: Vec<Seg>,
    },
    /// A live object: `world.T[e].path`.
    World {
        typ: String,
        addr: Term,
        path: String,
    },
    /// A value name (input or value rule), `k(V)`.
    Value { pred: String, path: Vec<Seg> },
    /// A type's name.
    Type(String),
    /// `p[a, b]`: the relation `p(a, b, V)`, or an extern with its one output.
    Lookup {
        pred: String,
        args: Vec<Term>,
        out: usize,
        path: Vec<Seg>,
    },
    /// A variable with fields or indexes.
    Var { var: Term, path: Vec<Seg> },
}

/// Where a term stands: a whole value (a field, a head argument, an
/// element of a list or object there) makes a dot a reference; anywhere
/// else it is a read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    Whole,
    Content,
}

/// Per-statement state: the variables and what the rule has read.
#[derive(Default, Clone)]
struct Rc {
    scope: usize,
    /// Source name -> lowered name.
    vars: BTreeMap<String, String>,
    /// Where each variable is first written.
    first: BTreeMap<String, Span>,
    /// Lowered names in use or reserved.
    reserved: BTreeSet<String>,
    /// Source variable -> its static resource type.
    types: BTreeMap<String, Term>,
    /// Names that stand alone somewhere in the statement: variables.
    candidates: BTreeSet<String>,
    /// Value name -> the variable its read binds (one read per rule).
    values: BTreeMap<String, String>,
    /// Reads already hoisted, by what they read.
    reads: BTreeMap<String, Term>,
    /// Lowered names an enclosing `for`/`when` binds.
    outer: BTreeSet<String>,
    /// Source names written in a binding position somewhere: an argument
    /// of a relation, the left of `=` or `in`, a `some` binder, a pattern.
    binders: BTreeSet<String>,
}

/// An error already recorded in `diags`.
struct Skip;
type L<T> = Result<T, Skip>;

pub struct Lowerer<'u> {
    units: &'u [Unit],
    decls: Decls,
    file: u32,
    /// Added to every range: the offset of an interpolation hole.
    offset: u32,
    pub diags: Vec<Diagnostic>,
    lenient: bool,
    /// `__neg_N` helpers generated so far.
    negs: usize,
    /// Helper rules of the statement being lowered.
    helpers: Vec<Stmt>,
    /// Whether the term being lowered is in a binding position.
    binding: bool,
}

impl<'u> Lowerer<'u> {
    fn new(units: &'u [Unit], lenient: bool) -> Self {
        let mut l = Lowerer {
            units,
            decls: Decls {
                scopes: vec![Scope::default()],
                ..Decls::default()
            },
            file: 0,
            offset: 0,
            diags: Vec::new(),
            lenient,
            negs: 0,
            helpers: Vec::new(),
            binding: false,
        };
        for u in units {
            let scope = l.new_scope(PROGRAM);
            l.decls.files.insert(u.file, scope);
            l.collect(u.file, &u.root, scope, PROGRAM);
        }
        l.decls.namespaces = l
            .decls
            .types
            .iter()
            .map(|t| t.split('.').next().unwrap_or(t).to_string())
            .collect();
        l
    }

    fn new_scope(&mut self, parent: usize) -> usize {
        self.decls.scopes.push(Scope {
            parent: Some(parent),
            ..Scope::default()
        });
        self.decls.scopes.len() - 1
    }

    /// Record the declarations of a statement list. `lets` is the scope
    /// `let`s land in, `decl` the one everything else does.
    fn collect(&mut self, file: u32, parent: &SyntaxNode, lets: usize, decl: usize) {
        for n in parent.children() {
            match n.kind() {
                INPUT => {
                    let name = word_text(&n, 1);
                    self.decls.scopes[decl].values.insert(name);
                }
                VALUE_RULE => {
                    let name = word_text(&n, 0);
                    self.decls.relations.insert(name.clone());
                    self.decls.scopes[decl].values.insert(name);
                }
                INPUT_RELATION => {
                    self.decls.relations.insert(word_text(&n, 2));
                }
                LET => {
                    if let Some(t) = terms(&n).next() {
                        self.decls.scopes[lets].lets.insert(word_text(&n, 1), t);
                    }
                }
                OUTPUT_DECL => {
                    if let Some(t) = node(&n, TYPE_EXPR) {
                        let ty = self.resource_type(&t);
                        self.decls.scopes[decl].outputs.insert(word_text(&n, 1), ty);
                    }
                }
                EXTERN => {
                    let name = dotted_text(&n, 1);
                    let cols = n
                        .children()
                        .filter(|c| c.kind() == BIND_ARG)
                        .map(|b| {
                            let input = tokens(&b).next().is_some_and(|t| t.kind() == PLUS);
                            (input, word_text(&b, 0))
                        })
                        .collect();
                    self.decls.externs.insert(name, cols);
                }
                DECL => {
                    let toks: Vec<SyntaxToken> = tokens(&n).collect();
                    if toks.get(1).is_some_and(|t| t.kind() != TYPE_KW) {
                        self.decls.relations.insert(dotted_text(&n, 1));
                    }
                }
                TYPE_DECL => {
                    self.decls.types.insert(dotted_text(&n, 1));
                }
                RESOURCE => {
                    let typ = dotted_text(&n, 1);
                    self.decls.types.insert(typ.clone());
                    if let Some(name) = self.static_header(&n) {
                        self.decls.scopes[decl]
                            .resources
                            .entry(name)
                            .or_default()
                            .push(typ);
                    }
                }
                INSTANCE => {
                    self.decls
                        .instances
                        .entry(word_text(&n, 1))
                        .or_default()
                        .insert(word_text(&n, 2));
                }
                MODULE | POLICY | SCENARIO => {
                    let scope = self.new_scope(lets);
                    let start: u32 = n.text_range().start().into();
                    self.decls.blocks.insert((file, start), scope);
                    if n.kind() == MODULE {
                        self.decls.modules.insert(word_text(&n, 1), scope);
                    }
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.collect(file, &b, scope, scope);
                    }
                }
                WHEN | FOR_STMT => {
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.collect(file, &b, lets, decl);
                    }
                }
                RULE | FACT => {
                    if let Some(h) = n
                        .children()
                        .find(|c| matches!(c.kind(), CALL | RECORD_ATOM))
                        && let Some(name) = self.callee(&h)
                    {
                        self.decls.relations.insert(name);
                    }
                }
                _ => {}
            }
        }
    }

    /// A resource header's name when it is static: a string with no holes,
    /// or a name the block's clauses do not bind.
    fn static_header(&self, n: &SyntaxNode) -> Option<String> {
        let t = self.header_token(n)?;
        if t.kind() == STRING {
            let text = t.text();
            if text.contains('{') {
                return None;
            }
            return unescape(text).ok();
        }
        let name = t.text().to_string();
        let block = node(n, BLOCK)?;
        let bound = block
            .children()
            .filter(|c| c.kind() == CLAUSE)
            .flat_map(|c| c.descendants().collect::<Vec<_>>())
            .filter_map(|c| Chain::of(&c))
            .any(|c| c.is_bare() && c.head == name);
        (!bound).then_some(name)
    }

    /// The name token of a resource or settings header.
    fn header_token(&self, n: &SyntaxNode) -> Option<SyntaxToken> {
        let mut seen_word = false;
        let mut after_dot = false;
        for t in tokens(n).skip(1) {
            match t.kind() {
                DOT => after_dot = true,
                STRING => return Some(t),
                k if is_word(k) => {
                    if n.kind() == SETTINGS || (seen_word && !after_dot) {
                        return Some(t);
                    }
                    seen_word = true;
                    after_dot = false;
                }
                _ => return None,
            }
        }
        None
    }

    /// `T` in `output k: T` when it names a resource type.
    fn resource_type(&self, t: &SyntaxNode) -> Option<String> {
        if node(t, TYPE_EXPR).is_some() {
            return None;
        }
        let name = dotted_text(t, 0);
        name.contains('.').then_some(name)
    }

    // --- scopes -------------------------------------------------------------

    fn chain_of(&self, scope: usize) -> Vec<usize> {
        let mut out = vec![scope];
        let mut s = scope;
        while let Some(p) = self.decls.scopes[s].parent {
            out.push(p);
            s = p;
        }
        out
    }

    fn find_let(&self, scope: usize, name: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].lets.get(name).cloned())
    }

    fn is_value(&self, scope: usize, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.decls.scopes[s].values.contains(name))
    }

    /// The types a resource name has in scope (the innermost scope that
    /// declares it).
    fn resource(&self, scope: usize, name: &str) -> Option<Vec<String>> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].resources.get(name).cloned())
    }

    fn resource_of_type(&self, scope: usize, typ: &str, name: &str) -> bool {
        self.chain_of(scope).into_iter().any(|s| {
            self.decls.scopes[s]
                .resources
                .get(name)
                .is_some_and(|ts| ts.iter().any(|t| t == typ))
        })
    }

    // --- spans and errors -----------------------------------------------

    fn span_of(&self, r: rowan::TextRange) -> Span {
        Span {
            file: self.file,
            start: u32::from(r.start()) + self.offset,
            end: u32::from(r.end()) + self.offset,
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

    // --- files --------------------------------------------------------------

    fn unit(&mut self, i: usize, require_edition: bool) -> Vec<Stmt> {
        let unit = &self.units[i];
        let (file, root) = (unit.file, unit.root.clone());
        let saved = self.file;
        self.file = file;
        let scope = self.decls.files[&file];
        let mut statements = Vec::new();
        let mut first = true;
        let edition = root.children().any(|n| n.kind() == EDITION);
        let mut import_ix = 0;
        for n in root.children() {
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
                            format!("missing the edition pragma `edition {EDITION_YEAR}`"),
                        )
                        .with_help(format!(
                            "every .df file starts with `edition {EDITION_YEAR}` (docs/grammar.md)"
                        ));
                        self.diags.push(d);
                    }
                    if n.kind() == IMPORT
                        && let Some(imports) = &self.units[i].imports
                    {
                        let target = imports.get(import_ix).copied().flatten();
                        import_ix += 1;
                        if let Some(t) = target {
                            statements.extend(self.unit(t, require_edition));
                            self.file = file;
                        }
                    } else {
                        statements.extend(self.stmt(&n, scope, &Rc::default()));
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
                format!("missing the edition pragma `edition {EDITION_YEAR}`"),
            )
            .with_help(format!(
                "every .df file starts with `edition {EDITION_YEAR}`"
            ));
            self.diags.push(d);
        }
        self.file = saved;
        statements
    }

    fn stmts(&mut self, block: Option<SyntaxNode>, scope: usize, outer: &Rc) -> Vec<Stmt> {
        let Some(block) = block else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for n in block.children().filter(|n| n.kind() != ERROR) {
            out.extend(self.stmt(&n, scope, outer));
        }
        out
    }

    /// A statement's lowering, with the helper rules it generated after it.
    /// `outer` carries the variables an enclosing `for`/`when` binds.
    fn stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> Vec<Stmt> {
        let saved = std::mem::take(&mut self.helpers);
        let mut out = self.stmt1(n, scope, outer).unwrap_or_default();
        out.append(&mut self.helpers);
        self.helpers = saved;
        out
    }

    /// A fresh rule context for a statement in `scope`.
    fn rc(&self, n: &SyntaxNode, scope: usize, outer: &Rc) -> Rc {
        let mut rc = Rc {
            scope,
            vars: outer.vars.clone(),
            types: outer.types.clone(),
            outer: outer.vars.values().cloned().collect(),
            ..Rc::default()
        };
        rc.reserved.extend(outer.vars.values().cloned());
        for c in n.descendants() {
            if let Some(ch) = Chain::of(&c)
                && ch.is_bare()
                && !self.is_value(scope, &ch.head)
                && self.find_let(scope, &ch.head).is_none()
            {
                rc.reserved.insert(capitalise(&ch.head));
                rc.candidates.insert(ch.head.clone());
            }
        }
        rc.candidates.extend(outer.vars.keys().cloned());
        // `x in T`: x has the static type T wherever it is used.
        for c in n.descendants().filter(|c| c.kind() == LIT_IN) {
            let mut ts = terms(&c);
            let Some(lhs) = ts.next().and_then(|t| Chain::of(&t)) else {
                continue;
            };
            if !lhs.is_bare() || rc.types.contains_key(&lhs.head) {
                continue;
            }
            if tokens(&c).any(|t| t.kind() == RESOURCE_KW) {
                let tv = fresh(&mut rc, "Type");
                rc.types.insert(lhs.head, var(&tv));
            } else if let Some(rhs) = ts.next().and_then(|t| Chain::of(&t))
                && let Some(t) = self.chain_type(&rc, &rhs)
            {
                rc.types.insert(lhs.head, str_term(&t));
            }
        }
        rc
    }

    /// The type a chain names, when it names one (the right side of `in`).
    fn chain_type(&self, rc: &Rc, c: &Chain) -> Option<String> {
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return None;
        }
        let name = c.fields().join(".");
        if self.decls.types.contains(&name) {
            return Some(name);
        }
        if c.head == "world" || c.head_kind == SETTINGS_KW {
            return None;
        }
        let local = self.find_let(rc.scope, &c.head).is_some()
            || self.is_value(rc.scope, &c.head)
            || rc.types.contains_key(&c.head)
            || rc.vars.contains_key(&c.head)
            || self.resource(rc.scope, &c.head).is_some()
            || self.decls.modules.contains_key(&c.head)
            || rc.candidates.contains(&c.head);
        // `T.n.path` is a read of a resource, not a type.
        let fields = c.fields();
        let resource = (1..fields.len())
            .any(|i| self.resource_of_type(rc.scope, &fields[..i].join("."), &fields[i]));
        (!local && !resource && !c.ops.is_empty()).then_some(name)
    }

    fn stmt1(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let one = |s: Stmt| Ok(vec![s]);
        match n.kind() {
            IMPORT => {
                let path = tokens(n).find(|t| t.kind() == STRING).ok_or(Skip)?;
                let path = self.string(&path)?;
                if tokens(n).filter(|t| is_word(t.kind())).nth(1).is_some() {
                    return self.error(
                        span,
                        "`import ... as` is gone: an import is a file include; wrap reusable \
                         rules in a `module` and instantiate it (E DR-3)",
                    );
                }
                one(Stmt::Import(Import { path, span }))
            }
            PROVIDER | STACK => {
                let name = if n.kind() == PROVIDER {
                    word_text(n, 1)
                } else {
                    dotted_text(n, 1)
                };
                let block = node(n, BLOCK);
                let mut rc = self.rc(n, scope, outer);
                let config = self.constant_assigns(&mut rc, block.as_ref())?;
                let c = Config { name, config, span };
                one(if n.kind() == PROVIDER {
                    Stmt::Provider(c)
                } else {
                    Stmt::Stack(c)
                })
            }
            INPUT => {
                let name = word_text(n, 1);
                let ty = self.type_expr(&node(n, TYPE_EXPR).ok_or(Skip)?);
                let mut rc = self.rc(n, scope, outer);
                let default = match terms(n).next() {
                    Some(t) => Some(self.constant(&mut rc, &t)?),
                    None => None,
                };
                let refinement = self.where_clause(n, scope)?;
                one(Stmt::Input(InputDecl {
                    name,
                    ty,
                    default,
                    refinement,
                    span,
                }))
            }
            INPUT_RELATION => {
                let pred = word_text(n, 2);
                let arity = self.arity(n)?;
                let source = terms(n).next().ok_or(Skip)?;
                let mut rc = self.rc(n, scope, outer);
                let source = self.constant(&mut rc, &source)?;
                one(Stmt::InputRelation(InputRelation {
                    pred,
                    arity,
                    source,
                    span,
                }))
            }
            OUTPUT_DECL => self.output(n, scope, outer),
            EXPORT => {
                let pred = word_text(n, 1);
                let arity = self.arity(n)?;
                one(Stmt::Export(Export { pred, arity, span }))
            }
            CONTRIBUTES => {
                let c = n.children().find_map(|c| Chain::of(&c)).ok_or(Skip)?;
                let grant = self.grant(&c, span)?;
                one(Stmt::Contributes(Contributes { grant, span }))
            }
            EXTERN => {
                let name = dotted_text(n, 1);
                let args = n
                    .children()
                    .filter(|c| c.kind() == BIND_ARG)
                    .map(|b| BindArg {
                        input: tokens(&b).next().is_some_and(|t| t.kind() == PLUS),
                        name: word_text(&b, 0),
                        ty: node(&b, TYPE_EXPR).map(|t| self.type_expr(&t)),
                    })
                    .collect();
                let persist = tokens(n).any(|t| t.kind() == PERSIST_KW);
                one(Stmt::ExternFn(ExternFn {
                    name,
                    args,
                    persist,
                    span,
                }))
            }
            TYPE_DECL => {
                let name = dotted_text(n, 1);
                let attrs = self.attr_decls(n, scope)?;
                one(Stmt::Pending(Pending {
                    kind: PendingKind::TypeDecl { name, attrs },
                    span,
                }))
            }
            DECL => self.decl(n, span).map(|s| vec![s]),
            MODULE | POLICY | SCENARIO => {
                let name = word_text(n, 1);
                let start: u32 = n.text_range().start().into();
                let inner = self.decls.blocks[&(self.file, start)];
                let body = self.stmts(node(n, STMT_BLOCK), inner, outer);
                one(match n.kind() {
                    MODULE => Stmt::Module(Module { name, body, span }),
                    POLICY => Stmt::PolicyPack(PolicyPack { name, body, span }),
                    _ => Stmt::Scenario(Scenario { name, body, span }),
                })
            }
            APPLY => one(Stmt::ApplyPolicy(ApplyPolicy {
                name: word_text(n, 1),
                span,
            })),
            LET => {
                let t = terms(n).next().ok_or(Skip)?;
                if Chain::of(&t).is_none() {
                    return self.error(
                        self.span(&t),
                        "`let` names a reference (`let cfg = settings[env]`); a value is a value \
                         rule: `name = term`",
                    );
                }
                Ok(Vec::new())
            }
            WITH => {
                let key = word_text(n, 1);
                let mut rc = self.rc(n, scope, outer);
                let t = terms(n).next().ok_or(Skip)?;
                let value = self.constant(&mut rc, &t)?;
                one(Stmt::Fact(Atom {
                    pred: "input".to_string(),
                    args: vec![str_term(&key), value],
                    record: None,
                    span,
                }))
            }
            WHEN | FOR_STMT => self.when(n, scope, outer),
            INSTANCE => self.instance(n, scope, outer),
            RESOURCE | SETTINGS => self.block_stmt(n, scope, outer),
            RULE | FACT => self.rule(n, scope, outer),
            VALUE_RULE => self.value_rule(n, scope, outer),
            CONTRIBUTION => self.contribution(n, scope, outer),
            CHECK => self.check(n, scope, outer),
            k => self.error(span, format!("unexpected {k:?}")),
        }
    }

    // --- declarations that lower to themselves ----------------------------

    /// `decl p/N` is an extern, `decl p/N mixed` lets `p/N` have both facts
    /// and rules; `decl p(field: type, ...)` a record declaration; `decl
    /// type ... open` is pending.
    fn decl(&mut self, n: &SyntaxNode, span: Span) -> L<Stmt> {
        let toks: Vec<SyntaxToken> = tokens(n).collect();
        if toks.get(1).is_some_and(|t| t.kind() == TYPE_KW) {
            return Ok(Stmt::Pending(Pending {
                kind: PendingKind::DeclOpenType {
                    name: dotted_text(n, 2),
                },
                span,
            }));
        }
        let pred = dotted_text(n, 1);
        if toks.iter().any(|t| t.kind() == SLASH) {
            let arity = self.arity(n)?;
            if toks.last().is_some_and(|t| t.text() == "mixed") {
                return Ok(Stmt::Mixed(Extern { pred, arity, span }));
            }
            return Ok(Stmt::Extern(Extern { pred, arity, span }));
        }
        let fields = n
            .children()
            .filter(|c| c.kind() == BIND_ARG)
            .map(|b| word_text(&b, 0))
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

    /// `contributes p`, `contributes _.path`, `contributes settings.path`,
    /// `contributes TYPE.path`.
    fn grant(&mut self, c: &Chain, span: Span) -> L<Grant> {
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return self.error(span, "a grant is a relation or TYPE.path");
        }
        let segs = c.fields();
        if segs.len() == 1 && c.head != "_" {
            return Ok(Grant::Pred(c.head.clone()));
        }
        let split = if c.head == "_" || c.head_kind == SETTINGS_KW {
            1
        } else if let Some(i) = (1..segs.len())
            .rev()
            .find(|i| self.decls.types.contains(&segs[..*i].join(".")))
        {
            i
        } else if segs.len() >= 3 {
            2
        } else {
            1
        };
        let typ = segs[..split].join(".");
        let path = segs[split..].join(".");
        Ok(Grant::Arg {
            typ: (typ != "_").then_some(typ),
            path: (!path.is_empty() && path != "_").then_some(path),
        })
    }

    fn attr_decls(&mut self, n: &SyntaxNode, scope: usize) -> L<Vec<AttrDecl>> {
        let mut out = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let path = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let ty = node(&a, TYPE_EXPR).map(|t| self.type_expr(&t));
            let flags = tokens(&a)
                .filter(|t| t.kind() == IDENT)
                .map(|t| t.text().to_string())
                .collect();
            let refinement = self.where_clause(&a, scope)?;
            let children = self.attr_decls(&a, scope)?;
            out.push(AttrDecl {
                path,
                ty,
                flags,
                refinement,
                children,
                span: self.span(&a),
            });
        }
        Ok(out)
    }

    /// A `where` body: names are their own text (the attribute, an input).
    fn where_clause(&mut self, n: &SyntaxNode, scope: usize) -> L<Vec<Lit>> {
        let Some(b) = node(n, WHERE_CLAUSE).and_then(|w| node(&w, BODY)) else {
            return Ok(Vec::new());
        };
        let saved = self.lenient;
        self.lenient = true;
        let mut rc = Rc {
            scope,
            ..Rc::default()
        };
        let r = self.body(&mut rc, &b);
        self.lenient = saved;
        r
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
                let name = dotted_text(n, 0);
                let args: Vec<TypeExpr> = n
                    .children()
                    .filter(|c| c.kind() == TYPE_EXPR)
                    .map(|c| self.type_expr(&c))
                    .collect();
                if args.is_empty() {
                    TypeExpr::Name(name)
                } else if name == "enum" {
                    // `enum("a", "b")`: its members are names.
                    TypeExpr::Apply(
                        name,
                        args.into_iter()
                            .map(|a| match a {
                                TypeExpr::Str(s) => TypeExpr::Name(s),
                                other => other,
                            })
                            .collect(),
                    )
                } else {
                    TypeExpr::Apply(name, args)
                }
            }
        }
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

    // --- statements with bodies -------------------------------------------

    /// A term that must be a constant: no read, no variable.
    fn constant(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Term> {
        let mut pre = Vec::new();
        let t = self.term(rc, n, Pos::Whole, &mut pre)?;
        if !pre.is_empty() {
            return self.error(
                self.span(n),
                "this value is read before the program runs: it must be a constant",
            );
        }
        self.check_bound(rc, &[], &[&t])?;
        Ok(t)
    }

    fn constant_assigns(
        &mut self,
        rc: &mut Rc,
        block: Option<&SyntaxNode>,
    ) -> L<Vec<(String, Term, Span)>> {
        let Some(block) = block else {
            return Ok(Vec::new());
        };
        if let Some(c) = node(block, CLAUSE) {
            return self.error(self.span(&c), "a provider or stack block takes no clause");
        }
        let mut out = Vec::new();
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let value = self.constant(rc, &terms(&a).next().ok_or(Skip)?)?;
            out.push((key, value, self.span(&a)));
        }
        Ok(out)
    }

    fn output(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Some(t) = node(n, TYPE_EXPR) {
            let ty = match self.resource_type(&t) {
                Some(_) => TypeExpr::Name("addr".to_string()),
                None => self.type_expr(&t),
            };
            return Ok(vec![Stmt::Output(OutputDecl {
                name,
                ty: Some(ty),
                value: None,
                span,
            })]);
        }
        let t = terms(n).next().ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut pre = Vec::new();
        // A bare resource name is its address.
        let value = match Chain::of(&t) {
            Some(c) if c.is_bare() && self.resource(scope, &c.head).is_some() => str_term(&c.head),
            _ => self.term(&mut rc, &t, Pos::Whole, &mut pre)?,
        };
        if pre.is_empty() {
            self.check_bound(&rc, &[], &[&value])?;
            return Ok(vec![Stmt::Output(OutputDecl {
                name,
                ty: None,
                value: Some(value),
                span,
            })]);
        }
        let head = Atom {
            pred: "output".to_string(),
            args: vec![str_term(&name), value],
            record: None,
            span,
        };
        self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?;
        Ok(vec![Stmt::Rule(RuleStmt { head, body: pre })])
    }

    /// `when B { S }` and `for B { S }`: a nested `when` per literal of B.
    fn when(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let b = node(n, BODY).ok_or(Skip)?;
        let mut rc = self.rc(&b, scope, outer);
        let guard = self.body(&mut rc, &b)?;
        self.check_bound(&rc, &guard, &[])?;
        let inner = Rc {
            vars: rc.vars.clone(),
            types: rc.types.clone(),
            ..Rc::default()
        };
        let mut body = self.stmts(node(n, STMT_BLOCK), scope, &inner);
        for g in guard.into_iter().rev() {
            body = vec![Stmt::When(When {
                guard: g,
                body,
                span,
            })];
        }
        Ok(body)
    }

    /// The clauses of a block: its `for` and `if` bodies, in order.
    fn clauses(&mut self, rc: &mut Rc, block: &SyntaxNode) -> L<Vec<Lit>> {
        let mut out = Vec::new();
        let mut field_seen = false;
        let mut failed = false;
        for c in block.children() {
            match c.kind() {
                ASSIGN => field_seen = true,
                CLAUSE if field_seen => {
                    return self.error(
                        self.span(&c),
                        "a `for` or `if` clause goes at the top of the block, before any field",
                    );
                }
                CLAUSE => match node(&c, BODY) {
                    Some(b) => match self.body(rc, &b) {
                        Ok(ls) => out.extend(ls),
                        Err(Skip) => failed = true,
                    },
                    None => failed = true,
                },
                _ => {}
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// The assignments of a block, their reads appended to `reads`.
    fn fields(
        &mut self,
        rc: &mut Rc,
        block: &SyntaxNode,
        reads: &mut Vec<Lit>,
    ) -> L<Vec<FieldAssign>> {
        let mut out = Vec::new();
        let mut failed = false;
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let r = (|| {
                let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
                let op = if tokens(&a).any(|t| t.kind() == PLUS_EQ) {
                    FieldOp::Add
                } else {
                    FieldOp::Assign
                };
                let value = self.term(rc, &terms(&a).next().ok_or(Skip)?, Pos::Whole, reads)?;
                Ok(FieldAssign {
                    key,
                    op,
                    value,
                    rank: self.rank_tok(&a)?,
                    span: self.span(&a),
                })
            })();
            match r {
                Ok(f) => out.push(f),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    fn instance(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let module = word_text(n, 1);
        let name = word_text(n, 2);
        let block = node(n, BLOCK).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, &block)?;
        let mut reads = Vec::new();
        let fields = self.fields(&mut rc, &block, &mut reads)?;
        body.extend(reads);
        let mut inputs = Vec::new();
        for f in fields {
            if matches!(f.op, FieldOp::Add) {
                return self.error(f.span, "an instance input is set with `=`, not `+=`");
            }
            if f.rank.is_some() {
                return self.error(f.span, "an instance input takes no rank");
            }
            inputs.push((f.key, f.value, f.span));
        }
        let values: Vec<&Term> = inputs.iter().map(|(_, v, _)| v).collect();
        self.check_bound(&rc, &body, &values)?;
        Ok(vec![Stmt::Instance(Instance {
            module,
            name,
            inputs,
            body: (!body.is_empty()).then_some(body),
            span,
        })])
    }

    /// `resource T n { for B  if B  f = t ... }` and `settings e { ... }`.
    fn block_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let block = node(n, BLOCK).ok_or(Skip)?;
        let header = self.header_token(n).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, &block)?;
        let mut reads = Vec::new();
        let fields = self.fields(&mut rc, &block, &mut reads)?;
        body.extend(reads);
        // The header: a string with holes is bound last, by `format`; a
        // name the clauses bind is that variable; anything else static.
        let name = if header.kind() == STRING && header.text().contains('{') {
            let mut pre = Vec::new();
            let t = self.string_term(&mut rc, &header, &mut pre)?;
            body.extend(pre);
            let v = fresh(&mut rc, "Addr");
            body.push(Lit::Eq(var(&v), t));
            var(&v)
        } else if header.kind() == STRING {
            str_term(&self.string(&header)?)
        } else {
            let text = header.text();
            let bound = bound_vars(&body);
            match rc.vars.get(text) {
                Some(v) if bound.contains(v) || rc.outer.contains(v) => var(v),
                _ if text == "_" => Term::Wildcard,
                _ => str_term(text),
            }
        };
        let values: Vec<&Term> = fields
            .iter()
            .map(|f| &f.value)
            .chain(std::iter::once(&name))
            .collect();
        self.check_bound(&rc, &body, &values)?;
        let rank = self.rank_tok(n)?;
        let body = (!body.is_empty()).then_some(body);
        Ok(vec![if n.kind() == RESOURCE {
            Stmt::Resource(Resource {
                typ: str_term(&dotted_text(n, 1)),
                name,
                rank,
                fields,
                body,
                span,
            })
        } else {
            Stmt::Settings(Settings {
                env: name,
                rank,
                fields,
                body,
                span,
            })
        }])
    }

    fn opt_body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        match node(n, BODY) {
            Some(b) => self.body(rc, &b),
            None => Ok(Vec::new()),
        }
    }

    fn rule(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let head_node = n
            .children()
            .find(|c| matches!(c.kind(), CALL | RECORD_ATOM))
            .ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let mut head = self.atom(&mut rc, &head_node, Pos::Whole, &mut body)?;
        if let Some(rank) = self.rank_tok(n)? {
            if head.pred != "arg" || head.args.len() != 4 || head.record.is_some() {
                return self.error(
                    span,
                    "a rank applies to an `arg(T, A, Path, Value)` head only",
                );
            }
            head.args.push(str_term(rank.name()));
        }
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        if head.pred == "constraint" {
            let [Term::Val(Value::Str(message))] = head.args.as_slice() else {
                return self.error(span, "a constraint head is `constraint \"message\"`");
            };
            if !has_body {
                return self.error(
                    span,
                    "a constraint needs a body: `constraint \"...\" if ...`",
                );
            }
            return Ok(vec![Stmt::Constraint(Constraint {
                message: message.clone(),
                body,
                span,
            })]);
        }
        Ok(vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }])
    }

    /// `k = t [if B]`: the relation `k(t)`, read by name.
    fn value_rule(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        if tokens(n).any(|t| t.kind() == PLUS_EQ) {
            return self.error(span, "a value rule is `name = term`; `+=` adds to a field");
        }
        let name = word_text(n, 0);
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let t = terms(n).next().ok_or(Skip)?;
        let value = self.term(&mut rc, &t, Pos::Whole, &mut body)?;
        if self.rank_tok(n)?.is_some() {
            return self.error(span, "a value rule takes no rank");
        }
        let head = Atom {
            pred: name,
            args: vec![value],
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        Ok(vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }])
    }

    /// `R.p = t [@rank] [if B]`: a contribution `arg(T, A, p, t)`.
    fn contribution(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let mut ts = terms(n);
        let lhs = ts.next().ok_or(Skip)?;
        let rhs = ts.next().ok_or(Skip)?;
        let Some(c) = Chain::of(&lhs) else {
            return self.error(self.span(&lhs), "expected a reference and a path to assign");
        };
        let mut pre = Vec::new();
        let res = self.resolve(&mut rc, &c, &mut pre)?;
        body.extend(pre);
        let (typ, addr, path) = match res {
            Res::Ref { typ, addr, path } if !path.is_empty() => (typ, addr, path),
            Res::Settings { addr, path } if !path.is_empty() => (str_term("settings"), addr, path),
            _ => {
                return self.error(
                    self.span(&lhs),
                    "the left side of a contribution is a resource or settings row and a path \
                     (`r.tags`, `settings.prod.x`)",
                );
            }
        };
        let Some(path) = path_string(&path) else {
            return self.error(self.span(&lhs), "a contribution's path is constant");
        };
        let value = self.term(&mut rc, &rhs, Pos::Whole, &mut body)?;
        let add = tokens(n).any(|t| t.kind() == PLUS_EQ);
        let mut args = vec![typ, addr, str_term(&path), value];
        if let Some(rank) = self.rank_tok(n)? {
            if add {
                return self.error(span, "a rank applies to `=`, not `+=`");
            }
            args.push(str_term(rank.name()));
        }
        let head = Atom {
            pred: if add { "arg_add" } else { "arg" }.to_string(),
            args,
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        Ok(vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }])
    }

    /// `deny "m" {o} if B`, `warn ...`, `constraint "m" if B`.
    fn check(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let kw = tokens(n).next().ok_or(Skip)?;
        let msg = tokens(n).find(|t| t.kind() == STRING).ok_or(Skip)?;
        let message = self.string(&msg)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        if kw.kind() == CONSTRAINT_KW {
            if node(n, OBJECT).is_some() {
                return self.error(span, "a constraint takes a message and no object");
            }
            if !has_body {
                return self.error(
                    span,
                    "a constraint needs a body: `constraint \"...\" if ...`",
                );
            }
            self.check_bound(&rc, &body, &[])?;
            return Ok(vec![Stmt::Constraint(Constraint {
                message,
                body,
                span,
            })]);
        }
        let mut args = vec![str_term(&message)];
        if let Some(o) = node(n, OBJECT) {
            args.push(self.term(&mut rc, &o, Pos::Whole, &mut body)?);
        }
        let head = Atom {
            pred: kw.text().to_string(),
            args,
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        Ok(vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }])
    }

    /// Every variable written in the statement is bound: by a relation
    /// read, an equality, or an enclosing `for`/`when`. A name that is none
    /// of these was meant as a string.
    fn check_bound(&mut self, rc: &Rc, body: &[Lit], heads: &[&Term]) -> L<()> {
        let mut bound = bound_vars(body);
        // A comprehension binds its own variables, wherever it stands.
        let holder: Vec<Lit> = heads
            .iter()
            .map(|t| Lit::Neq((*t).clone(), (*t).clone()))
            .collect();
        bound.extend(bound_vars(&holder));
        bound.extend(rc.outer.iter().cloned());
        let mut failed = false;
        for (src, low) in &rc.vars {
            if rc.outer.contains(low) || (bound.contains(low) && rc.binders.contains(src)) {
                continue;
            }
            let Some(span) = rc.first.get(src).copied() else {
                continue;
            };
            failed = true;
            let d = Diagnostic::error(span, format!("unknown name `{src}`")).with_help(format!(
                "a variable is bound by a relation or an equality in the body; a string is \
                 quoted: \"{src}\""
            ));
            self.diags.push(d);
        }
        if failed { Err(Skip) } else { Ok(()) }
    }

    // --- bodies -------------------------------------------------------------

    fn body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        let mut out = Vec::new();
        let mut failed = false;
        for l in n.children() {
            if self.lit(rc, &l, &mut out).is_err() {
                failed = true;
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// One literal, with the reads it hoists before it, into `out`.
    fn lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        self.bind(true, |l| l.lit1(rc, n, out))
    }

    fn lit1(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let span = self.span(n);
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let atom = self.atom(rc, &a, Pos::Content, out)?;
                out.push(Lit::Pos(atom));
            }
            LIT_TRUTH => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let res = self.resolve(rc, &c, out)?;
                match self.read_atom(rc, &res, Term::Val(Value::Bool(true)), span) {
                    Some(a) => out.push(Lit::Pos(a)),
                    None => {
                        let t = self.realize(rc, res, Pos::Content, out, span)?;
                        out.push(Lit::Eq(t, Term::Val(Value::Bool(true))));
                    }
                }
            }
            LIT_CMP => self.cmp(rc, n, out)?,
            LIT_IN | LIT_NOT_IN => {
                let lit = self.membership(rc, n, out)?;
                out.push(if n.kind() == LIT_IN { lit } else { negate(lit) });
            }
            LIT_SOME => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let (binders, list) = ts.split_at(ts.len() - 1);
                let list = self.bind(false, |l| l.term(rc, &list[0], Pos::Content, out))?;
                let mut args = vec![list];
                for b in binders {
                    args.push(self.term(rc, b, Pos::Content, out)?);
                }
                out.push(Lit::Pos(atom_at("member", args, span)));
            }
            LIT_EXISTS => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let (typ, addr) = self.reference(rc, &c, out, span)?;
                out.push(Lit::Pos(atom_at("want", vec![typ, addr], span)));
            }
            LIT_HAS => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let res = self.resolve(rc, &c, out)?;
                match self.read_atom(rc, &res, Term::Wildcard, span) {
                    Some(a) => out.push(Lit::Pos(a)),
                    None => {
                        return self.error(
                            span,
                            "`has` takes an attribute of a resource (`has r.p`), a settings \
                             leaf or a value name",
                        );
                    }
                }
            }
            LIT_NOT => {
                let inner = n.children().next().ok_or(Skip)?;
                self.not(rc, &inner, out, span)?;
            }
            LIT_NOT_BLOCK => {
                let b = node(n, BODY).ok_or(Skip)?;
                self.neg_helper(rc, &b, out, span)?;
            }
            k => return self.error(span, format!("unexpected {k:?} in a body")),
        }
        Ok(())
    }

    /// `not L`: an atom, a read with a value, a membership; anything else
    /// through a helper relation.
    fn not(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>, span: Span) -> L<()> {
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let atom = self.atom(rc, &a, Pos::Content, out)?;
                out.push(Lit::Not(atom));
                return Ok(());
            }
            LIT_IN => {
                let lit = self.membership(rc, n, out)?;
                out.push(negate(lit));
                return Ok(());
            }
            LIT_EXISTS => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let (typ, addr) = self.reference(rc, &c, out, span)?;
                out.push(Lit::Not(atom_at("want", vec![typ, addr], span)));
                return Ok(());
            }
            LIT_TRUTH | LIT_HAS => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let value = if n.kind() == LIT_HAS {
                    Term::Wildcard
                } else {
                    Term::Val(Value::Bool(true))
                };
                let mut pre = Vec::new();
                let res = self.resolve(rc, &c, &mut pre)?;
                if let Some(a) = self.read_atom(rc, &res, value, span) {
                    out.extend(pre);
                    out.push(Lit::Not(a));
                    return Ok(());
                }
            }
            LIT_CMP => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
                if ts.len() == 2 && matches!(ops.as_slice(), [EQ | EQ2]) {
                    for (r, v) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                        let Some(c) = Chain::of(r) else { continue };
                        let mut pre = Vec::new();
                        let mut rc2 = rc.clone();
                        let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) else {
                            continue;
                        };
                        if matches!(res, Res::Val(_) | Res::Var { .. } | Res::Type(_)) {
                            continue;
                        }
                        let mut vpre = Vec::new();
                        let Ok(value) = self.probe(|l| {
                            l.bind(false, |l| l.term(&mut rc2, v, Pos::Content, &mut vpre))
                        }) else {
                            continue;
                        };
                        if !vpre.is_empty() {
                            continue;
                        }
                        if let Some(a) = self.read_atom(&mut rc2, &res, value, span) {
                            *rc = rc2;
                            out.extend(pre);
                            out.push(Lit::Not(a));
                            return Ok(());
                        }
                    }
                }
            }
            _ => {}
        }
        // A body holding this one literal.
        self.neg_helper(rc, n, out, span)
    }

    /// `not { B }` (or a `not` no single literal can say): a helper
    /// `__neg_N(ȳ) :- P, B` over the variables ȳ the body so far binds,
    /// and `not __neg_N(ȳ)`.
    fn neg_helper(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>, span: Span) -> L<()> {
        let mut inner = Vec::new();
        let mut rc2 = rc.clone();
        rc2.reads.clear();
        rc2.values.clear();
        if n.kind() == BODY {
            inner = self.body(&mut rc2, n)?;
        } else {
            self.lit(&mut rc2, n, &mut inner)?;
        }
        // Keep the variable table: names the helper introduced are its own.
        for (k, v) in &rc2.vars {
            rc.vars.entry(k.clone()).or_insert_with(|| v.clone());
        }
        rc.reserved.extend(rc2.reserved.iter().cloned());
        let outer_bound = {
            let mut b = bound_vars(out);
            b.extend(rc.outer.iter().cloned());
            b
        };
        let mut used = BTreeSet::new();
        for l in &inner {
            lit_vars(l, &mut used);
        }
        let shared: Vec<String> = used.intersection(&outer_bound).cloned().collect();
        let pred = format!("__neg_{}", self.negs);
        self.negs += 1;
        let head = atom_at(&pred, shared.iter().map(|v| var(v)).collect(), span);
        let mut body: Vec<Lit> = out
            .iter()
            .filter(|l| !matches!(l, Lit::Not(_)))
            .cloned()
            .collect();
        body.extend(inner);
        self.helpers.push(Stmt::Rule(RuleStmt {
            head: head.clone(),
            body,
        }));
        // The helper's own variables are bound in the helper.
        for v in used.difference(&outer_bound) {
            rc.outer.insert(v.clone());
        }
        out.push(Lit::Not(head));
        Ok(())
    }

    /// `a op b [op c]`, with the direct forms: `x = R.p` and `R.p == c`
    /// are the read itself.
    fn cmp(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
        if ts.len() == 2 && matches!(ops.as_slice(), [EQ | EQ2]) {
            for (r, v) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                let Some(c) = Chain::of(r) else { continue };
                let mut rc2 = rc.clone();
                let mut pre = Vec::new();
                let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) else {
                    continue;
                };
                if matches!(res, Res::Val(_) | Res::Var { .. } | Res::Type(_)) {
                    continue;
                }
                let mut vpre = Vec::new();
                let binding = ops[0] == EQ;
                let Ok(value) = self
                    .probe(|l| l.bind(binding, |l| l.term(&mut rc2, v, Pos::Content, &mut vpre)))
                else {
                    continue;
                };
                if !vpre.is_empty() {
                    continue;
                }
                if let Some(a) = self.read_atom(&mut rc2, &res, value, span) {
                    *rc = rc2;
                    out.extend(pre);
                    out.push(Lit::Pos(a));
                    return Ok(());
                }
            }
        }
        let mut lowered = Vec::new();
        for (i, t) in ts.iter().enumerate() {
            // `=` binds either side; `==`, `!=` and the orders test.
            let binding = ops.get(i.saturating_sub(1)) == Some(&EQ) && i <= 1
                || (i == 0 && ops.first() == Some(&EQ));
            lowered.push(self.bind(binding, |l| l.term(rc, t, Pos::Content, out))?);
        }
        for (i, op) in ops.iter().enumerate() {
            let (a, b) = (lowered[i].clone(), lowered[i + 1].clone());
            out.push(match op {
                EQ | EQ2 => Lit::Eq(a, b),
                NEQ => Lit::Neq(a, b),
                LT => Lit::Lt(a, b),
                LE => Lit::Le(a, b),
                GT => Lit::Gt(a, b),
                _ => Lit::Ge(a, b),
            });
        }
        Ok(())
    }

    /// Run `f` with the binding flag set to `binding`.
    fn bind<T>(&mut self, binding: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.binding;
        self.binding = binding;
        let r = f(self);
        self.binding = saved;
        r
    }

    /// Run `f`, discarding the diagnostics it records when it fails.
    fn probe<T>(&mut self, f: impl FnOnce(&mut Self) -> L<T>) -> L<T> {
        let n = self.diags.len();
        let helpers = self.helpers.len();
        let negs = self.negs;
        let r = f(self);
        if r.is_err() {
            self.diags.truncate(n);
            self.helpers.truncate(helpers);
            self.negs = negs;
        }
        r
    }

    /// `x in T` (a generator over `want`), `x in resource`, `x in world.T`,
    /// or `x in list`.
    fn membership(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<Lit> {
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let lhs_node = ts.first().ok_or(Skip)?;
        let any_type = tokens(n).any(|t| t.kind() == RESOURCE_KW);
        let rhs = ts.get(1).and_then(Chain::of);
        let typ = if any_type {
            let lhs = Chain::of(lhs_node).map(|c| c.head).unwrap_or_default();
            Some(match rc.types.get(&lhs) {
                Some(t) => t.clone(),
                None => var(&fresh(rc, "Type")),
            })
        } else {
            rhs.as_ref()
                .and_then(|c| self.chain_type(rc, c))
                .map(|t| str_term(&t))
        };
        let world = rhs
            .as_ref()
            .filter(|c| c.head == "world" && !c.ops.is_empty());
        if typ.is_some() || world.is_some() {
            // The left side: an element, bound or checked; a bare resource
            // name is its address; a computed name is bound first.
            let lhs = match Chain::of(lhs_node) {
                Some(c) if c.is_bare() && self.resource(rc.scope, &c.head).is_some() => {
                    let (_, a) = self.reference(rc, &c, out, span)?;
                    a
                }
                _ => {
                    let t = self.term(rc, lhs_node, Pos::Content, out)?;
                    if matches!(t, Term::Func { .. }) {
                        let v = fresh(rc, "Name");
                        out.push(Lit::Eq(var(&v), t));
                        var(&v)
                    } else {
                        t
                    }
                }
            };
            if let Some(w) = world {
                let typ = w.fields()[1..].join(".");
                return Ok(Lit::Pos(atom_at(
                    "cloud_exists",
                    vec![str_term(&typ), lhs],
                    span,
                )));
            }
            return Ok(Lit::Pos(atom_at("want", vec![typ.unwrap(), lhs], span)));
        }
        let rhs = ts.get(1).ok_or(Skip)?;
        let list = self.bind(false, |l| l.term(rc, rhs, Pos::Content, out))?;
        let item = self.term(rc, lhs_node, Pos::Content, out)?;
        Ok(Lit::Pos(atom_at("member", vec![list, item], span)))
    }

    /// A chain that must name one resource: its type and address.
    fn reference(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<(Term, Term)> {
        if c.is_bare()
            && let Some(types) = self.resource(rc.scope, &c.head)
        {
            if types.len() > 1 {
                return self.ambiguous(&c.head, &types, span);
            }
            return Ok((str_term(&types[0]), str_term(&c.head)));
        }
        match self.resolve(rc, c, out)? {
            Res::Ref { typ, addr, path } if path.is_empty() => Ok((typ, addr)),
            _ => self.error(
                span,
                "`exists` takes a resource: a name, `T.name`, `T[e]` or `m.i/name`",
            ),
        }
    }

    fn ambiguous<T>(&mut self, name: &str, types: &[String], span: Span) -> L<T> {
        let list = types
            .iter()
            .map(|t| format!("{t}.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        self.error(
            span,
            format!(
                "`{name}` names {} resources: write one of {list}",
                types.len()
            ),
        )
    }

    /// A relation atom: a call or a record, its arguments lowered at `pos`.
    fn atom(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Atom> {
        let span = self.span(n);
        let pred = self.callee(n).ok_or(Skip);
        let Ok(pred) = pred else {
            return self.error(span, "a relation is named by a plain name (`p` or `m.i.p`)");
        };
        if n.kind() == RECORD_ATOM {
            let mut fields = BTreeMap::new();
            for f in n.children().filter(|c| c.kind() == RECORD_FIELD) {
                let key = tokens(&f).next().ok_or(Skip)?.text().to_string();
                let value = self.term(rc, &terms(&f).next().ok_or(Skip)?, pos, pre)?;
                if fields.insert(key.clone(), value).is_some() {
                    return self.error(self.span(&f), format!("field `{key}` given twice"));
                }
            }
            return Ok(Atom {
                pred,
                args: Vec::new(),
                record: Some(fields),
                span,
            });
        }
        let args = self.args(rc, n, pos, pre)?;
        Ok(Atom {
            pred,
            args,
            record: None,
            span,
        })
    }

    /// The name a call or record is applied by: its chain's dotted text.
    fn callee(&self, n: &SyntaxNode) -> Option<String> {
        let c = n.children().find_map(|c| Chain::of(&c))?;
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return None;
        }
        Some(c.fields().join("."))
    }

    fn args(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Vec<Term>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for t in terms(&list) {
            out.push(self.term(rc, &t, pos, pre)?);
        }
        Ok(out)
    }

    // --- terms ------------------------------------------------------------

    fn term(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Term> {
        let span = self.span(n);
        let first = || tokens(n).next().unwrap();
        match n.kind() {
            LITERAL => {
                let t = first();
                match t.kind() {
                    INT => match t.text().parse::<i64>() {
                        Ok(i) => Ok(Term::Val(Value::Int(i))),
                        Err(_) => self.error(span, "integer out of range"),
                    },
                    STRING => self.string_term(rc, &t, pre),
                    TRUE_KW => Ok(Term::Val(Value::Bool(true))),
                    FALSE_KW => Ok(Term::Val(Value::Bool(false))),
                    _ => Err(self.not_yet(n, "the `null` literal (E DR-6)", None)),
                }
            }
            PATH_LIT => {
                let p = self.keypath(&first())?;
                Ok(str_term(&p[1..]))
            }
            CHAIN => {
                let c = Chain::of(n).ok_or(Skip)?;
                let res = self.resolve(rc, &c, pre)?;
                self.realize(rc, res, pos, pre, span)
            }
            CALL => {
                let name = self.callee(n);
                let Some(name) = name else {
                    return self.error(span, "a function is named by a plain name");
                };
                let args = self.bind(false, |l| l.args(rc, n, Pos::Content, pre))?;
                Ok(Term::Func { name, args })
            }
            RECORD_ATOM => self.error(
                span,
                "a record `p{...}` is a literal or a head, not a value",
            ),
            LIST => {
                let mut out = Vec::new();
                for t in terms(n) {
                    out.push(self.term(rc, &t, pos, pre)?);
                }
                Ok(Term::List(out))
            }
            OBJECT => {
                let mut m = BTreeMap::new();
                for f in n.children().filter(|c| c.kind() == OBJECT_FIELD) {
                    let k = tokens(&f).next().ok_or(Skip)?;
                    let key = if k.kind() == STRING {
                        self.string(&k)?
                    } else {
                        k.text().to_string()
                    };
                    let v = match terms(&f).next() {
                        Some(t) => self.term(rc, &t, pos, pre)?,
                        // `{ a }` is `{ a: a }`.
                        None => {
                            let c = Chain {
                                head: key.clone(),
                                head_kind: k.kind(),
                                range: k.text_range(),
                                ops: Vec::new(),
                            };
                            let res = self.resolve(rc, &c, pre)?;
                            self.realize(rc, res, pos, pre, self.span_of(k.text_range()))?
                        }
                    };
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
                let saved = (
                    std::mem::take(&mut rc.reads),
                    std::mem::take(&mut rc.values),
                );
                let mut body = self.body(rc, &node(n, BODY).ok_or(Skip)?)?;
                let item_node = terms(n).next().ok_or(Skip)?;
                let item = self.bind(false, |l| l.term(rc, &item_node, Pos::Whole, &mut body))?;
                (rc.reads, rc.values) = saved;
                Ok(Term::ListComp {
                    item: Box::new(item),
                    body,
                })
            }
            PAREN => {
                let inner = terms(n).next().ok_or(Skip)?;
                self.bind(false, |l| l.term(rc, &inner, Pos::Content, pre))
            }
            BIN_EXPR => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let op = tokens(n).next().ok_or(Skip)?;
                // `us-east` with no spaces: meant as a string.
                if op.kind() == MINUS
                    && ts.iter().all(|t| t.kind() == CHAIN)
                    && ts[0].text_range().end() == op.text_range().start()
                    && op.text_range().end() == ts[1].text_range().start()
                {
                    let d = Diagnostic::error(
                        span,
                        format!("`{}` is arithmetic on two names", n.text()),
                    )
                    .with_help(format!(
                        "`-` is always an operator; a name with one is a string: \"{}\"",
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
                let a = self.bind(false, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                let b = self.bind(false, |l| l.term(rc, &ts[1], Pos::Content, pre))?;
                Ok(func(name, vec![a, b]))
            }
            UNARY_EXPR => {
                let inner = terms(n).next().ok_or(Skip)?;
                Ok(
                    match self.bind(false, |l| l.term(rc, &inner, Pos::Content, pre))? {
                        Term::Val(Value::Int(i)) if inner.kind() == LITERAL => {
                            Term::Val(Value::Int(-i))
                        }
                        t => func("sub", vec![Term::Val(Value::Int(0)), t]),
                    },
                )
            }
            k => self.error(span, format!("unexpected {k:?} as a term")),
        }
    }

    /// A string literal: `"a{e}b"` is `format("a%sb", e)`, `{{` and `}}`
    /// are braces.
    fn string_term(&mut self, rc: &mut Rc, t: &SyntaxToken, pre: &mut Vec<Lit>) -> L<Term> {
        let text = t.text();
        if !text.contains(['{', '}']) {
            return Ok(str_term(&self.string(t)?));
        }
        let span = self.span_of(t.text_range());
        let base: u32 = t.text_range().start().into();
        let inner = &text[1..text.len() - 1];
        let bytes = inner.as_bytes();
        let mut fmt = String::new();
        let mut lit = String::new();
        let mut args = Vec::new();
        let mut i = 0;
        let flush = |lit: &mut String, fmt: &mut String, l: &mut Self| -> L<()> {
            let s = unescape(&format!("\"{lit}\"")).map_err(|e| {
                l.diags.push(Diagnostic::error(span, e));
                Skip
            })?;
            if s.contains("%s") {
                l.diags.push(Diagnostic::error(
                    span,
                    "an interpolated string cannot hold `%s`",
                ));
                return Err(Skip);
            }
            fmt.push_str(&s);
            lit.clear();
            Ok(())
        };
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    // `\u{...}` keeps its braces.
                    let end = if bytes.get(i + 1) == Some(&b'u') {
                        inner[i..].find('}').map_or(i + 2, |e| i + e + 1)
                    } else {
                        i + 2
                    };
                    lit.push_str(&inner[i..end.min(inner.len())]);
                    i = end;
                }
                b'{' if bytes.get(i + 1) == Some(&b'{') => {
                    lit.push('{');
                    i += 2;
                }
                b'}' if bytes.get(i + 1) == Some(&b'}') => {
                    lit.push('}');
                    i += 2;
                }
                b'{' => {
                    let mut depth = 1;
                    let mut j = i + 1;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    if depth > 0 {
                        return self.error(
                            span,
                            "an interpolation `{` is never closed; a brace is `{{`",
                        );
                    }
                    flush(&mut lit, &mut fmt, self)?;
                    fmt.push_str("%s");
                    let hole = &inner[i + 1..j - 1];
                    // +1: the opening quote.
                    let at = base + 1 + i as u32 + 1;
                    args.push(self.bind(false, |l| l.hole(rc, hole, at, pre))?);
                    i = j;
                }
                b'}' => {
                    return self.error(span, "an unmatched `}` in a string; a brace is `}}`");
                }
                _ => {
                    let c = inner[i..].chars().next().unwrap();
                    lit.push(c);
                    i += c.len_utf8();
                }
            }
        }
        flush(&mut lit, &mut fmt, self)?;
        if args.is_empty() {
            return Ok(str_term(&fmt));
        }
        let mut all = vec![str_term(&fmt)];
        all.extend(args);
        Ok(func("format", all))
    }

    /// An interpolation hole: a term, read now (a content position).
    fn hole(&mut self, rc: &mut Rc, src: &str, at: u32, pre: &mut Vec<Lit>) -> L<Term> {
        let parse = parse::parse_term(src);
        let saved = self.offset;
        self.offset += at;
        let r = (|| {
            if let Some(e) = parse.errors.first() {
                let span = Span {
                    file: self.file,
                    start: self.offset + e.start as u32,
                    end: self.offset + e.end as u32,
                    origin: 0,
                };
                return self.error(span, format!("in an interpolation: {}", e.message));
            }
            let root = parse.syntax();
            let t = terms(&root).next().ok_or(Skip)?;
            self.term(rc, &t, Pos::Content, pre)
        })();
        self.offset = saved;
        r
    }

    // --- resolution -------------------------------------------------------

    /// A variable's lowered name, recording it.
    fn var_named(&mut self, rc: &mut Rc, name: &str, span: Span) -> String {
        if self.binding {
            rc.binders.insert(name.to_string());
        }
        if let Some(v) = rc.vars.get(name) {
            return v.clone();
        }
        let mut v = capitalise(name);
        if rc.vars.values().any(|w| *w == v) {
            v = fresh(rc, &v);
        }
        rc.reserved.insert(v.clone());
        rc.vars.insert(name.to_string(), v.clone());
        rc.first.insert(name.to_string(), span);
        v
    }

    /// What a chain denotes (section "Names" of docs/grammar.md). Index
    /// terms are lowered in `rc`, their reads into `pre`.
    fn resolve(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>) -> L<Res> {
        self.resolve_depth(rc, c, pre, 0)
    }

    fn resolve_depth(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        depth: usize,
    ) -> L<Res> {
        let span = self.span_of(c.range);
        let h = c.head.as_str();
        if self.lenient && !rc.vars.contains_key(h) {
            if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
                return self.error(span, "a name here is a path: `a.b.c`");
            }
            return Ok(Res::Val(str_term(&c.fields().join("."))));
        }
        if h == "_" && c.is_bare() {
            return Ok(Res::Val(Term::Wildcard));
        }
        // A typed variable: a reference.
        if let Some(t) = rc.types.get(h).cloned() {
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Ref {
                typ: t,
                addr: var(&v),
                path,
            });
        }
        if !rc.vars.contains_key(h) {
            if let Some(l) = self.find_let(rc.scope, h) {
                if depth > 8 {
                    return self.error(span, format!("`let {h}` refers to itself"));
                }
                let Some(mut inner) = Chain::of(&l) else {
                    return self.error(span, format!("`let {h}` does not name a reference"));
                };
                // The alias's own names resolve where it is used; its
                // position is the use's.
                inner.ops.extend(c.ops.iter().cloned());
                inner.range = c.range;
                let saved_file = self.file;
                let r = self.resolve_depth(rc, &inner, pre, depth + 1);
                self.file = saved_file;
                return r;
            }
            if self.is_value(rc.scope, h) {
                let path = self.segs(rc, &c.ops, pre)?;
                return Ok(Res::Value {
                    pred: h.to_string(),
                    path,
                });
            }
            if c.head_kind == SETTINGS_KW {
                return self.settings(rc, c, pre, span);
            }
            if h == "world" && !c.ops.is_empty() {
                return self.world(rc, c, pre, span);
            }
        }
        if c.is_bare() {
            return self.bare(rc, h, span);
        }
        if !rc.vars.contains_key(h) {
            if let Some(types) = self.resource(rc.scope, h) {
                if types.len() > 1 {
                    return self.ambiguous(h, &types, span);
                }
                let path = self.segs(rc, &c.ops, pre)?;
                return Ok(Res::Ref {
                    typ: str_term(&types[0]),
                    addr: str_term(h),
                    path,
                });
            }
            if let Some(r) = self.module_path(rc, c, pre, span)? {
                return Ok(r);
            }
            if let Some(r) = self.typed_path(rc, c, pre, span)? {
                return Ok(r);
            }
        }
        // A variable's fields: `x.a.b`, `x[0]`.
        if rc.vars.contains_key(h)
            || (rc.candidates.contains(h) && !self.decls.namespaces.contains(h))
        {
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Var { var: var(&v), path });
        }
        // A dotted name that names nothing else is a type's name.
        if c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            return Ok(Res::Type(c.fields().join(".")));
        }
        self.error(span, format!("unknown name `{h}`"))
    }

    /// A bare name: a variable, unless it names something no variable may.
    fn bare(&mut self, rc: &mut Rc, h: &str, span: Span) -> L<Res> {
        if !rc.vars.contains_key(h) {
            let what = if self.resource(rc.scope, h).is_some() {
                Some("the resource")
            } else if self.decls.modules.contains_key(h) {
                Some("the module")
            } else if self.decls.namespaces.contains(h) {
                Some("the type namespace")
            } else if h == "world" {
                Some("the inventory")
            } else if h == "settings" {
                Some("the settings")
            } else {
                None
            };
            if let Some(what) = what {
                return self.error(
                    span,
                    format!(
                        "variable `{h}` shadows {what} `{h}`: rename the variable, or write \
                         `{h}.path` to read it"
                    ),
                );
            }
        }
        let v = self.var_named(rc, h, span);
        Ok(Res::Val(var(&v)))
    }

    /// The parts after a reference or value, lowered.
    fn segs(&mut self, rc: &mut Rc, ops: &[Op], pre: &mut Vec<Lit>) -> L<Vec<Seg>> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                Op::Field(f) => out.push(Seg::F(f.clone())),
                Op::Index(ts, r) => {
                    if ts.len() != 1 {
                        return self.error(self.span_of(*r), "an index takes one term");
                    }
                    let t = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                    out.push(Seg::I(t));
                }
                Op::Slash(_, r) => {
                    return self.error(
                        self.span_of(*r),
                        "`/` names a resource of an instance: `m.i/name`",
                    );
                }
            }
        }
        Ok(out)
    }

    /// `settings.n.path`, `settings[e].path`.
    fn settings(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let addr = match c.ops.first() {
            Some(Op::Field(n)) => str_term(n),
            Some(Op::Index(ts, _)) if ts.len() == 1 => {
                self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?
            }
            _ => {
                return self.error(
                    span,
                    "settings are read as `settings.NAME.path` or `settings[e].path`",
                );
            }
        };
        let path = self.segs(rc, &c.ops[1..], pre)?;
        Ok(Res::Settings { addr, path })
    }

    /// `world.T[e].path`: the provider's inventory.
    fn world(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let fields = c.fields();
        let typ = fields[1..].join(".");
        let rest = &c.ops[fields.len() - 1..];
        let Some(Op::Index(ts, _)) = rest.first() else {
            return self.error(span, "a live object is `world.T[name]`");
        };
        if ts.len() != 1 {
            return self.error(span, "a live object is `world.T[name]`");
        }
        let addr = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
        let segs = self.segs(rc, &rest[1..], pre)?;
        let Some(path) = path_string(&segs) else {
            return self.error(span, "a live object's path is constant");
        };
        Ok(Res::World { typ, addr, path })
    }

    /// `m.i.k` (an output), `m.i/n.p` (a resource of an instance), `m[e]`,
    /// `m.i` (the instance scope).
    fn module_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let m = c.head.as_str();
        let is_module = self.decls.modules.contains_key(m);
        let (inst, rest) = match c.ops.first() {
            Some(Op::Field(i))
                if is_module && self.decls.instances.get(m).is_some_and(|s| s.contains(i)) =>
            {
                (str_term(&format!("{m}.{i}")), &c.ops[1..])
            }
            // `a.b/c` is the address of `c` in instance `a.b`, even when
            // the module is not in view.
            Some(Op::Field(i)) if matches!(c.ops.get(1), Some(Op::Slash(..))) => {
                (str_term(&format!("{m}.{i}")), &c.ops[1..])
            }
            Some(Op::Index(ts, _)) if is_module && ts.len() == 1 => {
                let e = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                (
                    func("format", vec![str_term(&format!("{m}.%s")), e]),
                    &c.ops[1..],
                )
            }
            _ => return Ok(None),
        };
        let scoped = |n: &str| match &inst {
            Term::Val(Value::Str(s)) => func("scoped", vec![str_term(s), str_term(n)]),
            t => func("scoped", vec![t.clone(), str_term(n)]),
        };
        match rest.first() {
            None => Ok(Some(Res::Val(inst))),
            Some(Op::Slash(n, _)) => {
                let path = self.segs(rc, &rest[1..], pre)?;
                if path.is_empty() {
                    // An address needs no type.
                    let typ = self.module_resource_type(m, n).unwrap_or_default();
                    return Ok(Some(Res::Ref {
                        typ: str_term(&typ),
                        addr: scoped(n),
                        path,
                    }));
                }
                let Some(typ) = self.module_resource_type(m, n) else {
                    return self
                        .error(span, format!("module {m} declares no resource `{n}`"))
                        .map(Some);
                };
                Ok(Some(Res::Ref {
                    typ: str_term(&typ),
                    addr: scoped(n),
                    path,
                }))
            }
            Some(Op::Field(k)) => {
                let path = self.segs(rc, &rest[1..], pre)?;
                let typed = self
                    .decls
                    .modules
                    .get(m)
                    .and_then(|s| self.decls.scopes[*s].outputs.get(k).cloned())
                    .flatten();
                if let Some(t) = typed
                    && !path.is_empty()
                {
                    // A typed output: the address it holds, then a reference.
                    let v = self.read_var(
                        rc,
                        "output",
                        vec![inst.clone(), str_term(k)],
                        2,
                        k,
                        pre,
                        span,
                    );
                    return Ok(Some(Res::Ref {
                        typ: str_term(&t),
                        addr: v,
                        path,
                    }));
                }
                Ok(Some(Res::Output {
                    inst,
                    key: k.clone(),
                    path,
                }))
            }
            Some(_) => self
                .error(span, "after an instance: `.output`, `/resource`")
                .map(Some),
        }
    }

    fn module_resource_type(&self, m: &str, n: &str) -> Option<String> {
        let s = *self.decls.modules.get(m)?;
        let types = self.decls.scopes[s].resources.get(n)?;
        (types.len() == 1).then(|| types[0].clone())
    }

    /// `T.n.path` for a resource `n` of type `T`, `T[e].path`, and the
    /// lookups `p[a, b]` and `ext[a]`.
    fn typed_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let fields = c.fields();
        let k = fields.len();
        // The longest `T.n` with `n` a declared resource of type `T`.
        for i in (1..k).rev() {
            let typ = fields[..i].join(".");
            if self.resource_of_type(rc.scope, &typ, &fields[i]) {
                let path = self.segs(rc, &c.ops[i..], pre)?;
                return Ok(Some(Res::Ref {
                    typ: str_term(&typ),
                    addr: str_term(&fields[i]),
                    path,
                }));
            }
        }
        let Some(Op::Index(ts, _)) = c.ops.get(k - 1) else {
            return Ok(None);
        };
        let name = fields.join(".");
        let rest = &c.ops[k..];
        if let Some(cols) = self.decls.externs.get(&name).cloned() {
            let outs: Vec<usize> = cols
                .iter()
                .enumerate()
                .filter(|(_, (input, _))| !input)
                .map(|(i, _)| i)
                .collect();
            if outs.len() != 1 || ts.len() != cols.len() - 1 {
                return self
                    .error(
                        span,
                        format!(
                            "extern {name} is looked up with its inputs in brackets; it needs \
                             exactly one output"
                        ),
                    )
                    .map(Some);
            }
            let mut args = Vec::new();
            for t in ts {
                args.push(self.bind(true, |l| l.term(rc, t, Pos::Content, pre))?);
            }
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Lookup {
                pred: name,
                args,
                out: outs[0],
                path,
            }));
        }
        if k == 1 && self.decls.relations.contains(&name) && !self.decls.types.contains(&name) {
            let mut args = Vec::new();
            for t in ts {
                args.push(self.bind(true, |l| l.term(rc, t, Pos::Content, pre))?);
            }
            let path = self.segs(rc, rest, pre)?;
            let out = args.len();
            return Ok(Some(Res::Lookup {
                pred: name,
                args,
                out,
                path,
            }));
        }
        if self.decls.types.contains(&name) || k > 1 {
            if ts.len() != 1 {
                return self.error(span, "a resource is `T[name]`").map(Some);
            }
            let addr = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Ref {
                typ: str_term(&name),
                addr,
                path,
            }));
        }
        self.error(span, format!("unknown relation or type `{name}`"))
            .map(Some)
    }

    /// Hoist a read `pred(args.., V)` (V at `out`) once per rule; its
    /// variable.
    #[allow(clippy::too_many_arguments)]
    fn read_var(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        mut args: Vec<Term>,
        out: usize,
        hint: &str,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> Term {
        let key = format!("{pred}{args:?}@{out}");
        if let Some(v) = rc.reads.get(&key) {
            return v.clone();
        }
        let v = var(&fresh(rc, &capitalise(hint)));
        args.insert(out, v.clone());
        pre.push(Lit::Pos(atom_at(pred, args, span)));
        rc.reads.insert(key, v.clone());
        v
    }

    /// A resolved chain as a term at `pos`, hoisting what it reads.
    fn realize(
        &mut self,
        rc: &mut Rc,
        res: Res,
        pos: Pos,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Term> {
        match res {
            Res::Val(t) => Ok(t),
            Res::Type(t) => Ok(str_term(&t)),
            Res::Var { var: v, path } => self.path_of(rc, v, path, pre, span),
            Res::Ref { addr, path, .. } if path.is_empty() => Ok(addr),
            Res::Ref { typ, addr, path } if pos == Pos::Whole => {
                let Some(p) = path_string(&path) else {
                    return self.error(span, "a reference's path is constant");
                };
                Ok(func("ref", vec![typ, addr, str_term(&p)]))
            }
            Res::Ref { typ, addr, path } => {
                let Seg::F(first) = &path[0] else {
                    return self.error(span, "a resource's attribute is `r.name`");
                };
                let v = self.read_var(
                    rc,
                    "attr",
                    vec![typ, addr, str_term(first)],
                    3,
                    first,
                    pre,
                    span,
                );
                self.path_of(rc, v, path[1..].to_vec(), pre, span)
            }
            Res::Settings { addr, path } => {
                let (key, rest) = split_fields(&path);
                if key.is_empty() {
                    return self.error(span, "a settings row is read by a path: `settings.prod.x`");
                }
                let last = key.rsplit('.').next().unwrap_or(&key).to_string();
                let v = self.read_var(
                    rc,
                    "setting",
                    vec![addr, str_term(&key)],
                    2,
                    &last,
                    pre,
                    span,
                );
                self.path_of(rc, v, rest, pre, span)
            }
            Res::Output { inst, key, path } => {
                let v = self.read_var(rc, "output", vec![inst, str_term(&key)], 2, &key, pre, span);
                self.path_of(rc, v, path, pre, span)
            }
            Res::World { typ, addr, path } => {
                let last = path.rsplit('.').next().unwrap_or(&path).to_string();
                Ok(self.read_var(
                    rc,
                    "cloud_attr",
                    vec![str_term(&typ), addr, str_term(&path)],
                    3,
                    &last,
                    pre,
                    span,
                ))
            }
            Res::Value { pred, path } => {
                let v = match rc.values.get(&pred) {
                    Some(v) => var(v),
                    None => {
                        let name = fresh(rc, &capitalise(&pred));
                        pre.push(Lit::Pos(atom_at(&pred, vec![var(&name)], span)));
                        rc.values.insert(pred.clone(), name.clone());
                        var(&name)
                    }
                };
                self.path_of(rc, v, path, pre, span)
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } => {
                let v = self.read_var(
                    rc,
                    &pred.clone(),
                    args,
                    out,
                    &pred.replace('.', "_"),
                    pre,
                    span,
                );
                self.path_of(rc, v, path, pre, span)
            }
        }
    }

    /// Fields and indexes into a value: `__path(V, "a.b")`, `member(V, i, W)`.
    fn path_of(
        &mut self,
        rc: &mut Rc,
        mut v: Term,
        path: Vec<Seg>,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Term> {
        let mut fields: Vec<String> = Vec::new();
        let flush = |v: Term, fields: &mut Vec<String>| {
            if fields.is_empty() {
                return v;
            }
            let p = fields.join(".");
            fields.clear();
            func("__path", vec![v, str_term(&p)])
        };
        for s in path {
            match s {
                Seg::F(f) => fields.push(f),
                Seg::I(i) => {
                    v = flush(v, &mut fields);
                    let w = var(&fresh(rc, "Item"));
                    pre.push(Lit::Pos(atom_at("member", vec![v, i, w.clone()], span)));
                    v = w;
                }
            }
        }
        Ok(flush(v, &mut fields))
    }

    /// The read a resolved chain is, with `value` in its value column: the
    /// direct forms `R.p == c`, `k == c`, `not R.p`. `None` when the chain
    /// is not one read.
    fn read_atom(&mut self, rc: &mut Rc, res: &Res, value: Term, span: Span) -> Option<Atom> {
        let _ = rc;
        match res {
            Res::Ref { typ, addr, path } if path.len() == 1 => match &path[0] {
                Seg::F(p) => Some(atom_at(
                    "attr",
                    vec![typ.clone(), addr.clone(), str_term(p), value],
                    span,
                )),
                Seg::I(_) => None,
            },
            Res::Settings { addr, path } => {
                let (key, rest) = split_fields(path);
                (!key.is_empty() && rest.is_empty())
                    .then(|| atom_at("setting", vec![addr.clone(), str_term(&key), value], span))
            }
            Res::Output { inst, key, path } if path.is_empty() => Some(atom_at(
                "output",
                vec![inst.clone(), str_term(key), value],
                span,
            )),
            Res::World { typ, addr, path } => Some(atom_at(
                "cloud_attr",
                vec![str_term(typ), addr.clone(), str_term(path), value],
                span,
            )),
            Res::Value { pred, path } if path.is_empty() => Some(atom_at(pred, vec![value], span)),
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } if path.is_empty() => {
                let mut args = args.clone();
                args.insert(*out, value);
                Some(atom_at(pred, args, span))
            }
            _ => None,
        }
    }
}

/// A fresh lowered name starting with `base`, reserved.
fn fresh(rc: &mut Rc, base: &str) -> String {
    let base = if base.is_empty() { "V" } else { base };
    let mut name = base.to_string();
    let mut i = 1;
    while rc.reserved.contains(&name) {
        name = format!("{base}{i}");
        i += 1;
    }
    rc.reserved.insert(name.clone());
    name
}

fn atom_at(pred: &str, args: Vec<Term>, span: Span) -> Atom {
    Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span,
    }
}

fn negate(l: Lit) -> Lit {
    match l {
        Lit::Pos(a) => Lit::Not(a),
        Lit::Not(a) => Lit::Pos(a),
        other => other,
    }
}

fn atom_terms(a: &Atom) -> Vec<&Term> {
    a.args
        .iter()
        .chain(a.record.iter().flat_map(|r| r.values()))
        .collect()
}

/// The leading fields of a path as a dotted key, and the rest.
fn split_fields(path: &[Seg]) -> (String, Vec<Seg>) {
    let n = path.iter().take_while(|s| matches!(s, Seg::F(_))).count();
    let key = path[..n]
        .iter()
        .map(|s| match s {
            Seg::F(f) => f.as_str(),
            Seg::I(_) => "",
        })
        .collect::<Vec<_>>()
        .join(".");
    (key, path[n..].to_vec())
}

/// A constant path as today's string: `a.b[0].c`.
fn path_string(path: &[Seg]) -> Option<String> {
    let mut out = String::new();
    for s in path {
        match s {
            Seg::F(f) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(f);
            }
            Seg::I(Term::Val(Value::Int(i))) => out.push_str(&format!("[{i}]")),
            Seg::I(_) => return None,
        }
    }
    Some(out)
}

/// The variables a body binds: arguments of its positive relation reads
/// (patterns included), and both sides of an equality.
fn bound_vars(body: &[Lit]) -> BTreeSet<String> {
    fn pattern(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(v) => {
                out.insert(v.clone());
            }
            Term::List(xs) => xs.iter().for_each(|x| pattern(x, out)),
            Term::Obj(m) => m.values().for_each(|x| pattern(x, out)),
            Term::ListComp { body, .. } => out.extend(bound_vars(body)),
            _ => {}
        }
    }
    fn nested(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::ListComp { item, body } => {
                out.extend(bound_vars(body));
                nested(item, out);
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|x| nested(x, out)),
            Term::Obj(m) => m.values().for_each(|x| nested(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for l in body {
        match l {
            Lit::Pos(a) => {
                a.args.iter().for_each(|t| pattern(t, &mut out));
                if let Some(r) = &a.record {
                    r.values().for_each(|t| pattern(t, &mut out));
                }
                a.args.iter().for_each(|t| nested(t, &mut out));
            }
            Lit::Not(a) => a.args.iter().for_each(|t| nested(t, &mut out)),
            Lit::Eq(a, b) => {
                pattern(a, &mut out);
                pattern(b, &mut out);
                nested(a, &mut out);
                nested(b, &mut out);
            }
            Lit::Neq(a, b) | Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                nested(a, &mut out);
                nested(b, &mut out);
            }
        }
    }
    out
}

fn lit_vars(l: &Lit, out: &mut BTreeSet<String>) {
    fn term(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(v) => {
                out.insert(v.clone());
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|x| term(x, out)),
            Term::Obj(m) => m.values().for_each(|x| term(x, out)),
            Term::ListComp { item, body } => {
                term(item, out);
                body.iter().for_each(|l| lit_vars(l, out));
            }
            _ => {}
        }
    }
    match l {
        Lit::Pos(a) | Lit::Not(a) => {
            a.args.iter().for_each(|t| term(t, out));
            if let Some(r) = &a.record {
                r.values().for_each(|t| term(t, out));
            }
        }
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            term(a, out);
            term(b, out);
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
    use crate::partition::{fmt_atom, fmt_lit, fmt_rule, fmt_term};

    fn lits(ls: &[Lit]) -> String {
        ls.iter().map(fmt_lit).collect::<Vec<_>>().join(", ")
    }

    /// Every statement as the core prints it, one per line.
    fn show(stmts: &[Stmt]) -> Vec<String> {
        stmts
            .iter()
            .filter_map(|s| {
                Some(match s {
                    Stmt::Rule(r) => fmt_rule(r),
                    Stmt::Fact(a) => fmt_atom(a),
                    Stmt::Constraint(c) => {
                        format!("constraint {:?} :- {}", c.message, lits(&c.body))
                    }
                    Stmt::Resource(r) => {
                        let fields = r
                            .fields
                            .iter()
                            .map(|f| format!("{} = {}", f.key, fmt_term(&f.value)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let body = r.body.as_deref().map(lits).unwrap_or_default();
                        format!(
                            "resource {} {} {{ {fields} }} :- {body}",
                            fmt_term(&r.typ),
                            fmt_term(&r.name)
                        )
                    }
                    Stmt::Settings(st) => {
                        let fields = st
                            .fields
                            .iter()
                            .map(|f| format!("{} = {}", f.key, fmt_term(&f.value)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let body = st.body.as_deref().map(lits).unwrap_or_default();
                        format!("settings {} {{ {fields} }} :- {body}", fmt_term(&st.env))
                    }
                    Stmt::Instance(i) => {
                        let inputs = i
                            .inputs
                            .iter()
                            .map(|(k, v, _)| format!("{k} = {}", fmt_term(v)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let body = i.body.as_deref().map(lits).unwrap_or_default();
                        format!("instance {} {} {{ {inputs} }} :- {body}", i.module, i.name)
                    }
                    Stmt::Output(o) => {
                        format!("output {} = {:?}", o.name, o.value.as_ref().map(fmt_term))
                    }
                    Stmt::When(w) => {
                        format!("when {} {}", fmt_lit(&w.guard), show(&w.body).join("; "))
                    }
                    Stmt::Module(m) => {
                        format!("module {} {{ {} }}", m.name, show(&m.body).join("; "))
                    }
                    _ => return None,
                })
            })
            .collect()
    }

    fn lower(src: &str) -> Vec<String> {
        match crate::parser::parse_file_g("t.df", src, false) {
            Ok(p) => show(&p.statements),
            Err(e) => panic!("{e:#}"),
        }
    }

    fn error(src: &str) -> String {
        match crate::parser::parse_file_g("t.df", src, false) {
            Ok(p) => panic!("lowered: {:?}", show(&p.statements)),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn value_names_are_read_by_name() {
        let got = lower(
            "input env: enum(\"a\", \"b\") = \"a\"\n\
             p(x) if q(x), env == \"a\"\n\
             r(env) if q(_)\n\
             s(x) if q(x), not env, has env\n\
             serving = \"blue\" if q(1)\n\
             t(serving)\n",
        );
        assert_eq!(
            &got[..],
            [
                "p(X) :- q(X), env(\"a\")",
                "r(Env) :- q(_), env(Env)",
                "s(X) :- q(X), not env(true), env(_)",
                "serving(\"blue\") :- q(1)",
                "t(Serving) :- serving(Serving)",
            ]
        );
    }

    /// The worked example of the proposal (section 5.6): the block's reads
    /// follow its `for` clause, the interpolated name comes last.
    #[test]
    fn a_resource_block_lowers_to_its_shared_body() {
        let got = lower(
            "resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
             zone_index(\"a\", 0)\n\
             resource net.subnet \"private-{z}\" {\n\
               for data(\"zone\", z)\n\
               vpc_id     = vpc.id\n\
               cidr       = inet_subnet(vpc.cidr, 4, zone_index[z])\n\
               zone       = z\n\
               visibility = \"private\"\n\
             }\n",
        );
        assert_eq!(
            got[2],
            "resource \"net.subnet\" Addr { vpc_id = ref(\"net.vpc\", \"vpc\", \"id\"), \
             cidr = inet_subnet(Cidr, 4, ZoneIndex), zone = Z, visibility = \"private\" } :- \
             data(\"zone\", Z), attr(\"net.vpc\", \"vpc\", \"cidr\", Cidr), \
             zone_index(Z, ZoneIndex), Addr = format(\"private-%s\", Z)"
        );
    }

    #[test]
    fn a_dot_is_a_reference_in_a_field_and_a_read_elsewhere() {
        let got = lower(
            "resource k8s.namespace web { name = \"web\" }\n\
             resource k8s.deployment a { namespace = k8s.namespace.web.name }\n\
             resource k8s.deployment b {\n  if ns = k8s.namespace.web.name\n  namespace = ns\n}\n\
             p(web.name, x) if x = web.name\n",
        );
        assert_eq!(
            &got[1..],
            [
                "resource \"k8s.deployment\" \"a\" { namespace = ref(\"k8s.namespace\", \"web\", \"name\") } :- ",
                "resource \"k8s.deployment\" \"b\" { namespace = Ns } :- attr(\"k8s.namespace\", \"web\", \"name\", Ns)",
                "p(ref(\"k8s.namespace\", \"web\", \"name\"), X) :- attr(\"k8s.namespace\", \"web\", \"name\", X)",
            ]
        );
    }

    #[test]
    fn a_let_names_a_settings_row() {
        let got = lower(
            "input env: string = \"dev\"\n\
             let cfg = settings[env]\n\
             resource net.vpc v { cidr = cfg.net.cidr, name = \"{cfg.name}-vpc\" }\n\
             constraint \"x\" if cfg.x.y != \"z\"\n",
        );
        assert_eq!(
            &got[..],
            [
                "resource \"net.vpc\" \"v\" { cidr = Cidr, name = format(\"%s-vpc\", Name) } :- \
                 env(Env), setting(Env, \"net.cidr\", Cidr), setting(Env, \"name\", Name)",
                "constraint \"x\" :- env(Env), setting(Env, \"x.y\", Y), Y != \"z\"",
            ]
        );
    }

    #[test]
    fn membership_existence_and_negation() {
        let got = lower(
            "resource db.postgres pg { public = false }\n\
             deny \"public\" { resource: p } if p in db.postgres, not p.public == false\n\
             r.tags = { team: \"x\" } if r in resource\n\
             q(x) if some i, x in [1, 2], i >= 0, x not in [3]\n\
             ok(1) if exists pg, has pg.public, not exists db.postgres[\"other\"]\n\
             big(n) if n in world.net.vpc, world.net.vpc[n].size > 3\n",
        );
        assert_eq!(
            &got[1..],
            [
                "deny(\"public\", {resource: P}) :- want(\"db.postgres\", P), not attr(\"db.postgres\", P, \"public\", false)",
                "arg(Type, R, \"tags\", {team: \"x\"}) :- want(Type, R)",
                "q(X) :- member([1, 2], I, X), I >= 0, not member([3], X)",
                "ok(1) :- want(\"db.postgres\", \"pg\"), attr(\"db.postgres\", \"pg\", \"public\", _), not want(\"db.postgres\", \"other\")",
                "big(N) :- cloud_exists(\"net.vpc\", N), cloud_attr(\"net.vpc\", N, \"size\", Size), Size > 3",
            ]
        );
    }

    #[test]
    fn modules_instances_and_outputs() {
        let got = lower(
            "module m {\n  input n: int\n  output vpc: net.vpc\n  output ids: list(string)\n  \
             resource net.vpc vpc { size = n }\n  output vpc = vpc\n  output ids = [vpc.id]\n}\n\
             instance m a { n = 1 }\n\
             inst(\"a\")\n\
             p(v, s) if inst(i), v = m[i].vpc, s = m.a/vpc.size\n\
             q(x) if x = m.a.ids, exists m.a/vpc\n",
        );
        assert_eq!(
            got[0],
            "module m { output vpc = None; output ids = None; resource \"net.vpc\" \"vpc\" { size = N } :- n(N); output vpc = Some(\"\\\"vpc\\\"\"); \
             output ids = Some(\"[ref(\\\"net.vpc\\\", \\\"vpc\\\", \\\"id\\\")]\") }"
        );
        assert_eq!(
            &got[3..],
            [
                "p(V, S) :- inst(I), output(format(\"m.%s\", I), \"vpc\", V), attr(\"net.vpc\", scoped(\"m.a\", \"vpc\"), \"size\", S)",
                "q(X) :- output(\"m.a\", \"ids\", X), want(\"net.vpc\", scoped(\"m.a\", \"vpc\"))",
            ]
        );
    }

    #[test]
    fn interpolation_and_lookups() {
        let got = lower(
            "extern file.json(+path, -value)\n\
             p(\"{{x}} {x}%\") if q(x)\n\
             r(v) if v = file.json[\"a.json\"]\n\
             s(y) if q(x), y = \"n-{x}\", \"n-{x}\" in net.route\n",
        );
        assert_eq!(
            &got[..],
            [
                "p(format(\"{x} %s%\", X)) :- q(X)",
                "r(V) :- file.json(\"a.json\", V)",
                "s(Y) :- q(X), Y = format(\"n-%s\", X), Name = format(\"n-%s\", X), want(\"net.route\", Name)",
            ]
        );
    }

    #[test]
    fn a_negated_body_is_a_helper() {
        let got = lower("p(x) if q(x), not { r(x, y), s(y) }\n");
        assert_eq!(
            &got[..],
            [
                "p(X) :- q(X), not __neg_0(X)",
                "__neg_0(X) :- q(X), r(X, Y), s(Y)",
            ]
        );
    }

    #[test]
    fn when_and_for_nest_their_guards() {
        let got = lower(
            "input env: string = \"dev\"\nwhen env == \"prod\" { a(1) }\n\
             for e(x), f(x) { b(x) }\n",
        );
        assert_eq!(
            &got[..],
            ["when env(\"prod\") a(1)", "when e(X) when f(X) b(X)",]
        );
    }

    #[test]
    fn a_variable_may_not_shadow_a_name() {
        let e = error("resource net.vpc main { cidr = \"x\" }\np(net) if q(net)\n");
        assert!(
            e.contains("variable `net` shadows the type namespace `net`"),
            "{e}"
        );
        let e = error("resource net.vpc main { cidr = \"x\" }\np(main) if q(main)\n");
        assert!(
            e.contains("t.df:2:14: variable `main` shadows the resource `main`"),
            "{e}"
        );
    }

    #[test]
    fn an_unbound_name_is_meant_as_a_string() {
        let e = error("input env: string = \"dev\"\np(1) if env == prod\n");
        assert!(e.contains("t.df:2:16: unknown name `prod`"), "{e}");
        assert!(e.contains("\"prod\""), "{e}");
        let e = error("resource net.vpc main { cidr = dev }\n");
        assert!(e.contains("unknown name `dev`"), "{e}");
    }

    #[test]
    fn a_name_used_twice_needs_its_type() {
        let e = error(
            "resource k8s.namespace web { n = 1 }\nresource k8s.service web { n = 1 }\n\
             resource x.y z { a = web.n }\n",
        );
        assert!(
            e.contains("`web` names 2 resources: write one of k8s.namespace.web, k8s.service.web"),
            "{e}"
        );
    }

    #[test]
    fn a_block_name_is_a_variable_when_its_clause_binds_it() {
        let got = lower(
            "t(\"a\")\nresource net.vpc t { for t(t)\n size = 1 }\nresource net.vpc shared { size = 2 }\n",
        );
        assert_eq!(
            &got[1..],
            [
                "resource \"net.vpc\" T { size = 1 } :- t(T)",
                "resource \"net.vpc\" \"shared\" { size = 2 } :- ",
            ]
        );
        assert!(matches!(
            crate::parser::parse_file_g("t.df", "resource net.vpc n { size = 1 }\n", false)
                .unwrap()
                .statements[0],
            crate::ast::Stmt::Resource(crate::ast::Resource {
                name: Term::Val(_),
                body: None,
                ..
            })
        ));
    }
}
