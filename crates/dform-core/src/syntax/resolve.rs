//! From the lossless tree to `ast`: name resolution (docs/grammar.md
//! "Names") and lowering to today's AST, so `transform` and everything after
//! it see exactly what the core has always seen.
//!
//! Resolution is program-wide: a module, a resource or an input declared in
//! one file is named in another. `lower` takes every file of the program
//! (each with the files its imports resolved to), collects the
//! declarations, then lowers the entry files, inlining each import where it
//! stands. A chain (`a.b[e].c`) is resolved in this order: a variable of
//! the rule with a static type, a value name (an input or a `let`, which
//! may hold a reference), `settings`, `world`, a resource in scope, a
//! module instance, a type's resource by key (`T[e]`), a relation or extern
//! lookup, a type; a bare name that is none of these is a variable, and may
//! not take the name of a resource, a module or a type namespace in scope.
//! A `.` is static (H section 5.1): a name after it that nothing declares is
//! an error, never a string.
//!
//! Where a read lands: in a rule body, just before the literal that holds
//! it; in a head, a field or an instance input, appended to the body (the
//! block's one shared body: a read in any field gates the whole block).

use super::SyntaxKind::{self, *};
use super::parser as parse;
use super::{SyntaxNode, SyntaxToken};
use crate::ast::{
    ApplyPolicy, Atom, AttrDecl, BindArg, Config, Contributes, Decl, Export, Extern, ExternFn,
    FieldAssign, FieldOp, Grant, Import, InputDecl, InputRelation, Instance, Lit, Module,
    OutputDecl, Pending, PendingKind, PolicyPack, Program, Rank, Resource, RuleStmt, Scenario,
    Settings, Span, Stmt, Term, TypeExpr,
};
use crate::diag::Diagnostic;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

mod alias;
mod heads;
mod provider;
pub use provider::ENV_VAR;

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
    /// Every unit an `import` of the file names, loaded by it or before:
    /// whose type aliases are in scope in the file.
    pub links: Vec<usize>,
}

/// How text is read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A program.
    Program,
    /// Text the compiler printed (a refinement's `Display`): every name is
    /// its own text and a string has no interpolation.
    Text,
    /// A `query` or `why` pattern, read without the program's declarations:
    /// a dotted name that names nothing else is a type.
    Pattern,
}

/// Lower `entries` (and, through their imports, the rest of `units`).
/// `require_edition`: every file must start with the edition pragma.
pub fn lower(
    units: &[Unit],
    entries: &[usize],
    require_edition: bool,
    mode: Mode,
) -> Result<Program, Vec<Diagnostic>> {
    let mut l = Lowerer::new(units, mode == Mode::Text);
    l.text = mode == Mode::Text;
    l.any_type = mode == Mode::Pattern;
    l.core = !require_edition;
    if mode == Mode::Program {
        l.check_heads();
    }
    let mut statements = l.declare_builtin_externs();
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

/// The static type of a value name whose value is a reference (H-6): a dot
/// on it reads through the reference.
#[derive(Clone, Debug, PartialEq, Eq)]
enum VType {
    /// A settings row's key: `let cfg = settings[env]`.
    Settings,
    /// A resource's address, of the type: `let db = db.postgres["main"]`.
    Ref(String),
    /// A live object's name, of the type: `let o = world.net.vpc[n]`.
    World(String),
}

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    /// Inputs and `let`s: names read bare.
    values: BTreeSet<String>,
    /// A `let`'s rows: the terms it is defined by.
    lets: BTreeMap<String, Vec<SyntaxNode>>,
    /// Resources with a static name: name -> the types declaring it.
    resources: BTreeMap<String, Vec<String>>,
    /// Settings rows with a static name declared here.
    settings: BTreeSet<String>,
    /// Module instances declared here: (module, name).
    instances: BTreeSet<(String, String)>,
    /// A module's `output k: T`: `Some(T)` when T is a resource type.
    outputs: BTreeMap<String, Option<String>>,
    /// The arities each relation this scope's heads and `decl`s give it.
    arities: BTreeMap<String, BTreeSet<usize>>,
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
    /// Resource types: every resource header's, every `type` block's, every
    /// `type_*` fact's, and the built-in provider schemas'.
    types: BTreeSet<String>,
    /// First segments of the types: no variable may take one.
    namespaces: BTreeSet<String>,
    /// The namespaces whose every type the compiler knows: the built-in
    /// schemas' but `k8s` (a cluster's types, CRDs included, are its own).
    /// Another namespace's type may be a provider schema's the compiler
    /// does not read (`--provider`, a plugin's).
    closed: BTreeSet<String>,
    /// Relations: rule and fact heads, `decl`s, input relations.
    relations: BTreeSet<String>,
    /// `extern` relations: their columns, `(input, name)`.
    externs: BTreeMap<String, Vec<(bool, String)>>,
    /// Scenario scopes: a `set` of a stack input is theirs.
    scenarios: BTreeSet<usize>,
    /// Relations the program's own facts and rules define.
    heads: BTreeSet<String>,
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
        LITERAL | CHAIN | CALL | LIST | OBJECT | COMPREHENSION | PAREN | BIN_EXPR | UNARY_EXPR
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

/// A file's `stack` header, read from its tree without resolving it: the
/// stack's name and the inputs that key it (discovery, `project`).
pub fn stack_header(root: &SyntaxNode) -> Option<(String, Vec<String>)> {
    let n = root.children().find(|n| n.kind() == STACK)?;
    let keys = tokens(&n)
        .skip_while(|t| t.kind() != L_BRACKET)
        .take_while(|t| t.kind() != R_BRACKET)
        .filter(|t| is_word(t.kind()))
        .map(|t| t.text().to_string())
        .collect();
    Some((dotted_text(&n, 1), keys))
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
                    (None, DOT) => pending = Some(t.kind()),
                    (Some(DOT), STRING) => {
                        let s = crate::syntax::resolve::unescape(t.text()).unwrap_or_default();
                        ops.push(Op::Field(s));
                        pending = None;
                    }
                    (Some(DOT), _) => {
                        ops.push(Op::Field(t.text().to_string()));
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
    /// Strings are literal: no interpolation (`Mode::Text`).
    text: bool,
    /// Any dotted name may be a type (`Mode::Pattern`).
    any_type: bool,
    /// The core relations are writable (H-15): text that is not a
    /// program file, such as a provider schema or a test of the core.
    core: bool,
    /// What a call in the term being lowered is.
    calls: Calls,
    /// Type aliases and where each is in scope.
    aliases: alias::Aliases,
    /// The outputs declared so far, by scope.
    outputs: BTreeSet<(usize, String)>,
}

/// What a call is where it is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Calls {
    /// A function the evaluator applies.
    Function,
    /// In a rule's head: a function, or an aggregate.
    Head,
    /// A constructor the compiler reads as data: a provider's or stack's
    /// setting (`local("DIR")`, `jwks(...)`), an input relation's source,
    /// a `type_refine` constraint. Each reader checks its own names.
    Data,
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
            text: false,
            any_type: false,
            core: false,
            calls: Calls::Function,
            aliases: alias::Aliases::default(),
            outputs: BTreeSet::new(),
        };
        for u in units {
            let scope = l.new_scope(PROGRAM);
            l.decls.files.insert(u.file, scope);
            l.collect(u.file, &u.root, PROGRAM, scope);
        }
        l.decls.types.extend(schema_types().iter().cloned());
        l.decls.namespaces = l
            .decls
            .types
            .iter()
            .map(|t| t.split('.').next().unwrap_or(t).to_string())
            .collect();
        l.decls.closed = schema_types()
            .iter()
            .filter_map(|t| t.split_once('.').map(|(n, _)| n.to_string()))
            .filter(|n| n != "k8s")
            .collect();
        l.collect_aliases();
        l
    }

    fn new_scope(&mut self, parent: usize) -> usize {
        self.decls.scopes.push(Scope {
            parent: Some(parent),
            ..Scope::default()
        });
        self.decls.scopes.len() - 1
    }
    /// Record the declarations of a statement list in `decl`; a module,
    /// policy or scenario block nests in `outer`.
    /// Record the declarations of a statement list in `decl`.
    fn collect(&mut self, file: u32, parent: &SyntaxNode, decl: usize, outer: usize) {
        let arity = |n: &SyntaxNode| n.children().filter(|c| c.kind() == BIND_ARG).count();
        for n in parent.children() {
            match n.kind() {
                INPUT => {
                    let name = word_text(&n, 1);
                    self.decls.scopes[decl].values.insert(name);
                }
                LET => {
                    let name = word_text(&n, 1);
                    self.decls.relations.insert(name.clone());
                    let s = &mut self.decls.scopes[decl];
                    s.values.insert(name.clone());
                    s.arities.entry(name.clone()).or_default().insert(1);
                    if let Some(t) = terms(&n).next() {
                        s.lets.entry(name).or_default().push(t);
                    }
                }
                INPUT_RELATION => {
                    let name = word_text(&n, 1);
                    self.decls.relations.insert(name.clone());
                    self.decls.scopes[decl]
                        .arities
                        .entry(name)
                        .or_default()
                        .insert(arity(&n));
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
                    let name = dotted_text(&n, 1);
                    self.decls.relations.insert(name.clone());
                    self.decls.scopes[decl]
                        .arities
                        .entry(name)
                        .or_default()
                        .insert(arity(&n));
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
                SETTINGS => {
                    if let Some(name) = self.static_header(&n) {
                        self.decls.scopes[decl].settings.insert(name);
                    }
                }
                INSTANCE => {
                    let (m, i) = (word_text(&n, 1), word_text(&n, 2));
                    self.decls
                        .instances
                        .entry(m.clone())
                        .or_default()
                        .insert(i.clone());
                    self.decls.scopes[decl].instances.insert((m, i));
                }
                MODULE | POLICY | SCENARIO => {
                    let scope = self.new_scope(outer);
                    let start: u32 = n.text_range().start().into();
                    self.decls.blocks.insert((file, start), scope);
                    match n.kind() {
                        MODULE => {
                            self.decls.modules.insert(word_text(&n, 1), scope);
                        }
                        SCENARIO => {
                            self.decls.scenarios.insert(scope);
                        }
                        _ => {}
                    }
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.collect(file, &b, scope, scope);
                    }
                }
                RULE | FACT => {
                    if let Some(h) = n.children().find(|c| c.kind() == CALL)
                        && let Some(name) = self.callee(&h)
                    {
                        // A schema's `type_provider(T, ...)`, `type_attr(T, ...)`
                        // rows declare T.
                        let first = node(&h, ARG_LIST).and_then(|a| terms(&a).next());
                        if name.starts_with("type_")
                            && let Some(t) = &first
                        {
                            if let Some(c) = Chain::of(t)
                                && c.ops.iter().all(|o| matches!(o, Op::Field(..)))
                            {
                                self.decls.types.insert(c.fields().join("."));
                            } else if t.kind() == LITERAL
                                && let Some(s) = tokens(t).find(|x| x.kind() == STRING)
                                && let Ok(s) = unescape(s.text())
                            {
                                self.decls.types.insert(s);
                            }
                        }
                        let n_args = node(&h, ARG_LIST).map_or(0, |a| {
                            a.children()
                                .filter(|c| is_term(c.kind()) || c.kind() == NAMED_ARG)
                                .count()
                        });
                        self.decls.scopes[decl]
                            .arities
                            .entry(name.clone())
                            .or_default()
                            .insert(n_args);
                        self.decls.heads.insert(name.clone());
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
            if has_hole(text) {
                return None;
            }
            return string_value(text).ok();
        }
        let name = t.text().to_string();
        let bound = n
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

    /// The scope a statement lowered in `scope` declares into: a file's top
    /// level declares into the program.
    fn decl_scope(&self, scope: usize) -> usize {
        if self.decls.files.values().any(|s| *s == scope) {
            PROGRAM
        } else {
            scope
        }
    }

    fn chain_of(&self, scope: usize) -> Vec<usize> {
        let mut out = vec![scope];
        let mut s = scope;
        while let Some(p) = self.decls.scopes[s].parent {
            out.push(p);
            s = p;
        }
        out
    }

    /// A `let`'s rows and the scope that declares it.
    fn find_let(&self, scope: usize, name: &str) -> Option<(usize, Vec<SyntaxNode>)> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.decls.scopes[s]
                .lets
                .get(name)
                .map(|rows| (s, rows.clone()))
        })
    }

    /// The static type of a value name whose value is a reference (H-6):
    /// what every row of its `let` names, read from the rows' text. `Err`
    /// names the rows' types when they disagree.
    fn value_type(&self, scope: usize, name: &str) -> Result<Option<VType>, String> {
        self.value_type_depth(scope, name, 0)
    }

    fn value_type_depth(
        &self,
        scope: usize,
        name: &str,
        depth: usize,
    ) -> Result<Option<VType>, String> {
        let Some((at, rows)) = self.find_let(scope, name) else {
            return Ok(None);
        };
        if depth > 8 {
            return Ok(None);
        }
        let mut ty: Option<Option<VType>> = None;
        for row in &rows {
            let t = self.term_vtype(at, row, depth);
            match &ty {
                None => ty = Some(t),
                Some(u) if *u == t => {}
                Some(u) => {
                    let show = |v: &Option<VType>| match v {
                        None => "a value".to_string(),
                        Some(VType::Settings) => "a settings row".to_string(),
                        Some(VType::Ref(t)) => format!("a {t} reference"),
                        Some(VType::World(t)) => format!("a live {t}"),
                    };
                    return Err(format!(
                        "`let {name}` is {} in one row and {} in another",
                        show(u),
                        show(&t)
                    ));
                }
            }
        }
        Ok(ty.flatten())
    }

    /// The reference a `let` row's term names, if it names one: a settings
    /// row, a resource (by name in scope or `T[e]`), a live object, or
    /// another `let` holding one.
    fn term_vtype(&self, scope: usize, t: &SyntaxNode, depth: usize) -> Option<VType> {
        let c = Chain::of(t)?;
        let index_then_end = |ops: &[Op]| matches!(ops, [Op::Index(ts, _)] if ts.len() == 1);
        if c.head_kind == SETTINGS_KW {
            return index_then_end(&c.ops).then_some(VType::Settings);
        }
        if c.head == "world" {
            let fields = c.fields();
            let rest = &c.ops[fields.len() - 1..];
            return (fields.len() > 1 && index_then_end(rest))
                .then(|| VType::World(fields[1..].join(".")));
        }
        if c.is_bare() {
            if self.is_value(scope, &c.head) {
                return self
                    .value_type_depth(scope, &c.head, depth + 1)
                    .ok()
                    .flatten();
            }
            return match self.resource(scope, &c.head) {
                Some(types) if types.len() == 1 => Some(VType::Ref(types[0].clone())),
                _ => None,
            };
        }
        let fields = c.fields();
        let rest = &c.ops[fields.len() - 1..];
        let typ = fields.join(".");
        (self.decls.types.contains(&typ) && index_then_end(rest)).then_some(VType::Ref(typ))
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
        self.quoted_keys(&root);
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
        // Each doc comment is a `doc(Kind, Name, Key, Value)` fact per pair
        // (docs/grammar.md "Doc comments").
        if !self.text && !self.any_type {
            for d in super::doc::collect(&root) {
                let span = self.span_of(d.range);
                for (k, v) in &d.pairs {
                    let args = [d.kind, &d.name, k, v].map(str_term).to_vec();
                    statements.push(Stmt::Fact(atom_at("doc", args, span)));
                }
            }
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
        if !(self.core || self.lenient || self.text || self.any_type) {
            let span = self.span(n);
            if self.placeholders(&out, span).is_err() {
                return Vec::new();
            }
        }
        out
    }

    /// `_` stands where a variable could and is never accessed: not as a
    /// head's column (it has no value to derive), a field's value, a
    /// function's argument (an interpolation's included) or a comparison's
    /// side. Checked on what a statement lowered to.
    fn placeholders(&mut self, stmts: &[Stmt], span: Span) -> L<()> {
        fn has(t: &Term) -> bool {
            match t {
                Term::Wildcard => true,
                Term::Func { args, .. } | Term::List(args) => args.iter().any(has),
                Term::Obj(m) => m.values().any(has),
                Term::ListComp { item, body } => has(item) || lits(body),
                _ => false,
            }
        }
        // `_` inside a function's arguments, anywhere in `t`.
        fn in_func(t: &Term) -> bool {
            match t {
                Term::Func { args, .. } => args.iter().any(has),
                Term::List(xs) => xs.iter().any(in_func),
                Term::Obj(m) => m.values().any(in_func),
                Term::ListComp { item, body } => has(item) || lits(body),
                _ => false,
            }
        }
        fn lits(body: &[Lit]) -> bool {
            body.iter().any(|l| match l {
                Lit::Pos(a) | Lit::Not(a) => {
                    a.args.iter().any(in_func)
                        || a.record.as_ref().is_some_and(|r| r.values().any(in_func))
                }
                Lit::Eq(a, b)
                | Lit::Neq(a, b)
                | Lit::Gt(a, b)
                | Lit::Ge(a, b)
                | Lit::Lt(a, b)
                | Lit::Le(a, b) => has(a) || has(b),
            })
        }
        let never = "`_` is a placeholder and is never accessed: name it (`env.p`, `x`)";
        let bad = |l: &mut Self, at: Span, msg: String| {
            l.diags
                .push(Diagnostic::error(at, msg).with_help(never.to_string()));
        };
        let before = self.diags.len();
        for st in stmts {
            let (head, body): (Option<&Atom>, &[Lit]) = match st {
                Stmt::Fact(a) => (Some(a), &[]),
                Stmt::Rule(r) => (Some(&r.head), &r.body),
                Stmt::Resource(r) => {
                    for f in r.fields.iter().filter(|f| has(&f.value)) {
                        bad(
                            self,
                            f.span,
                            format!("`{}` is given `_`, which has no value", f.key),
                        );
                    }
                    (None, r.body.as_deref().unwrap_or_default())
                }
                Stmt::Settings(r) => {
                    for f in r.fields.iter().filter(|f| has(&f.value)) {
                        bad(
                            self,
                            f.span,
                            format!("`{}` is given `_`, which has no value", f.key),
                        );
                    }
                    (None, r.body.as_deref().unwrap_or_default())
                }
                Stmt::Instance(i) => {
                    for (k, _, at) in i.inputs.iter().filter(|(_, v, _)| has(v)) {
                        bad(
                            self,
                            *at,
                            format!("input `{k}` is given `_`, which has no value"),
                        );
                    }
                    (None, i.body.as_deref().unwrap_or_default())
                }
                Stmt::Output(o) => {
                    if o.value.as_ref().is_some_and(has) {
                        bad(
                            self,
                            o.span,
                            format!("output `{}` is `_`, which has no value", o.name),
                        );
                    }
                    (None, &[])
                }
                _ => (None, &[]),
            };
            if let Some(h) = head {
                let columns: Vec<(String, &Term)> = match &h.record {
                    Some(r) => r.iter().map(|(k, v)| (format!("`{k}`"), v)).collect(),
                    None => h
                        .args
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (format!("{}", i + 1), v))
                        .collect(),
                };
                if let Some((c, _)) = columns.iter().find(|(_, v)| has(v)) {
                    bad(
                        self,
                        h.span,
                        format!(
                            "`_` in the head of `{}`: column {c} has no finite set of values; \
                             bind a variable there in the body",
                            h.pred
                        ),
                    );
                }
            }
            if lits(body) {
                bad(
                    self,
                    span,
                    "`_` as a value: a function's argument, an interpolation or a comparison \
                     reads it"
                        .to_string(),
                );
            }
        }
        if self.diags.len() > before {
            Err(Skip)
        } else {
            Ok(())
        }
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
            // A resource named in scope is its address, not a variable.
            if !lhs.is_bare()
                || rc.types.contains_key(&lhs.head)
                || self.resource(scope, &lhs.head).is_some()
            {
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
        // A namespace no declaration or builtin schema names is a provider
        // schema's the compiler does not read (`--provider`, a plugin's).
        let namespace = self.decls.namespaces.contains(&c.head)
            || self.any_type
            || !self.decls.relations.contains(&c.head);
        (!local && !resource && namespace && !c.ops.is_empty()).then_some(name)
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
            PROVIDER => self.provider(n, scope, outer),
            STACK => {
                let name = dotted_text(n, 1);
                let block = node(n, BLOCK);
                let mut rc = self.rc(n, scope, outer);
                // A stack's `config = FORMAT(SOURCE)` is a table, not a
                // constant.
                let table = block.as_ref().and_then(|b| {
                    b.children()
                        .filter(|a| a.kind() == ASSIGN)
                        .find(|a| node(a, BLOCK_PATH).is_some_and(|p| p.text() == "config"))
                });
                let config = self.constant_assigns(&mut rc, block.as_ref(), table.as_ref())?;
                // The words between the header's `[` and `]`.
                let key_tokens: Vec<SyntaxToken> = tokens(n)
                    .skip_while(|t| t.kind() != L_BRACKET)
                    .take_while(|t| t.kind() != R_BRACKET)
                    .filter(|t| is_word(t.kind()))
                    .collect();
                let keys = key_tokens
                    .iter()
                    .map(|t| (t.text().to_string(), self.span_of(t.text_range())))
                    .collect();
                let mut out = match &table {
                    Some(a) => self.stack_config(a, &name, &key_tokens, scope, outer)?,
                    None => Vec::new(),
                };
                let c = Config {
                    name,
                    keys,
                    config,
                    span,
                };
                out.insert(0, Stmt::Stack(c));
                Ok(out)
            }
            INPUT => {
                let name = word_text(n, 1);
                let ty = self.type_expr(&node(n, TYPE_EXPR).ok_or(Skip)?);
                let mut rc = self.rc(n, scope, outer);
                let default = match terms(n).next() {
                    Some(t) => Some(self.constant(&mut rc, &t)?),
                    None => None,
                };
                let refinement = self.refinement(n, scope)?;
                one(Stmt::Input(InputDecl {
                    name,
                    ty,
                    default,
                    refinement,
                    span,
                }))
            }
            INPUT_RELATION => {
                let source = terms(n).next().ok_or(Skip)?;
                if source.kind() == CALL && self.callee(&source).as_deref() == Some("facts") {
                    return self.facts_relation(n, &source, scope, outer);
                }
                self.table(n, scope, outer)
            }
            OUTPUT_DECL => self.output(n, scope, outer),
            // An alias lowers to nothing: each use is its type.
            TYPE_ALIAS => Ok(Vec::new()),
            EXPORT if tokens(n).nth(1).is_some_and(|t| t.kind() == TYPE_KW) => Ok(Vec::new()),
            EXPORT => {
                // `export p`: every arity the module gives `p`.
                let pred = word_text(n, 1);
                let arities = self.decls.scopes[scope]
                    .arities
                    .get(&pred)
                    .cloned()
                    .unwrap_or_default();
                // None: the module interface check says so.
                let arities = if arities.is_empty() {
                    BTreeSet::from([0])
                } else {
                    arities
                };
                Ok(arities
                    .into_iter()
                    .map(|arity| {
                        Stmt::Export(Export {
                            pred: pred.clone(),
                            arity,
                            span,
                        })
                    })
                    .collect())
            }
            CONTRIBUTES => {
                let c = n.children().find_map(|c| Chain::of(&c)).ok_or(Skip)?;
                let grant = self.grant(&c, span)?;
                one(Stmt::Contributes(Contributes { grant, span }))
            }
            EXTERN => {
                let name = dotted_text(n, 1);
                self.check_extern(&name, span)?;
                let args = n
                    .children()
                    .filter(|c| c.kind() == BIND_ARG)
                    .map(|b| BindArg {
                        input: tokens(&b).next().is_some_and(|t| t.kind() == PLUS),
                        name: word_text(&b, 0),
                        ty: node(&b, TYPE_EXPR).map(|t| self.type_expr(&t)),
                    })
                    .collect();
                let persist = tokens(n).any(|t| t.text() == "persist");
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
            DECL => Ok(self.decl(n, span)),
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
            USE => one(Stmt::ApplyPolicy(ApplyPolicy {
                name: word_text(n, 1),
                span,
            })),
            LET => self.let_stmt(n, scope, outer),
            SET => self.set(n, scope, outer),
            INSTANCE => self.instance(n, scope, outer),
            RESOURCE | SETTINGS => self.block_stmt(n, scope, outer),
            RULE | FACT => self.rule(n, scope, outer),
            CHECK => self.check(n, scope, outer),
            k => self.error(span, format!("unexpected {k:?}")),
        }
    }

    // --- declarations that lower to themselves ----------------------------

    /// `decl p(a, b)` declares the relation `p/2` by its columns (H-11):
    /// one no rule of the program defines is fed from outside (a provider,
    /// a given fact); `decl p(a, b) mixed` lets it have both facts and
    /// rules. The column names are the named-argument form's.
    fn decl(&mut self, n: &SyntaxNode, span: Span) -> Vec<Stmt> {
        let pred = dotted_text(n, 1);
        let fields: Vec<String> = n
            .children()
            .filter(|c| c.kind() == BIND_ARG)
            .map(|b| word_text(&b, 0))
            .collect();
        let arity = fields.len();
        let mixed = tokens(n).last().is_some_and(|t| t.text() == "mixed");
        let e = Extern {
            pred: pred.clone(),
            arity,
            span,
        };
        let mut out = Vec::new();
        if mixed {
            out.push(Stmt::Mixed(e));
        } else if !self.decls.heads.contains(&pred) {
            out.push(Stmt::Extern(e));
        }
        out.push(Stmt::Decl(Decl { pred, fields, span }));
        out
    }

    /// `input p(a: T, ..) from facts(PATH)`: a relation read from a dform
    /// fact file (`facts(git(REPO, REF, PATH))` from git), re-read when it
    /// changes.
    fn facts_relation(
        &mut self,
        n: &SyntaxNode,
        source: &SyntaxNode,
        scope: usize,
        outer: &Rc,
    ) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let pred = word_text(n, 1);
        if n.parent().is_some_and(|p| p.kind() != SOURCE_FILE) {
            return self.error(span, "an input relation belongs at the top of the program");
        }
        let fields: Vec<String> = n
            .children()
            .filter(|c| c.kind() == BIND_ARG)
            .map(|b| word_text(&b, 0))
            .collect();
        let args: Vec<SyntaxNode> = node(source, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let [arg] = args.as_slice() else {
            return self.error(
                self.span(source),
                "facts takes one source: a path, or `git(REPO, REF, PATH)`",
            );
        };
        let mut rc = self.rc(n, scope, outer);
        let source = self.calls(Calls::Data, |l| l.constant(&mut rc, arg))?;
        // The core's source: `file(PATH)` or `git(REPO, REF, PATH)`.
        let source = match source {
            s @ Term::Func { .. } => s,
            path => func("file", vec![path]),
        };
        Ok(vec![
            Stmt::InputRelation(InputRelation {
                pred: pred.clone(),
                arity: fields.len(),
                source,
                span,
            }),
            Stmt::Decl(Decl { pred, fields, span }),
        ])
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

    /// `contributes p`, `contributes t.path` (every type), `contributes settings.path`,
    /// `contributes TYPE.path`.
    fn grant(&mut self, c: &Chain, span: Span) -> L<Grant> {
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return self.error(span, "a grant is a relation or TYPE.path");
        }
        let segs = c.fields();
        if c.head == "_" {
            let path = segs[1..].join(".");
            return self.error(
                span,
                format!(
                    "`_` is a placeholder and is never accessed: name the type, \
                     `contributes t.{path}` grants `.{path}` on every type"
                ),
            );
        }
        if segs.len() == 1 {
            return Ok(Grant::Pred(c.head.clone()));
        }
        // `t.path`: a name no type starts with stands for every type.
        let any = c.head_kind != SETTINGS_KW
            && !self.decls.namespaces.contains(&c.head)
            && !self.decls.types.contains(&c.head);
        let split = if c.head_kind == SETTINGS_KW || any {
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
            typ: (!any).then_some(typ),
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
            let refinement = self.refinement(&a, scope)?;
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

    /// A refinement's `check` body: names are their own text (the
    /// attribute, an input).
    fn refinement(&mut self, n: &SyntaxNode, scope: usize) -> L<Vec<Lit>> {
        let Some(b) = node(n, REFINEMENT).and_then(|w| node(&w, BODY)) else {
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
        self.type_expr_in(n, true)
    }

    /// `type_expr`; `aliases`: a bare name may be a type alias (not in an
    /// `enum(..)`, whose members are values).
    fn type_expr_in(&mut self, n: &SyntaxNode, aliases: bool) -> TypeExpr {
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
                    .map(|c| self.type_expr_in(&c, name != "enum"))
                    .collect();
                if args.is_empty() {
                    match aliases.then(|| self.alias(n, &name)).flatten() {
                        Some(t) => t,
                        None => TypeExpr::Name(name),
                    }
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

    /// A quoted segment `."k"` is one key: `.`, `[` or `]` inside it would
    /// read as a second segment wherever the path is printed.
    fn quoted_keys(&mut self, root: &SyntaxNode) {
        for c in root.descendants().filter(|n| n.kind() == CHAIN) {
            let mut after_dot = false;
            for t in c.children_with_tokens().filter_map(|e| e.into_token()) {
                if t.kind().is_trivia() {
                    continue;
                }
                if after_dot && t.kind() == STRING {
                    let _ = self.segment(&t);
                }
                after_dot = t.kind() == DOT;
            }
        }
    }

    fn segment(&mut self, t: &SyntaxToken) -> L<String> {
        let s = self.string(t)?;
        if s.contains(['.', '[', ']']) {
            return self.error(
                self.span_of(t.text_range()),
                format!("the key {s:?} holds `.`, `[` or `]`, which a path segment cannot carry"),
            );
        }
        Ok(s)
    }

    fn string(&mut self, t: &SyntaxToken) -> L<String> {
        let text = if self.text {
            unescape(t.text())
        } else {
            string_value(t.text())
        };
        match text {
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

    /// A provider's or stack's `k = constant` settings, but `skip`.
    fn constant_assigns(
        &mut self,
        rc: &mut Rc,
        block: Option<&SyntaxNode>,
        skip: Option<&SyntaxNode>,
    ) -> L<Vec<(String, Term, Span)>> {
        let Some(block) = block else {
            return Ok(Vec::new());
        };
        if let Some(c) = block.parent().and_then(|stmt| node(&stmt, CLAUSE)) {
            return self.error(self.span(&c), "a provider or stack block takes no clause");
        }
        let mut out = Vec::new();
        for a in block
            .children()
            .filter(|c| c.kind() == ASSIGN && Some(c) != skip)
        {
            let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let value = terms(&a).next().ok_or(Skip)?;
            let value = self.calls(Calls::Data, |l| l.constant(rc, &value))?;
            out.push((key, value, self.span(&a)));
        }
        Ok(out)
    }

    /// `input p(col: type, ...) from FORMAT(SOURCE)`: a table
    /// (`crate::tables`). Its rows are the answers of the extern
    /// `table.FORMAT.p`, asked once the source is known:
    /// `p(Cols) :- reads, Path = SOURCE, table.FORMAT.p(Path, At, Cols)`.
    /// `decl p(col, ...)` names the columns for the record form.
    fn table(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let pred = word_text(n, 1);
        if n.parent().is_some_and(|p| p.kind() != SOURCE_FILE) {
            return self.error(span, "an input relation belongs at the top of the program");
        }
        let mut cols: Vec<BindArg> = Vec::new();
        for b in n.children().filter(|c| c.kind() == BIND_ARG) {
            let name = word_text(&b, 0);
            let ty = node(&b, TYPE_EXPR).map(|t| self.type_expr(&t));
            let at = self.span(&b);
            if ty.is_none() {
                return self.error(
                    at,
                    format!("input relation {pred}: a table's column {name} needs a type"),
                );
            }
            if cols.iter().any(|c| c.name == name) {
                return self.error(
                    at,
                    format!("input relation {pred}: two columns are named {name}"),
                );
            }
            if let Some(Err(e)) = ty.as_ref().map(crate::inputs::check_type) {
                return self.error(at, format!("input relation {pred}: column {name}: {e}"));
            }
            if matches!(&ty, Some(TypeExpr::Apply(t, _)) if t == "secret") {
                let d = Diagnostic::error(
                    at,
                    format!("input relation {pred}: column {name} is a secret"),
                )
                .with_note(
                    "a table's rows are read in the clear and recorded in the plan file; \
                     a secret comes from a secret input or a `persist` extern",
                );
                self.diags.push(d);
                return Err(Skip);
            }
            cols.push(BindArg {
                input: false,
                name,
                ty,
            });
        }
        let mut rc = self.rc(n, scope, outer);
        let mut body = Vec::new();
        let src = terms(n).next().ok_or(Skip)?;
        let vars: Vec<Term> = cols
            .iter()
            .map(|c| var(&fresh(&mut rc, &capitalise(&c.name))))
            .collect();
        let mut out =
            self.table_body(&mut rc, &src, &pred, cols.clone(), vars.clone(), &mut body)?;
        self.check_bound(&rc, &body, &[])?;
        out.push(Stmt::Decl(Decl {
            pred: pred.clone(),
            fields: cols.iter().map(|c| c.name.clone()).collect(),
            span,
        }));
        out.push(Stmt::Rule(RuleStmt {
            head: atom_at(&pred, vars, span),
            body,
        }));
        Ok(out)
    }

    /// `stack app[k, ...] { config = FORMAT(SOURCE) }`: every leaf of the
    /// document is a contribution to the settings row of the deployment
    /// (named by the key's value, several keys' joined by `/`):
    /// `arg("settings", Row, P, V, normal) :- reads, table.FORMAT.stack.config(Path, At, P, V)`.
    fn stack_config(
        &mut self,
        a: &SyntaxNode,
        stack: &str,
        keys: &[SyntaxToken],
        scope: usize,
        outer: &Rc,
    ) -> L<Vec<Stmt>> {
        let span = self.span(a);
        if keys.is_empty() {
            let d = Diagnostic::error(
                span,
                format!("stack {stack} has no key: its config would be every deployment's"),
            )
            .with_help(format!(
                "key it by the inputs that name a deployment, `stack {stack}[env]`, \
                 or state the settings in the program"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let mut rc = self.rc(a, scope, outer);
        let mut body = Vec::new();
        let mut row = Vec::new();
        for k in keys {
            let at: u32 = k.text_range().start().into();
            row.push(self.hole(&mut rc, k.text(), at, &mut body)?);
        }
        let row = match row.len() {
            1 => row.remove(0),
            n => {
                let mut args = vec![str_term(&vec!["%s"; n].join("/"))];
                args.extend(row);
                func("format", args)
            }
        };
        let (path, value) = (var(&fresh(&mut rc, "Path")), var(&fresh(&mut rc, "Value")));
        let cols = [("path", "string"), ("value", "any")]
            .map(|(name, ty)| BindArg {
                input: false,
                name: name.into(),
                ty: Some(TypeExpr::Name(ty.into())),
            })
            .to_vec();
        let src = terms(a).next().ok_or(Skip)?;
        let mut out = self.table_body(
            &mut rc,
            &src,
            crate::tables::STACK_CONFIG,
            cols,
            vec![path.clone(), value.clone()],
            &mut body,
        )?;
        self.check_bound(&rc, &body, &[])?;
        out.push(Stmt::Rule(RuleStmt {
            head: atom_at(
                "arg",
                vec![
                    str_term("settings"),
                    row,
                    path,
                    value,
                    str_term(crate::transform::NORMAL),
                ],
                span,
            ),
            body,
        }));
        Ok(out)
    }

    /// A table's source, `FORMAT(PATH)` or `FORMAT(git(REPO, REF, PATH))`,
    /// read into `body` and asked of its externs there, their outputs
    /// `outs`; the extern declarations.
    fn table_body(
        &mut self,
        rc: &mut Rc,
        src: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
        outs: Vec<Term>,
        body: &mut Vec<Lit>,
    ) -> L<Vec<Stmt>> {
        let span = self.span(src);
        let formats = crate::tables::FORMATS;
        let bad = |l: &mut Self, what: &str| -> L<Vec<Stmt>> {
            let d = Diagnostic::error(span, format!("{what}: a table's source is FORMAT(SOURCE)"))
                .with_help(format!(
                    "FORMAT is one of {}; SOURCE is a path, `\"data/p.csv\"`, or \
                     `git(\"repo\", \"ref\", \"path\")`",
                    formats.join(", ")
                ));
            l.diags.push(d);
            Err(Skip)
        };
        let format = match (src.kind(), self.callee(src)) {
            (CALL, Some(f)) if formats.contains(&f.as_str()) => f,
            (CALL, Some(f)) => return bad(self, &format!("unknown format {f}")),
            _ => return bad(self, "not a format"),
        };
        let args: Vec<SyntaxNode> = node(src, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let [arg] = args.as_slice() else {
            return bad(self, &format!("{format} takes one source"));
        };
        let git = arg.kind() == CALL && self.callee(arg).as_deref() == Some("git");
        let parts: Vec<SyntaxNode> = if git {
            node(arg, ARG_LIST)
                .map(|l| terms(&l).collect())
                .unwrap_or_default()
        } else {
            vec![arg.clone()]
        };
        if parts.len() != if git { 3 } else { 1 } {
            return bad(self, "git takes a repository, a ref and a path");
        }
        let names: &[&str] = if git {
            &["repo", "ref", "path"]
        } else {
            &["path"]
        };
        let mut given = Vec::new();
        for (t, name) in parts.iter().zip(names) {
            let t = self.term(rc, t, Pos::Content, body)?;
            let v = var(&fresh(rc, &capitalise(name)));
            body.push(Lit::Eq(v.clone(), t));
            given.push(v);
        }
        let plus = |name: &str| BindArg {
            input: true,
            name: name.into(),
            ty: None,
        };
        let mut out = Vec::new();
        let (mut ins, mut args) = (Vec::new(), Vec::new());
        if git {
            let commit = var(&fresh(rc, "Commit"));
            let name = crate::tables::extern_name("git", table);
            body.push(Lit::Pos(atom_at(
                &name,
                vec![given[0].clone(), given[1].clone(), commit.clone()],
                span,
            )));
            out.push(Stmt::ExternFn(ExternFn {
                name,
                args: vec![
                    plus("repo"),
                    plus("ref"),
                    BindArg {
                        input: false,
                        name: "commit".into(),
                        ty: None,
                    },
                ],
                persist: false,
                span,
            }));
            ins.extend([plus("repo"), plus("commit"), plus("path")]);
            args.extend([given[0].clone(), commit, given[2].clone()]);
        } else {
            ins.push(plus("path"));
            args.push(given[0].clone());
        }
        let at = var(&fresh(rc, "At"));
        args.push(at);
        args.extend(outs);
        ins.push(BindArg {
            input: false,
            name: "at".into(),
            ty: None,
        });
        ins.extend(cols);
        let name = crate::tables::extern_name(&format, table);
        body.push(Lit::Pos(atom_at(&name, args, span)));
        out.push(Stmt::ExternFn(ExternFn {
            name,
            args: ins,
            persist: false,
            span,
        }));
        Ok(out)
    }

    /// `output k [: T] = t [where B]` (H-7): the declaration, when typed, and
    /// its value; a value that reads, or one with a condition, is the rule
    /// `output(k, t') :- B, reads`.
    fn output(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let mut out = Vec::new();
        // The declaration, once per scope (an output may have several rows);
        // with no type written, any.
        let ty = match node(n, TYPE_EXPR) {
            Some(t) => match self.resource_type(&t) {
                Some(_) => TypeExpr::Name("addr".to_string()),
                None => self.type_expr(&t),
            },
            None => TypeExpr::Name("any".to_string()),
        };
        if self.outputs.insert((scope, name.clone())) {
            out.push(Stmt::Output(OutputDecl {
                name: name.clone(),
                ty: Some(ty),
                value: None,
                span,
            }));
        }
        let Some(t) = terms(n).next() else {
            let d = Diagnostic::error(span, format!("output {name} has no value")).with_help(
                format!("an output is one statement: `output {name}: T = term`"),
            );
            self.diags.push(d);
            return Err(Skip);
        };
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        // A bare resource name is its address.
        let value = match Chain::of(&t) {
            Some(c)
                if c.is_bare()
                    && !rc.vars.contains_key(&c.head)
                    && self.resource(scope, &c.head).is_some() =>
            {
                str_term(&c.head)
            }
            _ => self.term(&mut rc, &t, Pos::Whole, &mut pre)?,
        };
        if pre.is_empty() && !has_body {
            self.check_bound(&rc, &[], &[&value])?;
            out.push(Stmt::Output(OutputDecl {
                name,
                ty: None,
                value: Some(value),
                span,
            }));
            return Ok(out);
        }
        let head = Atom {
            pred: "output".to_string(),
            args: vec![str_term(&name), value],
            record: None,
            span,
        };
        self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?;
        out.push(Stmt::Rule(RuleStmt { head, body: pre }));
        Ok(out)
    }

    /// The clause of a block statement: the `where` body after its block.
    fn clauses(&mut self, rc: &mut Rc, stmt: &SyntaxNode) -> L<Vec<Lit>> {
        let mut out = Vec::new();
        let mut failed = false;
        for c in stmt.children().filter(|c| c.kind() == CLAUSE) {
            match node(&c, BODY).map(|b| self.body(rc, &b)) {
                Some(Ok(ls)) => out.extend(ls),
                _ => failed = true,
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
        if name == "_" {
            return self.error(
                span,
                format!("an instance is written by its name: `instance {module} _` names nothing"),
            );
        }
        if self.is_value(scope, &name) {
            return self.error(
                span,
                format!(
                    "`{name}` is a value in scope, but an instance's name is literal: this is the \
                     instance {module}.{name}; name it otherwise"
                ),
            );
        }
        let block = node(n, BLOCK).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, n)?;
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

    /// `resource T n { f = t ... } where B` and `settings e { ... } where B`.
    fn block_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let block = node(n, BLOCK).ok_or(Skip)?;
        let header = self.header_token(n).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, n)?;
        let mut reads = Vec::new();
        let fields = self.fields(&mut rc, &block, &mut reads)?;
        let reads_at = body.len()..body.len() + reads.len();
        body.extend(reads);
        // The header: a string with holes is bound last, by `format`; a
        // name the clauses bind is that variable; anything else static.
        let name = if header.kind() == STRING && has_hole(header.text()) {
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
                _ if text == "_" && n.kind() == SETTINGS => {
                    // `settings _`: every settings row that exists.
                    let v = fresh(&mut rc, "Row");
                    body.push(Lit::Pos(atom_at(
                        crate::transform::SETTINGS_ROW,
                        vec![var(&v)],
                        span,
                    )));
                    var(&v)
                }
                _ if text == "_" => {
                    return self.error(
                        self.span_of(header.text_range()),
                        "a resource is written by its name: `_` names nothing; name it, or \
                         bind the name in the clause (`resource T n { .. } where p(n)`)",
                    );
                }
                _ if self.is_value(scope, text) => {
                    let (kind, every) = if n.kind() == SETTINGS {
                        ("settings row", "`settings _` for every row, or ")
                    } else {
                        ("resource", "")
                    };
                    let d = Diagnostic::error(
                        self.span_of(header.text_range()),
                        format!(
                            "`{text}` is a value in scope, but a header name the clause does not \
                             bind is the {kind}'s literal name: this is the {kind} \"{text}\""
                        ),
                    )
                    .with_help(format!(
                        "write {every}`\"{text}\"` for a {kind} named \"{text}\""
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
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
                reads: reads_at,
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
        let head_node = n.children().find(|c| c.kind() == CALL).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let calls = match self.callee(&head_node).as_deref() {
            Some("type_refine") => Calls::Data,
            _ => Calls::Head,
        };
        let mut head = self.calls(calls, |l| {
            l.atom(&mut rc, &head_node, Pos::Whole, &mut body)
        })?;
        if let Some(rank) = self.rank_tok(n)? {
            if head.pred != "arg" || head.args.len() != 4 || head.record.is_some() {
                return self.error(
                    span,
                    "a rank applies to an `arg(T, A, Path, Value)` head only",
                );
            }
            head.args.push(str_term(rank.name()));
        }
        self.core_head(&head_node, &head, has_body)?;
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        Ok(vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }])
    }

    /// `let k = t [where B]` (H-6): the relation `k(t)`, read by name. When
    /// `t` is a reference, `k`'s value is that reference and a dot on `k`
    /// reads through it.
    fn let_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Err(e) = self.value_type(scope, &name) {
            return self.error(span, e);
        }
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let t = terms(n).next().ok_or(Skip)?;
        let value = self.let_value(&mut rc, &t, &mut body)?;
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

    /// A `let`'s value: a reference is its key (a settings row's, a
    /// resource's address, a live object's name); anything else the term.
    fn let_value(&mut self, rc: &mut Rc, t: &SyntaxNode, body: &mut Vec<Lit>) -> L<Term> {
        if let Some(c) = Chain::of(t) {
            let mut pre = Vec::new();
            let mut rc2 = rc.clone();
            if let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) {
                let key = match &res {
                    Res::Settings { addr, path } if path.is_empty() => Some(addr.clone()),
                    Res::Ref { addr, path, .. } if path.is_empty() => Some(addr.clone()),
                    Res::World { addr, path, .. } if path.is_empty() => Some(addr.clone()),
                    _ => None,
                };
                if let Some(k) = key {
                    *rc = rc2;
                    body.extend(pre);
                    return Ok(k);
                }
            }
        }
        self.term(rc, t, Pos::Whole, body)
    }

    /// `set chain (=|+=) t [@rank] [where B]` (H-5): a contribution to a
    /// resource's attribute (`arg(T, A, p, t)`), a settings row's leaf, or
    /// an input (a stack input's `input(k, t)`, a module instance's).
    fn set(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let lhs = n.children().find(|c| c.kind() == CHAIN).ok_or(Skip)?;
        let rhs = terms(n).find(|t| *t != lhs).ok_or(Skip)?;
        let c = Chain::of(&lhs).ok_or(Skip)?;
        let add = tokens(n).any(|t| t.kind() == PLUS_EQ);
        let rank = self.rank_tok(n)?;
        if add && rank.is_some() {
            return self.error(span, "a rank applies to `=`, not `+=`");
        }
        let (typ, addr, path, block) = match self.set_target(&mut rc, &c, &mut body, span)? {
            Target::Cell(typ, addr, path, block) => (typ, addr, path, block),
            Target::Input(k) => {
                // A stack input: `input(k, t)`, as `--set k=t` gives it.
                if !self.decls.scenarios.contains(&scope) && !has_body {
                    let d = Diagnostic::error(
                        span,
                        format!("`set {k}` at the top of the program sets the program's own input"),
                    )
                    .with_help(format!(
                        "give `input {k}` a default, or pass `--set {k}=...` on the command line"
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                if add || rank.is_some() {
                    return self.error(span, "an input is set with `=` and no rank");
                }
                let value = self.term(&mut rc, &rhs, Pos::Whole, &mut body)?;
                let head = atom_at("input", vec![str_term(&k), value], span);
                self.check_bound(&rc, &body, &atom_terms(&head))?;
                return Ok(vec![if body.is_empty() && !has_body {
                    Stmt::Fact(head)
                } else {
                    Stmt::Rule(RuleStmt { head, body })
                }]);
            }
        };
        if let Some(block) = block
            && !has_body
        {
            let d = Diagnostic::error(
                self.span(&lhs),
                format!(
                    "`set {}` with no condition is an entry of `{block}`, declared in the same \
                     scope",
                    lhs.text()
                ),
            )
            .with_help(format!(
                "write `{path} = ...` in the block `{block} {{ .. }}`"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let value = self.term(&mut rc, &rhs, Pos::Whole, &mut body)?;
        let mut args = vec![typ, addr, str_term(&path), value];
        if let Some(rank) = rank {
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

    /// What a `set` sets: a cell `(T, A, path)` and, when the block that
    /// owns the cell is declared in the same scope, how that block is
    /// written; or a stack input.
    fn set_target(&mut self, rc: &mut Rc, c: &Chain, body: &mut Vec<Lit>, span: Span) -> L<Target> {
        let scope = rc.scope;
        // A stack input, set by name.
        if c.is_bare() && self.is_value(scope, &c.head) && self.find_let(scope, &c.head).is_none() {
            return Ok(Target::Input(c.head.clone()));
        }
        // A module instance's input: `m.i.k`.
        if let [Op::Field(i), Op::Field(k)] = c.ops.as_slice()
            && self.decls.modules.contains_key(&c.head)
            && self
                .decls
                .instances
                .get(&c.head)
                .is_some_and(|s| s.contains(i))
        {
            let own = self.decls.scopes[self.decl_scope(scope)]
                .instances
                .contains(&(c.head.clone(), i.clone()));
            return Ok(Target::Cell(
                str_term(crate::modules::INPUT),
                str_term(&format!("{}.{i}", c.head)),
                k.clone(),
                own.then(|| format!("instance {} {i}", c.head)),
            ));
        }
        // `set T[_].p = t`: every resource of `T` is `r in T` (H-5).
        if let Some(k) = c.ops.iter().position(|o| {
            matches!(o, Op::Index(ts, _) if ts.len() == 1
                && Chain::of(&ts[0]).is_some_and(|x| x.head == "_" && x.is_bare()))
        }) {
            let typ: Vec<&str> = std::iter::once(c.head.as_str())
                .chain(c.ops[..k].iter().filter_map(|o| match o {
                    Op::Field(f) => Some(f.as_str()),
                    Op::Index(..) => None,
                }))
                .collect();
            let typ = typ.join(".");
            let path: String = c.ops[k + 1..]
                .iter()
                .filter_map(|o| match o {
                    Op::Field(f) => Some(format!(".{f}")),
                    Op::Index(..) => None,
                })
                .collect();
            return self.error(
                span,
                format!(
                    "`{typ}[_]` is every resource of `{typ}`: write `set r{path} = .. where r in {typ}`"
                ),
            );
        }
        let mut pre = Vec::new();
        let res = self.resolve(rc, c, &mut pre)?;
        body.extend(pre);
        let (typ, addr, path) = match res {
            Res::Ref { typ, addr, path } if !path.is_empty() => (typ, addr, path),
            Res::Settings { addr, path } if !path.is_empty() => (str_term("settings"), addr, path),
            _ => {
                return self.error(
                    span,
                    "`set` sets a resource's attribute (`r.tags`, `T[e].p`), a settings row's \
                     leaf (`settings[e].p`) or an input",
                );
            }
        };
        let Some(path) = path_string(&path) else {
            return self.error(span, "a contribution's path is constant");
        };
        let s = &self.decls.scopes[self.decl_scope(scope)];
        let block = match (&typ, &addr) {
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) if t == "settings" => {
                s.settings.contains(a).then(|| format!("settings {a}"))
            }
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) => s
                .resources
                .get(a)
                .is_some_and(|ts| ts.contains(t))
                .then(|| format!("resource {t} {a}")),
            _ => None,
        };
        Ok(Target::Cell(typ, addr, path, block))
    }

    /// A core relation written where a surface form says it (H-15): an
    /// error that prints the surface form. The core stays writable where
    /// nothing else reaches: a variable type or path, a read of the
    /// contributions before they merge (`arg` in a body).
    fn core_head(&mut self, n: &SyntaxNode, head: &Atom, has_body: bool) -> L<()> {
        if self.lenient || self.any_type || self.text || self.core {
            return Ok(());
        }
        let a = arg_texts(n);
        let s = |i: usize| match head.args.get(i) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let cond = if has_body { " where .." } else { "" };
        let surface = match (head.pred.as_str(), head.args.len()) {
            ("want", 2) if s(0).is_some() => Some(format!(
                "a resource is declared by a block: `resource {} {} {{ .. }}`",
                s(0).unwrap(),
                a[1]
            )),
            ("arg" | "arg_add", 4 | 5) if s(0).is_some() && s(2).is_some() => {
                let op = if head.pred == "arg" { "=" } else { "+=" };
                let (t, p) = (s(0).unwrap(), s(2).unwrap());
                let target = match t.as_str() {
                    "settings" => format!("settings[{}]{}", a[1], crate::ir::path_suffix(&p)),
                    crate::modules::INPUT if s(1).as_deref() == Some("") => p.clone(),
                    crate::modules::INPUT => format!("{}.{p}", s(1).unwrap_or_default()),
                    _ => format!("{t}[{}]{}", a[1], crate::ir::path_suffix(&p)),
                };
                Some(format!("`set {target} {op} {}{cond}`", a[3]))
            }
            ("setting", 3) if s(1).is_some() => Some(format!(
                "`set settings[{}]{} = {}{cond}`",
                a[0],
                crate::ir::path_suffix(&s(1).unwrap()),
                a[2]
            )),
            ("output", 2) if s(0).is_some() => {
                Some(format!("`output {} = {}{cond}`", s(0).unwrap(), a[1]))
            }
            ("input", 2) if s(0).is_some() => Some(format!(
                "`set {} = {}{cond}` (in a scenario)",
                s(0).unwrap(),
                a[1]
            )),
            ("deny" | "warn", 1 | 2) if s(0).is_some() => Some(format!(
                "`{} {}{}{cond}`",
                head.pred,
                a[0],
                a.get(1)
                    .filter(|o| !o.is_empty())
                    .map(|o| format!(" {o}"))
                    .unwrap_or_default()
            )),
            _ => None,
        };
        let Some(surface) = surface else {
            return Ok(());
        };
        let d = Diagnostic::error(
            head.span,
            format!(
                "`{}` is the core's spelling of a surface form",
                n.text().to_string().trim()
            ),
        )
        .with_help(format!("write {surface} (H-15)"));
        self.diags.push(d);
        Err(Skip)
    }

    /// A body's read of a core relation a surface form says (H-15), of
    /// `member` (H-9), or of `deny`/`warn` (H-8).
    fn core_read(&mut self, n: &SyntaxNode, atom: &Atom) -> L<()> {
        if self.lenient || self.any_type || self.text || self.core {
            return Ok(());
        }
        let a = arg_texts(n);
        let s = |i: usize| match atom.args.get(i) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let (msg, help) = match (atom.pred.as_str(), atom.args.len()) {
            ("deny" | "warn", _) => (
                format!("a rule reads `{}`", atom.pred),
                "a deny or a warn is checked after evaluation: no rule may read deny/2 or \
                 warn/2 (H-8); read what the deny reads instead"
                    .to_string(),
            ),
            ("member", 2) => (
                "`member` is what `in` lowers to".to_string(),
                format!("write `{} in {}` (H-9)", a[1], a[0]),
            ),
            ("member", 3) => (
                "`member` is what an index lowers to".to_string(),
                format!("write `{} = {}[{}]` (H-9)", a[2], a[0], a[1]),
            ),
            ("want", 2) if s(0).is_some() => (
                format!("`{}` is the core's spelling of `in`", n.text()),
                format!("write `{} in {}` (H-15)", a[1], s(0).unwrap()),
            ),
            ("attr", 4) if s(0).is_some() && s(2).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = {}[{}]{}` (H-15)",
                    a[3],
                    s(0).unwrap(),
                    a[1],
                    crate::ir::path_suffix(&s(2).unwrap())
                ),
            ),
            ("setting", 3) if s(1).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = settings[{}]{}` (H-15)",
                    a[2],
                    a[0],
                    crate::ir::path_suffix(&s(1).unwrap())
                ),
            ),
            ("output", 3) if s(0).is_some() && s(1).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = {}.{}` (H-15)",
                    a[2],
                    s(0).unwrap(),
                    s(1).unwrap()
                ),
            ),
            ("cloud_attr", 4) if s(0).is_some() && s(2).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = world.{}[{}]{}` (H-15)",
                    a[3],
                    s(0).unwrap(),
                    a[1],
                    crate::ir::path_suffix(&s(2).unwrap())
                ),
            ),
            ("cloud_exists", 2) if s(0).is_some() => (
                format!("`{}` is the core's spelling of `in`", n.text()),
                format!("write `{} in world.{}` (H-15)", a[1], s(0).unwrap()),
            ),
            _ => return Ok(()),
        };
        let d = Diagnostic::error(atom.span, msg).with_help(help);
        self.diags.push(d);
        Err(Skip)
    }

    /// `deny "m" {o} where B`, `warn ...`.
    fn check(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let kw = tokens(n).next().ok_or(Skip)?;
        let msg = tokens(n).find(|t| t.kind() == STRING).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        // The message is a string like any other: `${e}` reads the body's
        // variables (H-13).
        let message = self.string_term(&mut rc, &msg, &mut body)?;
        let mut args = vec![message];
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
            let d = Diagnostic::error(span, format!("unknown name `{src}`"))
                .with_help(format!(
                    "a variable is bound by a relation or an equality in the body; a string is \
                     quoted: \"{src}\""
                ))
                .with_fix(
                    format!("quote it: \"{src}\""),
                    vec![(span, format!("\"{src}\""))],
                );
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
                self.core_read(&a, &atom)?;
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
            LIT_HAS => {
                let c = terms(n).next().and_then(|t| Chain::of(&t)).ok_or(Skip)?;
                let res = self.resolve(rc, &c, out)?;
                match self.read_atom(rc, &res, Term::Wildcard, span) {
                    Some(a) => out.push(Lit::Pos(a)),
                    // A nested path, or a field of a value: it has a value
                    // when the walk to it does.
                    None if matches!(
                        res,
                        Res::Ref { .. }
                            | Res::Var { .. }
                            | Res::Value { .. }
                            | Res::Settings { .. }
                    ) =>
                    {
                        let t = self.realize(rc, res, Pos::Content, out, span)?;
                        let v = fresh(rc, "Has");
                        out.push(Lit::Eq(var(&v), t));
                        rc.outer.insert(v);
                    }
                    None => {
                        return self.error(
                            span,
                            "`has` takes an attribute of a resource (`has r.p`), a settings \
                             leaf, a value name or a field of a value",
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
                self.core_read(&a, &atom)?;
                out.push(Lit::Not(atom));
                return Ok(());
            }
            LIT_IN => {
                let lit = self.membership(rc, n, out)?;
                out.push(negate(lit));
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
        // `pattern = e[i]` (or `e[i] = pattern`): an element matched by a
        // pattern is `member(e, i, pattern)` (H-9).
        if ts.len() == 2 && ops.as_slice() == [EQ] {
            for (c, p) in [(&ts[1], &ts[0]), (&ts[0], &ts[1])] {
                if !matches!(p.kind(), OBJECT | LIST) {
                    continue;
                }
                let Some(ch) = Chain::of(c) else { continue };
                let Some(Op::Index(ix, _)) = ch.ops.last() else {
                    continue;
                };
                if ix.len() != 1 {
                    continue;
                }
                let ix = ix[0].clone();
                let mut list = ch.clone();
                list.ops.pop();
                let res = self.resolve(rc, &list, out)?;
                let span = self.span(n);
                let l = self.realize(rc, res, Pos::Content, out, span)?;
                let i = self.bind(true, |x| x.term(rc, &ix, Pos::Content, out))?;
                let pat = self.bind(true, |x| x.term(rc, p, Pos::Content, out))?;
                out.push(Lit::Pos(atom_at("member", vec![l, i, pat], span)));
                return Ok(());
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

    fn calls<T>(&mut self, calls: Calls, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.calls, calls);
        let r = f(self);
        self.calls = saved;
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
                    // The type on the right picks among resources of one name.
                    let named = self.resource(rc.scope, &c.head).unwrap_or_default();
                    match &typ {
                        Some(Term::Val(Value::Str(t))) if named.contains(t) => str_term(&c.head),
                        _ => self.reference(rc, &c, out, span)?.1,
                    }
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
            _ => self.error(span, "expected a resource: a name in scope, or `T[e]`"),
        }
    }

    fn ambiguous<T>(&mut self, name: &str, types: &[String], span: Span) -> L<T> {
        let list = types
            .iter()
            .map(|t| {
                crate::ir::Address {
                    typ: t.clone(),
                    name: name.to_string(),
                }
                .to_string()
            })
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

    /// A relation atom, its arguments lowered at `pos`: positional, or
    /// named by the relation's columns (`p(a: x)`, H-12).
    fn atom(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Atom> {
        let span = self.span(n);
        let pred = self.callee(n).ok_or(Skip);
        let Ok(pred) = pred else {
            return self.error(span, "a relation is named by a plain name (`p` or `m.i.p`)");
        };
        let list = node(n, ARG_LIST);
        let named: Vec<SyntaxNode> = list
            .iter()
            .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG))
            .collect();
        if !named.is_empty() {
            if list.iter().any(|l| terms(l).next().is_some()) {
                return self.error(span, "an atom's arguments are all positional or all named");
            }
            let mut fields = BTreeMap::new();
            for f in named {
                let key = tokens(&f).next().ok_or(Skip)?.text().to_string();
                let value = self.term(rc, &terms(&f).next().ok_or(Skip)?, pos, pre)?;
                if fields.insert(key.clone(), value).is_some() {
                    return self.error(self.span(&f), format!("column `{key}` given twice"));
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

    /// `sum` of a value known here not to be an int, `min`/`max` of one
    /// neither an int nor a string: an error at the call rather than a
    /// deny of every group.
    fn check_aggregated(&mut self, name: &str, args: &[Term], span: Span) {
        if self.lenient || self.calls != Calls::Head {
            return;
        }
        let kind = match args {
            [Term::Val(Value::Int(_))] => "an int",
            [Term::Val(Value::Str(_))] => "a string",
            [Term::Func { name, .. }]
                if crate::functions::get(name).is_some_and(|f| f.ret == "string") =>
            {
                "a string"
            }
            [Term::Val(Value::Bool(_))] => "a bool",
            [Term::Val(Value::List(_)) | Term::List(_) | Term::ListComp { .. }] => "a list",
            [Term::Val(Value::Obj(_)) | Term::Obj(_)] => "an object",
            _ => return,
        };
        let ok: &[&str] = match name {
            "sum" => &["an int"],
            "min" | "max" => &["an int", "a string"],
            _ => return,
        };
        if !ok.contains(&kind) {
            self.diags.push(Diagnostic::error(
                span,
                format!(
                    "`{name}` aggregates {}, not {kind}",
                    if ok.len() == 1 {
                        "ints"
                    } else {
                        "ints or strings"
                    }
                ),
            ));
        }
    }

    /// A call to a function the evaluator does not have would have no value
    /// and fail its literal quietly: an error at the call. A refinement's
    /// calls are `refine::check_rest`'s, with the refinement's own message.
    fn check_function(&mut self, name: &str, span: Span) {
        if self.lenient || self.calls == Calls::Data || crate::functions::callable(name) {
            return;
        }
        if crate::partition::AGGREGATES.contains(&name) {
            if self.calls != Calls::Head {
                self.diags.push(Diagnostic::error(
                    span,
                    format!("`{name}` is an aggregate: it is written in a rule head"),
                ));
            }
            return;
        }
        self.diags.push(crate::functions::unknown(span, name));
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
                    _ => Ok(Term::Val(Value::Bool(false))),
                }
            }
            CHAIN => {
                let c = Chain::of(n).ok_or(Skip)?;
                let res = self.resolve(rc, &c, pre)?;
                self.realize(rc, res, pos, pre, span)
            }
            CALL => {
                if let Some(t) = self.env_var_call(rc, n, pos, pre) {
                    return t;
                }
                let name = self.callee(n);
                let Some(name) = name else {
                    return self.error(span, "a function is named by a plain name");
                };
                if node(n, ARG_LIST).is_some_and(|l| node(&l, NAMED_ARG).is_some()) {
                    return self.error(
                        span,
                        format!(
                            "`{name}` is a function here: named arguments name a relation's \
                             columns in an atom"
                        ),
                    );
                }
                self.check_function(&name, span);
                let args = self.bind(false, |l| l.args(rc, n, Pos::Content, pre))?;
                self.check_aggregated(&name, &args, span);
                Ok(Term::Func { name, args })
            }
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

    /// A string literal: `"a${e}b"` is `format("a%sb", e)` (H-13), `$${`
    /// is a literal `${`, and a brace is itself.
    fn string_term(&mut self, rc: &mut Rc, t: &SyntaxToken, pre: &mut Vec<Lit>) -> L<Term> {
        let text = t.text();
        if self.text || !text.contains("${") {
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
                b'$' if bytes.get(i + 1) == Some(&b'$') && bytes.get(i + 2) == Some(&b'{') => {
                    lit.push_str("${");
                    i += 3;
                }
                b'$' if bytes.get(i + 1) == Some(&b'{') => {
                    let mut depth = 1;
                    let mut j = i + 2;
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
                            "an interpolation `${` is never closed; a literal `${` is `$${`",
                        );
                    }
                    flush(&mut lit, &mut fmt, self)?;
                    fmt.push_str("%s");
                    let hole = &inner[i + 2..j - 1];
                    // +1: the opening quote.
                    let at = base + 1 + i as u32 + 2;
                    args.push(self.bind(false, |l| l.hole(rc, hole, at, pre))?);
                    i = j;
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
                let msg = e
                    .message
                    .replace("the end of the file", "the end of the hole");
                return self.error(span, format!("in an interpolation: {msg}"));
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
        if h == "_" {
            return self.error(
                span,
                "`_` is a placeholder and is never accessed: `_.p` and `_[k]` read nothing; \
                 name it (`env.p`)",
            );
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
            if self.is_value(rc.scope, h) {
                return self.value(rc, c, pre, span);
            }
            // `settings.x` names a resource called `settings` in scope; a
            // settings row is only ever `settings[e]`.
            let resource =
                matches!(c.ops.first(), Some(Op::Field(_))) && self.resource(rc.scope, h).is_some();
            if c.head_kind == SETTINGS_KW && !c.is_bare() && !resource {
                return self.settings(rc, c, pre, span);
            }
            if h == "world" && !c.ops.is_empty() {
                return self.world(rc, c, pre, span);
            }
        }
        if c.is_bare() {
            // `settings` alone is the pseudo-type's name (`type_lattice`).
            if c.head_kind == SETTINGS_KW && !rc.vars.contains_key(h) {
                return Ok(Res::Type(h.to_string()));
            }
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
        // A dotted name in a type namespace is a type's name, and must be a
        // known type (H-10): a typo is an error, never a string.
        if c.ops.iter().all(|o| matches!(o, Op::Field(..)))
            && (self.decls.namespaces.contains(h) || self.any_type)
        {
            let name = c.fields().join(".");
            if self.any_type
                || self.decls.types.contains(&name)
                || (!c.ops.is_empty() && !self.decls.closed.contains(h))
            {
                return Ok(Res::Type(name));
            }
            return self.error(
                span,
                format!(
                    "unknown type `{name}`: no resource header, `type` block or provider schema \
                     declares it"
                ),
            );
        }
        let quoted = format!("\"{}\"", c.fields().join("."));
        let mut d = Diagnostic::error(span, format!("unknown name `{h}`")).with_help(format!(
            "a resource, module, input, `let` or type is declared before it is read; a string \
             is quoted: {quoted}"
        ));
        if c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            d = d.with_fix(format!("quote it: {quoted}"), vec![(span, quoted)]);
        }
        self.diags.push(d);
        Err(Skip)
    }

    /// A value name: `k(V)`, read once per rule. A `let` holding a
    /// reference (H-6) reads through it: `cfg.x` is `cfg(E), setting(E,
    /// "x", V)`.
    fn value(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let pred = c.head.clone();
        let ty = match self.value_type(rc.scope, &pred) {
            Ok(t) => t,
            Err(e) => return self.error(span, e),
        };
        let Some(ty) = ty.filter(|_| !c.is_bare()) else {
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Value { pred, path });
        };
        let key = match rc.values.get(&pred) {
            Some(v) => var(v),
            None => {
                let name = fresh(rc, &capitalise(&pred));
                pre.push(Lit::Pos(atom_at(&pred, vec![var(&name)], span)));
                rc.values.insert(pred.clone(), name.clone());
                var(&name)
            }
        };
        match ty {
            VType::Settings => {
                let path = self.segs(rc, &c.ops, pre)?;
                Ok(Res::Settings { addr: key, path })
            }
            VType::Ref(typ) => {
                let path = self.segs(rc, &c.ops, pre)?;
                Ok(Res::Ref {
                    typ: str_term(&typ),
                    addr: key,
                    path,
                })
            }
            VType::World(typ) => {
                let segs = self.segs(rc, &c.ops, pre)?;
                let Some(path) = path_string(&segs) else {
                    return self.error(span, "a live object's path is constant");
                };
                Ok(Res::World {
                    typ,
                    addr: key,
                    path,
                })
            }
        }
    }

    /// A bare name: a variable, unless it names something no variable may.
    fn bare(&mut self, rc: &mut Rc, h: &str, span: Span) -> L<Res> {
        // A type named by one word (`gke_nodepool`) is that type.
        if !rc.vars.contains_key(h) && self.decls.types.contains(h) {
            return Ok(Res::Type(h.to_string()));
        }
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
            }
        }
        Ok(out)
    }

    /// `settings[e].path`: a settings row by its key.
    fn settings(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let addr = match c.ops.first() {
            Some(Op::Index(ts, _)) if ts.len() == 1 => {
                self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?
            }
            Some(Op::Field(n)) => {
                let d = Diagnostic::error(
                    span,
                    format!("a settings row is named by its key: `settings[\"{n}\"]`"),
                )
                .with_help("`.` is static, `[ ]` is a key (H section 5.1)");
                self.diags.push(d);
                return Err(Skip);
            }
            _ => return self.error(span, "a settings row is read as `settings[e].path`"),
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

    /// `m.i.k` (an output), `m[e].k`, `m.i` (the instance scope).
    fn module_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let m = c.head.as_str();
        if !self.decls.modules.contains_key(m) {
            return Ok(None);
        }
        let (inst, rest) = match c.ops.first() {
            Some(Op::Field(i)) if self.decls.instances.get(m).is_some_and(|s| s.contains(i)) => {
                (str_term(&format!("{m}.{i}")), &c.ops[1..])
            }
            Some(Op::Index(ts, _)) if ts.len() == 1 => {
                let e = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                (
                    func("format", vec![str_term(&format!("{m}.%s")), e]),
                    &c.ops[1..],
                )
            }
            _ => return Ok(None),
        };
        match rest.first() {
            None => Ok(Some(Res::Val(inst))),
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
            Some(_) => self.error(span, "after an instance: `.output`").map(Some),
        }
    }

    /// `T[e].path` for a resource of type `T` by its key, and the lookups
    /// `p[a, b]` and `ext[a]`. `T.n`, a resource by a dot after its type,
    /// is an error that names the spelling (H-10).
    fn typed_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let fields = c.fields();
        let k = fields.len();
        // `T.n`: the longest `T` with a resource `n` of that type in scope.
        for i in (1..k).rev() {
            let typ = fields[..i].join(".");
            if self.resource_of_type(rc.scope, &typ, &fields[i]) {
                let n = &fields[i];
                let unique = self.resource(rc.scope, n).is_some_and(|ts| ts.len() == 1);
                let write = if unique {
                    n.clone()
                } else {
                    crate::ir::Address {
                        typ,
                        name: n.clone(),
                    }
                    .to_string()
                };
                let d = Diagnostic::error(
                    span,
                    format!(
                        "`{}` names a resource by a dot after its type",
                        fields[..=i].join(".")
                    ),
                )
                .with_help(format!(
                    "a resource in scope is named `{write}` (H-10); `.` is static, `[ ]` a key"
                ));
                self.diags.push(d);
                return Err(Skip);
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
        // `T[e]` in a namespace no declaration or builtin schema names: a
        // provider schema's type the compiler does not read.
        let foreign = !self.decls.closed.contains(&c.head);
        if self.decls.types.contains(&name) || (k > 1 && (self.any_type || foreign)) {
            if ts.len() != 1 {
                return self.error(span, "a resource is `T[key]`").map(Some);
            }
            // `T["n"]` for a resource `n` in scope: it is named `n` (H-10).
            if !self.any_type
                && let Some(LITERAL) = ts.first().map(|t| t.kind())
                && let Some(s) = tokens(&ts[0]).find(|t| t.kind() == STRING)
                && let Ok(n) = string_value(s.text())
                && !has_hole(s.text())
                && self
                    .resource(rc.scope, &n)
                    .is_some_and(|types| types == vec![name.clone()])
            {
                let d = Diagnostic::error(
                    span,
                    format!("`{name}[\"{n}\"]` names the resource `{n}` in scope"),
                )
                .with_help(format!("write `{n}` (H-10)"));
                self.diags.push(d);
                return Err(Skip);
            }
            let addr = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Ref {
                typ: str_term(&name),
                addr,
                path,
            }));
        }
        if k > 1 && self.decls.closed.contains(&c.head) {
            return self
                .error(
                    span,
                    format!(
                        "unknown type `{name}`: no resource header, `type` block or provider \
                         schema declares it"
                    ),
                )
                .map(Some);
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

/// What a `set` sets.
enum Target {
    /// `(T, A, path)`, and the block that owns the cell when it is
    /// declared in the same scope.
    Cell(Term, Term, String, Option<String>),
    /// A stack input.
    Input(String),
}

/// The source text of a call's arguments, for a diagnostic.
fn arg_texts(n: &SyntaxNode) -> Vec<String> {
    let mut out: Vec<String> = node(n, ARG_LIST)
        .map(|l| {
            l.children()
                .filter(|c| is_term(c.kind()) || c.kind() == NAMED_ARG)
                .map(|c| c.text().to_string().trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    out.resize(out.len().max(5), String::new());
    out
}

/// A string literal holds an interpolation `${..}` (`$${` is a literal
/// `${`).
fn has_hole(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'$' if b[i + 1] == b'$' && b.get(i + 2) == Some(&b'{') => i += 3,
            b'$' if b[i + 1] == b'{' => return true,
            _ => i += 1,
        }
    }
    false
}

/// A string literal with no holes as its value: escapes, and `$${` as
/// `${`.
fn string_value(text: &str) -> Result<String, String> {
    Ok(unescape(text)?.replace("$${", "${"))
}

/// The types the built-in provider schemas declare (`type_provider`,
/// `type_attr`, ... rows): a program names them without a resource header
/// of its own. Read from the schema text, not resolved.
fn schema_types() -> &'static BTreeSet<String> {
    static TYPES: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    TYPES.get_or_init(|| {
        let mut out = BTreeSet::new();
        for name in ["fake", "gke", "k8s", "aws-mock"] {
            let Some(src) = crate::schema::builtin(name) else {
                continue;
            };
            for line in src.lines() {
                let Some((head, rest)) = line.trim().split_once('(') else {
                    continue;
                };
                if !head.starts_with("type_") {
                    continue;
                }
                let first = rest.split([',', ')']).next().unwrap_or("").trim();
                let t = first.trim_matches('"');
                if !t.is_empty() {
                    out.insert(t.to_string());
                }
            }
        }
        out
    })
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
                    Stmt::Module(m) => {
                        format!("module {} {{ {} }}", m.name, show(&m.body).join("; "))
                    }
                    _ => return None,
                })
            })
            .collect()
    }

    /// `src` lowered as text that is not a file (the core relations
    /// writable), or, `file`, as a program file (`edition 2026` first).
    fn parse_as(src: &str, file: bool) -> anyhow::Result<crate::ast::Program> {
        let src = if file {
            format!("edition 2026\n{src}")
        } else {
            src.to_string()
        };
        let file_id = crate::diag::add_source("t.df", &src);
        let parse = crate::syntax::parser::parse(&src);
        assert!(parse.errors.is_empty(), "{:?}", parse.errors);
        let units = [super::Unit {
            file: file_id,
            root: parse.syntax(),
            imports: None,
            links: Vec::new(),
        }];
        super::lower(&units, &[0], file, super::Mode::Program)
            .map_err(|d| crate::diag::Diagnostics(d).into())
    }

    fn parse(src: &str) -> anyhow::Result<crate::ast::Program> {
        parse_as(src, false)
    }

    fn lower(src: &str) -> Vec<String> {
        match parse(src) {
            Ok(p) => show(&p.statements),
            Err(e) => panic!("{e:#}"),
        }
    }

    fn error(src: &str) -> String {
        match parse(src) {
            Ok(p) => panic!("lowered: {:?}", show(&p.statements)),
            Err(e) => format!("{e:#}"),
        }
    }

    /// The errors of `src` as a program file.
    fn file_error(src: &str) -> String {
        match parse_as(src, true) {
            Ok(p) => panic!("lowered: {:?}", show(&p.statements)),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn value_names_are_read_by_name() {
        let got = lower(
            "input env: enum(\"a\", \"b\") = \"a\"\n\
             p(x) where q(x), env == \"a\"\n\
             r(env) where q(_)\n\
             s(x) where q(x), not env, has env\n\
             let serving = \"blue\" where q(1)\n\
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
    /// follow its clause, the interpolated name comes last.
    #[test]
    fn a_resource_block_lowers_to_its_shared_body() {
        let got = lower(
            "resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
             zone_index(\"a\", 0)\n\
             resource net.subnet \"private-${z}\" {\n\
               vpc_id     = vpc.id\n\
               cidr       = inet.subnet(vpc.cidr, 4, zone_index[z])\n\
               zone       = z\n\
               visibility = \"private\"\n\
             } where data(\"zone\", z)\n",
        );
        assert_eq!(
            got[2],
            "resource \"net.subnet\" Addr { vpc_id = ref(\"net.vpc\", \"vpc\", \"id\"), \
             cidr = inet.subnet(Cidr, 4, ZoneIndex), zone = Z, visibility = \"private\" } :- \
             data(\"zone\", Z), attr(\"net.vpc\", \"vpc\", \"cidr\", Cidr), \
             zone_index(Z, ZoneIndex), Addr = format(\"private-%s\", Z)"
        );
    }

    #[test]
    fn a_dot_is_a_reference_in_a_field_and_a_read_elsewhere() {
        let got = lower(
            "resource k8s.namespace web { name = \"web\" }\n\
             resource k8s.deployment a { namespace = web.name }\n\
             resource k8s.deployment b {\n  namespace = ns\n} where ns = web.name\n\
             p(web.name, x) where x = web.name\n",
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

    /// H-6: a `let` holding a reference is a value, its key, and a dot on
    /// it reads through the reference.
    #[test]
    fn a_let_holding_a_reference_reads_through_it() {
        let got = lower(
            "input env: string = \"dev\"\n\
             resource db.pg other { size = 1 }\n\
             let cfg = settings[env]\n\
             resource net.vpc v { cidr = cfg.net.cidr, name = \"${cfg.name}-vpc\" }\n\
             let pg = db.pg[\"main\"]\n\
             deny \"x\" where cfg.x.y != \"z\", pg.size > 3\n",
        );
        assert_eq!(
            &got[1..],
            [
                "cfg(Env) :- env(Env)",
                "resource \"net.vpc\" \"v\" { cidr = Cidr, name = format(\"%s-vpc\", Name) } :- \
                 cfg(Cfg), setting(Cfg, \"net.cidr\", Cidr), setting(Cfg, \"name\", Name)",
                "pg(\"main\")",
                "deny(\"x\") :- cfg(Cfg), setting(Cfg, \"x.y\", Y), Y != \"z\", pg(Pg), \
                 attr(\"db.pg\", Pg, \"size\", Size), Size > 3",
            ]
        );
        let e = error("let x = settings[\"a\"]\nlet x = 1 where q(1)\np(x.y) where q(1)\n");
        assert!(
            e.contains("`let x` is a settings row in one row and a value in another"),
            "{e}"
        );
    }

    #[test]
    fn a_namespace_no_builtin_schema_knows_is_a_providers() {
        let got = lower(
            "resource acme.gadget g {}\n\
             p(x) where x in acme.widget, acme.widget[x].size > 1\n",
        );
        assert_eq!(
            got.last().unwrap(),
            "p(X) :- want(\"acme.widget\", X), attr(\"acme.widget\", X, \"size\", Size), Size > 1"
        );
    }

    #[test]
    fn a_resource_named_settings_is_read_by_its_name() {
        let got = lower(
            "resource net.vpc settings { name = \"s\" }\n\
             p(n) where n = settings.name\n",
        );
        assert_eq!(
            got.last().unwrap(),
            "p(N) :- attr(\"net.vpc\", \"settings\", \"name\", N)"
        );
    }

    #[test]
    fn the_type_after_in_picks_among_resources_of_one_name() {
        let got = lower(
            "resource db.postgres main {}\n\
             resource net.vpc main {}\n\
             ok(1) where main in db.postgres\n",
        );
        assert_eq!(
            got.last().unwrap(),
            "ok(1) :- want(\"db.postgres\", \"main\")"
        );
    }

    #[test]
    fn membership_indexing_and_negation() {
        let got = lower(
            "resource db.postgres pg { public = false }\n\
             deny \"public\" { resource: p } where p in db.postgres, not p.public == false\n\
             set r.tags = { team: \"x\" } where r in resource\n\
             let xs = [1, 2]\n\
             let ys = [{ name: \"a\", net: 1 }]\n\
             q(x) where x = xs[i], i >= 0, x not in [3]\n\
             ok(1) where pg in db.postgres, has pg.public, not \"other\" in db.postgres\n\
             big(n) where n in world.net.vpc, world.net.vpc[n].size > 3\n\
             pair(n, c) where ys[_] = { name: n, net: c }\n",
        );
        assert_eq!(
            &got[1..],
            [
                "deny(\"public\", {resource: P}) :- want(\"db.postgres\", P), not attr(\"db.postgres\", P, \"public\", false)",
                "arg(Type, R, \"tags\", {team: \"x\"}) :- want(Type, R)",
                "xs([1, 2])",
                "ys([{name: \"a\", net: 1}])",
                "q(X) :- xs(Xs), member(Xs, I, Item), X = Item, I >= 0, not member([3], X)",
                "ok(1) :- want(\"db.postgres\", \"pg\"), attr(\"db.postgres\", \"pg\", \"public\", _), not want(\"db.postgres\", \"other\")",
                "big(N) :- cloud_exists(\"net.vpc\", N), cloud_attr(\"net.vpc\", N, \"size\", Size), Size > 3",
                "pair(N, C) :- ys(Ys), member(Ys, _, {name: N, net: C})",
            ]
        );
    }

    #[test]
    fn modules_instances_and_outputs() {
        let got = lower(
            "module m {\n  input n: int\n  resource net.vpc vpc { size = n }\n  \
             output vpc: net.vpc = vpc\n  output ids: list(string) = [vpc.id]\n}\n\
             instance m a { n = 1 }\n\
             inst(\"a\")\n\
             p(v, s) where inst(i), v = m[i].vpc, s = m.a.vpc.size\n\
             q(x) where x = m.a.ids, \"m.a::vpc\" in net.vpc\n",
        );
        assert_eq!(
            got[0],
            "module m { resource \"net.vpc\" \"vpc\" { size = N } :- n(N); output vpc = None; \
             output vpc = Some(\"\\\"vpc\\\"\"); output ids = None; \
             output ids = Some(\"[ref(\\\"net.vpc\\\", \\\"vpc\\\", \\\"id\\\")]\") }"
        );
        assert_eq!(
            &got[3..],
            [
                "p(V, S) :- inst(I), output(format(\"m.%s\", I), \"vpc\", V), output(\"m.a\", \"vpc\", Vpc), attr(\"net.vpc\", Vpc, \"size\", S)",
                "q(X) :- output(\"m.a\", \"ids\", X), want(\"net.vpc\", \"m.a::vpc\")",
            ]
        );
    }

    #[test]
    fn interpolation_and_lookups() {
        let got = lower(
            "extern file.json(+path, -value)\n\
             p(\"{x} $${x} ${x}%\") where q(x)\n\
             r(v) where v = file.json[\"a.json\"]\n\
             s(y) where q(x), y = \"n-${x}\", \"n-${x}\" in net.route\n",
        );
        assert_eq!(
            &got[..],
            [
                "p(format(\"{x} ${x} %s%\", X)) :- q(X)",
                "r(V) :- file.json(\"a.json\", V)",
                "s(Y) :- q(X), Y = format(\"n-%s\", X), Name = format(\"n-%s\", X), want(\"net.route\", Name)",
            ]
        );
    }

    #[test]
    fn a_negated_body_is_a_helper() {
        let got = lower("p(x) where q(x), not { r(x, y), s(y) }\n");
        assert_eq!(
            &got[..],
            [
                "p(X) :- q(X), not __neg_0(X)",
                "__neg_0(X) :- q(X), r(X, Y), s(Y)",
            ]
        );
    }

    /// `has` and `not ==` on a nested path or a value's field: the walk
    /// to it, and a helper for the negation.
    #[test]
    fn a_nested_path_is_walked_and_its_negation_is_a_helper() {
        let got = lower(
            "resource db.pg d { s = { a: 1 } }\n\
             c(x) where q(x), has x.limits\n\
             n(x) where q(x), not has x.limits\n\
             r(1) where not d.s.a == 2\n",
        );
        assert_eq!(
            &got[1..],
            [
                "c(X) :- q(X), Has = __path(X, \"limits\")",
                "n(X) :- q(X), not __neg_0(X)",
                "__neg_0(X) :- q(X), Has = __path(X, \"limits\")",
                "r(1) :- not __neg_1()",
                "__neg_1() :- attr(\"db.pg\", \"d\", \"s\", S), __path(S, \"a\") = 2",
            ]
        );
    }

    /// H-4: a statement's own `where` is its condition.
    #[test]
    fn a_statement_takes_its_condition() {
        let got = lower(
            "input env: string = \"dev\"\na(1) where env == \"prod\"\n\
             b(x) where e(x), f(x)\n",
        );
        assert_eq!(&got[..], ["a(1) :- env(\"prod\")", "b(X) :- e(X), f(X)"]);
    }

    #[test]
    fn a_variable_may_not_shadow_a_name() {
        let e = error("resource net.vpc main { cidr = \"x\" }\np(net) where q(net)\n");
        assert!(
            e.contains("variable `net` shadows the type namespace `net`"),
            "{e}"
        );
        let e = error("resource net.vpc main { cidr = \"x\" }\np(main) where q(main)\n");
        assert!(
            e.contains("t.df:2:17: variable `main` shadows the resource `main`"),
            "{e}"
        );
    }

    #[test]
    fn an_unbound_name_is_meant_as_a_string() {
        let e = error("input env: string = \"dev\"\np(1) where env == prod\n");
        assert!(e.contains("t.df:2:19: unknown name `prod`"), "{e}");
        assert!(e.contains("\"prod\""), "{e}");
        let e = error("resource net.vpc main { cidr = dev }\n");
        assert!(e.contains("unknown name `dev`"), "{e}");
    }

    #[test]
    fn a_name_used_twice_is_named_by_its_address() {
        let e = error(
            "resource k8s.namespace web { n = 1 }\nresource k8s.service web { n = 1 }\n\
             resource x.y z { a = web.n }\n",
        );
        assert!(
            e.contains(
                "`web` names 2 resources: write one of k8s.namespace[\"web\"], k8s.service[\"web\"]"
            ),
            "{e}"
        );
        // Named by its address, the second resource `web` is not an error.
        lower(
            "resource k8s.namespace web { n = 1 }\nresource k8s.service web { n = 1 }\n\
             resource x.y z { a = k8s.service[\"web\"].n }\n",
        );
    }

    #[test]
    fn a_block_name_is_a_variable_when_its_clause_binds_it() {
        let got = lower(
            "t(\"a\")\nresource net.vpc t {\n size = 1 } where t(t)\nresource net.vpc shared { size = 2 }\n",
        );
        assert_eq!(
            &got[1..],
            [
                "resource \"net.vpc\" T { size = 1 } :- t(T)",
                "resource \"net.vpc\" \"shared\" { size = 2 } :- ",
            ]
        );
        assert!(matches!(
            parse("resource net.vpc n { size = 1 }\n")
                .unwrap()
                .statements[0],
            crate::ast::Stmt::Resource(crate::ast::Resource {
                name: Term::Val(_),
                body: None,
                ..
            })
        ));
    }

    /// The compiler errors that name the one spelling (H-5, H-7, H-8,
    /// H-10, H-15).
    #[test]
    fn a_second_spelling_is_an_error_naming_the_first() {
        for (src, want) in [
            // H-5: an unconditional `set` on a block of the same scope.
            (
                "resource net.vpc main { cidr = \"x\" }\nset main.tags = {}\n",
                "an entry of `resource net.vpc main`",
            ),
            (
                "input env: string = \"dev\"\nset env = \"prod\"\n",
                "sets the program's own input",
            ),
            // H-7: an output has a value.
            ("output vpc: string\n", "output vpc has no value"),
            // H-8: no rule reads deny/2.
            (
                "deny \"m\" where q(1)\np(m) where deny(m)\n",
                "a rule reads `deny`",
            ),
            // H-10: a resource in scope by its key, or by a dot after its type.
            (
                "resource net.vpc main { cidr = \"x\" }\np(c) where c = net.vpc[\"main\"].cidr\n",
                "write `main` (H-10)",
            ),
            (
                "resource net.vpc main { cidr = \"x\" }\np(c) where c = net.vpc.main.cidr\n",
                "names a resource by a dot after its type",
            ),
            (
                "p(c) where c = net.vcp.main.cidr\n",
                "unknown type `net.vcp.main.cidr`",
            ),
            // H-15: the core where a surface form says it.
            ("p(x) where want(net.vpc, x)\n", "write `x in net.vpc` (H-15)"),
            (
                "p(v) where attr(net.vpc, \"main\", \"cidr\", v)\n",
                "write `v = net.vpc[\"main\"].cidr` (H-15)",
            ),
            (
                "arg(net.vpc, \"main\", \"cidr\", \"x\")\n",
                "write `set net.vpc[\"main\"].cidr = \"x\"` (H-15)",
            ),
            ("output(\"k\", 1)\n", "write `output k = 1` (H-15)"),
            ("deny(\"m\") where q(1)\n", "write `deny \"m\" where ..` (H-15)"),
            ("p(x) where member([1], x)\n", "write `x in [1]` (H-9)"),
        ] {
            let e = file_error(src);
            assert!(e.contains(want), "{src}: {e}");
        }
        // The core stays writable where nothing else reaches: a variable
        // type or path, a read of the contributions before they merge.
        parse_as(
            "resource net.vpc main { cidr = \"x\" }\n\
             arg(t, n, p, v) where override(t, n, p, v), want(t, n)\n\
             override(net.vpc, \"main\", \"tags\", {})\n\
             p(v) where arg(net.vpc, \"main\", \"cidr\", v)\n",
            true,
        )
        .unwrap();
    }
}
