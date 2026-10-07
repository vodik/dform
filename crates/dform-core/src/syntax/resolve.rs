//! From the lossless tree to `ast`: name resolution (docs/grammar.md
//! "Names") and lowering to today's AST, so `transform` and everything after
//! it see exactly what the core has always seen.
//!
//! Resolution is program-wide (R-65): `lower` takes every file of the
//! program, the entry files and the modules their paths reach, collects
//! the declarations, then lowers the entry files as the program's top level
//! and each other file as the module or component it is. A module's file
//! is a scope of its own, inside the program's: a name it does not declare
//! reads outward. A chain (`a.b[e].c`) is resolved in this order: a
//! variable of the rule with a static type, a value name (an input or a
//! `let`, which may hold a reference), `settings`, `world`, a resource in
//! scope, an instance (`blue.vpc`), a stack's deployment
//! (`platform[env=e].x`), a component's instances (`network[t].vpc`), a
//! used module's value (`config.region`), a type's resource by key
//! (`T[e]`), a relation or extern lookup, a type; a bare name that is none
//! of these is a variable, and may not take the name of a resource, a
//! module, an instance or a type namespace in scope.
//! A `.` is static (H section 5.1): a name after it that nothing declares is
//! an error, never a string.
//!
//! Where a read lands: in a rule body, just before the literal that holds
//! it; in a head, a field or an instance input, appended to the body (the
//! block's one shared body: a read in any field gates the whole block).

use super::SyntaxKind::{self, *};
use super::parser as parse;
use super::{SyntaxNode, SyntaxToken, tokens};
use crate::ast::{
    Atom, AttrDecl, BindArg, Config, Decl, Extern, ExternFn, FieldAssign, FieldOp, InputDecl,
    Instance, Lit, Module, OutputDecl, Pending, PendingKind, Program, Rank, Resource, RuleStmt,
    Span, Stmt, Term, TypeExpr, str_term, var,
};
use crate::diag::Diagnostic;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

mod aggregate;
mod alias;
mod binding;
mod each;
pub use each::each_var;
mod heads;
mod membership;
mod pattern;
mod provider;
mod singleton;
pub use provider::ENV_VAR;

/// One parsed file of a program.
pub struct Unit {
    /// `diag` source id.
    pub file: u32,
    pub root: SyntaxNode,
    /// The module the file is, by its path (`config`, `modules.net`); `None`
    /// for an entry file, the program's own top level.
    pub path: Option<String>,
}

/// A stack a `use` names (R-65): deployed by the tool, never lowered into
/// the program; what is read of it is a deployment's outputs, the keyed
/// read `instance_of(PATH, "", "NAME[k=v]"), output("NAME[k=v]", out, V)`.
#[derive(Debug, Clone)]
pub struct Deployed {
    /// Its path, `stacks.platform`.
    pub path: String,
    /// The deployment's name as a reader writes it: `platform`, or
    /// `infra.platform` of the package `infra`.
    pub name: String,
    /// Its keys, in order.
    pub keys: Vec<String>,
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

/// The stack a program is (R-29): its name, its file's stem, and its
/// settings as dform.toml gives them, `[stacks.NAME]` over `[defaults]`
/// (`project::Manifest::stack_settings`).
#[derive(Debug, Clone)]
pub struct StackSource {
    pub name: String,
    pub settings: Vec<Setting>,
    /// Where the manifest says them.
    pub span: Span,
}

/// One stack setting: its key, its value, and the value's place in
/// dform.toml.
#[derive(Debug, Clone)]
pub struct Setting {
    pub key: String,
    pub value: SettingValue,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum SettingValue {
    /// A string or a bool, as it is.
    Plain(Term),
    /// A term's text (`backend`, `approvals`) at byte `offset` of the
    /// manifest's source; `{stack}` is the stack's name.
    Term { text: String, offset: u32 },
}

/// Lower `entries` (and, through their imports, the rest of `units`).
/// `file`: they are program files, where a surface form says what the
/// core would (H-15); else text the compiler or a provider wrote.
pub fn lower(
    units: &[Unit],
    entries: &[usize],
    file: bool,
    mode: Mode,
) -> Result<Program, Vec<Diagnostic>> {
    lower_stack(units, entries, file, mode, None, &[])
}

/// [`lower`], the program being the stack `stack`: its settings lower to
/// the program's `stack` and its `config`'s rules. `deployed` are the stacks
/// its `use`s name.
pub fn lower_stack(
    units: &[Unit],
    entries: &[usize],
    file: bool,
    mode: Mode,
    stack: Option<&StackSource>,
    deployed: &[Deployed],
) -> Result<Program, Vec<Diagnostic>> {
    if mode == Mode::Program {
        units.iter().for_each(|u| field_orders(&u.root));
    }
    let mut l = Lowerer::new(units, entries, deployed, mode == Mode::Text);
    l.text = mode == Mode::Text;
    l.any_type = mode == Mode::Pattern;
    l.core = !file;
    if mode == Mode::Program {
        l.check_heads();
    }
    let mut statements = l.declare_builtin_externs();
    for &e in entries {
        statements.extend(l.unit(e));
    }
    for (i, u) in units.iter().enumerate() {
        if u.path.is_some() && !entries.contains(&i) {
            statements.extend(l.unit(i));
        }
    }
    let settings = stack.and_then(|st| l.stack_settings(st, entries));
    if l.diags.is_empty() {
        Ok(Program {
            statements,
            stack: settings,
        })
    } else {
        Err(l.diags)
    }
}

/// Remember the order `root` writes objects' fields in
/// (`fmt::value::remember`): each object literal's keys, and a block's
/// entries' keys under each path they share (`metadata.name`,
/// `metadata.labels.app` write `name` before `labels`).
fn field_orders(root: &SyntaxNode) {
    for n in root.descendants() {
        match n.kind() {
            OBJECT => {
                let keys = n
                    .children()
                    .filter(|c| c.kind() == OBJECT_FIELD)
                    .filter_map(|f| tokens(&f).next())
                    .map(|k| match k.kind() {
                        STRING => string_value(k.text()).unwrap_or_else(|_| k.text().into()),
                        _ => k.text().to_string(),
                    })
                    .collect();
                crate::fmt::value::remember(keys);
            }
            BLOCK => {
                let mut under: Vec<(Vec<String>, Vec<String>)> = Vec::new();
                for a in n.children().filter(|c| c.kind() == ASSIGN) {
                    let Some(path) = a
                        .children()
                        .find(|c| c.kind() == CHAIN || c.kind() == BLOCK_PATH)
                    else {
                        continue;
                    };
                    let text = path.text().to_string();
                    let segs: Vec<String> = crate::ir::path_keys(text.trim());
                    for i in 0..segs.len() {
                        let (at, key) = (segs[..i].to_vec(), segs[i].clone());
                        match under.iter_mut().find(|(p, _)| *p == at) {
                            Some((_, ks)) if ks.contains(&key) => {}
                            Some((_, ks)) => ks.push(key),
                            None => under.push((at, vec![key])),
                        }
                    }
                }
                for (_, ks) in under {
                    crate::fmt::value::remember(ks);
                }
            }
            _ => {}
        }
    }
}

/// A term the compiler reads as data, on its own: a backend as dform.toml
/// writes it (`local("DIR")`, `s3(..)`). The error is the first
/// diagnostic's message.
pub fn data_term(src: &str) -> Result<Term, String> {
    let parse = parse::parse_term(src);
    if let Some(e) = parse.errors.first() {
        return Err(e.message.clone());
    }
    let root = parse.syntax();
    let t = terms(&root)
        .next()
        .ok_or_else(|| "not a term".to_string())?;
    let mut l = Lowerer::new(&[], &[], &[], false);
    let mut rc = l.rc(&t, PROGRAM, &Rc::default());
    l.calls(Calls::Data, |l| l.constant(&mut rc, &t))
        .map_err(|_| {
            l.diags
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_default()
        })
}

// --- declarations ---------------------------------------------------------

/// The static type of a value name whose value is a reference (H-6): a dot
/// on it reads through the reference.
#[derive(Clone, Debug, PartialEq, Eq)]
enum VType {
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
    /// Each input's declaration, for its type.
    input_nodes: BTreeMap<String, SyntaxNode>,
    /// A `let`'s rows: the terms it is defined by.
    lets: BTreeMap<String, Vec<SyntaxNode>>,
    /// Resources with a static name: name -> the types declaring it.
    resources: BTreeMap<String, Vec<String>>,
    /// Instances declared here: name -> its component's path as written,
    /// then, once bound, as resolved (`instances`).
    instances_written: BTreeMap<String, String>,
    instances: BTreeMap<String, String>,
    /// Components declared here (`component network`) -> the path.
    components: BTreeMap<String, String>,
    /// Components instanced here, by their last segment (`instance
    /// postgres db` makes `postgres[t]` readable) -> the path. A path an
    /// `instance` or a `use` writes never reads one, so binding does not
    /// depend on which statement comes first.
    copied: BTreeMap<String, String>,
    /// Instances whose component names nothing: the `instance` is the
    /// error, and a read of its name says nothing more.
    unbound: BTreeSet<String>,
    /// Modules used here: the name it binds -> the module's path.
    uses: BTreeMap<String, String>,
    /// The names a `use` or an `instance` binds here, each declaration in
    /// source order: `(path as written, whether it is a use, the
    /// statement)`. Several of one name are guarded declarations (R-104).
    bound: BTreeMap<String, Vec<(String, bool, SyntaxNode)>>,
    /// A name declared more than once (R-104) -> each declaration's
    /// module or component path, as resolved, and its statement.
    alternatives: BTreeMap<String, Vec<(String, SyntaxNode)>>,
    /// Each value output's declared type, as written (R-104): what two
    /// declarations of one name must agree on.
    output_types: BTreeMap<String, String>,
    /// Component signatures declared here, `type T = component { .. }`
    /// (R-104): name -> the file and the `type` statement.
    signatures: BTreeMap<String, (u32, SyntaxNode)>,
    /// Stacks used here: the name it binds -> its index in `deployed`.
    stacks: BTreeMap<String, usize>,
    /// A component's `output k: T`: `Some(T)` when T is a resource type.
    outputs: BTreeMap<String, Option<String>>,
    /// The arities each relation this scope's heads and `decl`s give it.
    arities: BTreeMap<String, BTreeSet<usize>>,
    /// Each relation's `decl`: its columns (R-55).
    decl_nodes: BTreeMap<String, SyntaxNode>,
    /// `input p` with no `from`: the relations a module's user gives.
    relation_inputs: BTreeSet<String>,
    /// `output p`: the relations a component exports (R-55).
    relation_outputs: BTreeSet<String>,
    /// Relations this scope's own facts and rules define.
    heads: BTreeSet<String>,
}

#[derive(Default)]
struct Decls {
    scopes: Vec<Scope>,
    /// The scope of each file's top level, by source id.
    files: BTreeMap<u32, usize>,
    /// The scope of a component block, by (file, offset).
    blocks: BTreeMap<(u32, u32), usize>,
    /// The entry files' top-level scopes: what they declare is the
    /// program's.
    entries: BTreeSet<usize>,
    /// Each module file's path, by source id (R-65).
    paths: BTreeMap<u32, String>,
    /// Modules and components by path: a file, or a component item.
    modules: BTreeMap<String, ModDecl>,
    /// The stacks the program's `use`s name.
    deployed: Vec<Deployed>,
    /// Resource types: every resource header's, every `type` block's, every
    /// `type_*` fact's, and the built-in provider schemas'.
    types: BTreeSet<String>,
    /// First segments of the types: no variable may take one.
    namespaces: BTreeSet<String>,
    /// The namespaces whose every type the compiler knows: the built-in
    /// schemas' but [`OPEN_NAMESPACES`]. Another namespace's type may be a
    /// provider schema's the compiler does not read (`--provider`, a
    /// plugin's).
    closed: BTreeSet<String>,
    /// Relations: rule and fact heads, `decl`s, input relations.
    relations: BTreeSet<String>,
    /// `extern` relations: their columns, `(input, name)`.
    externs: BTreeMap<String, Vec<(bool, String)>>,
    /// Relations the program's own facts and rules define.
    heads: BTreeSet<String>,
    /// Each `resource T n` statement as collected, before what `T` names
    /// is known: the scope it declares into, its file and the statement.
    pending: Vec<(usize, u32, SyntaxNode)>,
    /// The `resource C n` statements whose type is a component (R-113):
    /// copies, by file and offset.
    copies: BTreeSet<(u32, u32)>,
    /// Each provider a `use P as A` renames, and its names (R-115):
    /// `ovh` -> `ca`, `eu`. `x in ovh.instance` ranges over `ca.instance`
    /// and `eu.instance` too.
    renamed: BTreeMap<String, BTreeSet<String>>,
}

const PROGRAM: usize = 0;

/// The built-in schemas' namespaces they do not close: a mock of part of a
/// real provider's types (`aws`, `google`; R-36), and `k8s`, whose types
/// are a cluster's own, CRDs included.
const OPEN_NAMESPACES: &[&str] = &["aws", "google", "k8s"];

/// A module or a component (R-65).
#[derive(Clone)]
struct ModDecl {
    /// Its body's scope.
    scope: usize,
    /// Whether it is a component, an item; else a module, a file.
    component: bool,
}

fn node(n: &SyntaxNode, k: SyntaxKind) -> Option<SyntaxNode> {
    n.children().find(|c| c.kind() == k)
}

fn is_term(k: SyntaxKind) -> bool {
    matches!(
        k,
        LITERAL
            | CHAIN
            | CALL_CHAIN
            | CALL
            | LIST
            | OBJECT
            | COMPREHENSION
            | PAREN
            | TUPLE
            | BIN_EXPR
            | UNARY_EXPR
            | RANGE
    )
}

fn terms(n: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    n.children().filter(|c| is_term(c.kind()))
}

/// The resource type a typed `let`'s row declares, from the row's term
/// `t`: `let v: net.vpc = ..`, `let v: ref(net.vpc) = ..` (R-74).
fn declared_ref(t: &SyntaxNode) -> Option<String> {
    let ty = node(&t.parent().filter(|l| l.kind() == LET)?, TYPE_EXPR)?;
    let name = dotted_text(&ty, 0);
    let name = match node(&ty, TYPE_EXPR) {
        Some(inner) if name == "ref" => dotted_text(&inner, 0),
        _ => name,
    };
    name.contains('.').then_some(name)
}

/// The name a term is when it is one word and nothing else: `env`.
fn bare_name(t: &SyntaxNode) -> Option<String> {
    let mut ws = t
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|k| !k.kind().is_trivia());
    match (ws.next(), ws.next()) {
        (Some(w), None) if w.kind() == IDENT => Some(w.text().to_string()),
        _ => None,
    }
}

/// The first word token of a node after `skip` others.
fn word_text(n: &SyntaxNode, skip: usize) -> String {
    tokens(n)
        .filter(|t| t.kind().is_word())
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
        if t.kind().is_word() {
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

/// The path of a `use` or an `instance` as written (`modules.net.vpc`),
/// its segments' tokens, and the word after it (an instance's name, or a
/// use's `as`).
fn path_parts(n: &SyntaxNode) -> (Vec<SyntaxToken>, Vec<SyntaxToken>) {
    let mut path = Vec::new();
    let mut rest = Vec::new();
    let mut dot = true;
    for t in tokens(n).skip(1) {
        match t.kind() {
            DOT if !dot && rest.is_empty() => dot = true,
            k if k.is_word() && dot && rest.is_empty() => {
                path.push(t);
                dot = false;
            }
            k if k.is_word() => rest.push(t),
            _ => break,
        }
    }
    (path, rest)
}

/// A `use` statement (R-65): its path as written, and the name it binds,
/// the word after `as`, else the path's last segment.
pub fn use_parts(n: &SyntaxNode) -> (String, String) {
    let (path, rest) = path_parts(n);
    let text = path.iter().map(|t| t.text()).collect::<Vec<_>>().join(".");
    let name = match rest.as_slice() {
        [r#as, alias, ..] if r#as.text() == "as" => alias.text().to_string(),
        _ => path
            .last()
            .map(|t| t.text().to_string())
            .unwrap_or_default(),
    };
    (text, name)
}

/// A `use` of a provider (R-112 amendment 2): `use ovh { endpoint = .. }`
/// imports the provider's namespace and configures it, as `use ovh
/// { .. }` did. A `use` is a provider's when its path is one segment that
/// names no module of the program (`units`), no stack it deploys, no
/// component its file declares and not `std`; the provider's name.
pub fn provider_use(n: &SyntaxNode, units: &[Unit], deployed: &[Deployed]) -> Option<String> {
    if n.kind() != USE {
        return None;
    }
    let (written, _) = use_parts(n);
    let local_component = || {
        n.parent().is_some_and(|p| {
            p.children()
                .any(|c| c.kind() == COMPONENT && component_name(&c) == written)
        })
    };
    (!written.is_empty()
        && !written.contains('.')
        && written != "std"
        && !units
            .iter()
            .any(|u| u.path.as_deref() == Some(written.as_str()))
        && !deployed.iter().any(|d| d.path == written)
        && !local_component())
    .then_some(written)
}

/// Each provider a `use P as A` of the program renames, and its names
/// (R-115).
fn renamed(units: &[Unit], deployed: &[Deployed]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for u in units {
        for n in u.root.descendants().filter(|n| n.kind() == USE) {
            let (of, name) = use_parts(&n);
            if of != name && provider_use(&n, units, deployed).is_some() {
                out.entry(of).or_default().insert(name);
            }
        }
    }
    out
}

/// Whether `name` is the namespace of a built-in schema's types: `aws`
/// of the aws mock's `aws.vpc` (R-112), which `dev --provider aws-mock`
/// serves.
pub fn builtin_namespace(name: &str) -> bool {
    schema_types()
        .iter()
        .any(|t| t.split_once('.').is_some_and(|(ns, _)| ns == name))
}

/// Whether `typ` is a type of a built-in schema (`net.vpc` of the fake
/// cloud's).
pub fn builtin_type(typ: &str) -> bool {
    schema_types().contains(typ)
}

/// Whether a `use`'s block names a `source`: a provider's setting, the
/// executable that serves it (`use mine { source = "mine" }`).
pub fn names_source(n: &SyntaxNode) -> bool {
    node(n, BLOCK).is_some_and(|b| {
        b.children()
            .filter(|a| a.kind() == ASSIGN)
            .any(|a| node(&a, BLOCK_PATH).is_some_and(|p| p.text() == "source"))
    })
}

/// The provider a `use` may import, read from its statement alone: a
/// path of one segment that is not `std`. Whether it is one takes the
/// program's modules ([`provider_use`]); a tool with only the tree (`fmt`,
/// the language server's schema lookup) takes a module's `use` along,
/// which no provider serves.
pub fn maybe_provider_use(n: &SyntaxNode) -> Option<String> {
    let (written, _) = use_parts(n);
    (n.kind() == USE && !written.is_empty() && !written.contains('.') && written != "std")
        .then_some(written)
}

/// A resource's type path as written and its name, a word or a string:
/// for `resource PATH NAME { .. }` whose type is a component (R-113), the
/// component's path and the copy's name.
pub fn copy_parts(n: &SyntaxNode) -> (String, String) {
    let name = header_name(n)
        .map(|t| match t.kind() {
            STRING => string_value(t.text()).unwrap_or_else(|_| t.text().to_string()),
            _ => t.text().to_string(),
        })
        .unwrap_or_default();
    (dotted_text(n, 1), name)
}

/// The name token of a resource header: the word or string after its
/// dotted type.
pub fn header_name(n: &SyntaxNode) -> Option<SyntaxToken> {
    let mut seen_word = false;
    let mut after_dot = false;
    for t in tokens(n).skip(1) {
        match t.kind() {
            DOT => after_dot = true,
            STRING => return Some(t),
            k if k.is_word() => {
                if seen_word && !after_dot {
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

/// A `component NAME { .. }`'s name.
pub fn component_name(n: &SyntaxNode) -> String {
    word_text(n, 1)
}

/// The token that names what a `use`, an `instance` or a copy's
/// `resource` binds: the alias or the copy's name when written, else the
/// path's last segment.
pub fn bound_token(n: &SyntaxNode) -> Option<SyntaxToken> {
    if n.kind() == RESOURCE {
        return header_name(n);
    }
    let (path, rest) = path_parts(n);
    match (n.kind(), rest.as_slice()) {
        (USE, [r#as, alias, ..]) if r#as.text() == "as" => Some(alias.clone()),
        _ => path.last().cloned(),
    }
}

/// Is an `INPUT` node a `key` (R-29)?
pub fn is_key(n: &SyntaxNode) -> bool {
    tokens(n).next().is_some_and(|t| t.kind() == KEY_KW)
}

/// A file's keys, read from its tree without resolving it: the names of
/// its top-level `key` statements, in order (discovery, `project`).
pub fn key_names(root: &SyntaxNode) -> Vec<String> {
    root.children()
        .filter(|n| n.kind() == INPUT && is_key(n))
        .map(|n| word_text(&n, 1))
        .collect()
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
    /// `[k=v, ..]`: a stack's deployment by its keys (R-65).
    Keyed(Vec<(String, SyntaxNode)>, rowan::TextRange),
}

/// A chain: its head word and the parts after it.
#[derive(Clone)]
struct Chain {
    head: String,
    head_kind: SyntaxKind,
    /// The call a `CALL_CHAIN` reads the result of (R-71), `f(x).p`; its
    /// `head` is then empty.
    call: Option<SyntaxNode>,
    range: rowan::TextRange,
    ops: Vec<Op>,
}

impl Chain {
    /// A chain that starts with a name.
    fn of(n: &SyntaxNode) -> Option<Chain> {
        Chain::read(n).filter(|c| c.call.is_none())
    }

    /// A chain, or a read of a call's result (R-71): what `has`, a truth
    /// test and a term resolve.
    fn read(n: &SyntaxNode) -> Option<Chain> {
        if !matches!(n.kind(), CHAIN | CALL_CHAIN) {
            return None;
        }
        let mut it = n
            .children_with_tokens()
            .filter(|e| e.as_token().is_none_or(|t| !t.kind().is_trivia()));
        let (head, head_kind, call) = match it.next()? {
            rowan::NodeOrToken::Node(c) if c.kind() == CALL => (String::new(), CALL, Some(c)),
            rowan::NodeOrToken::Node(_) => return None,
            rowan::NodeOrToken::Token(t) => (t.text().to_string(), t.kind(), None),
        };
        let mut ops = Vec::new();
        let mut pending: Option<SyntaxKind> = None;
        for e in it {
            match e {
                rowan::NodeOrToken::Node(ix) if ix.kind() == INDEX => {
                    let mut named: Vec<(String, SyntaxNode)> = ix
                        .children()
                        .filter(|c| c.kind() == NAMED_ARG)
                        .filter_map(|a| Some((word_text(&a, 0), terms(&a).next()?)))
                        .collect();
                    // Beside a `k = v`, a bare name is the pun `k = k`
                    // (R-33): `platform[env, region = "GRA11"]`.
                    if !named.is_empty() {
                        let puns: Vec<(String, SyntaxNode)> = terms(&ix)
                            .filter_map(|t| Some((bare_name(&t)?, t)))
                            .collect();
                        named.extend(puns);
                    }
                    ops.push(match named.is_empty() {
                        true => Op::Index(terms(&ix).collect(), ix.text_range()),
                        false => Op::Keyed(named, ix.text_range()),
                    });
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
            head,
            head_kind,
            call,
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
    /// `[k]` with `k` a string or a read (`["api"]`, `[c.name]`): the
    /// element of a keyed list whose key is `k` (R-35); a bare variable or
    /// an integer is the position.
    K(Term),
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
    /// An instance output; `typ` when its declared type is a resource
    /// type (it holds that resource's address).
    Output {
        inst: Term,
        key: String,
        path: Vec<Seg>,
        typ: Option<Term>,
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
/// else it is a read. `Value` is a whole value a resource is given to (an
/// entry, a `set`, an output, an instance input, a `let`): there a resource
/// itself, by its name, `T[e]` or a typed variable, is the reference
/// `ref(T, A, "")` (R-43), where a head argument keeps its address.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    Whole,
    Value,
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
    /// Source variable -> the provider namespace `x in NS` ranges it over
    /// (R-49); its type in `types` is a variable.
    namespaces: BTreeMap<String, String>,
    /// Source variable -> the component `x in network` ranges it over
    /// (R-67): `x` is a copy's name, `x.k` its output.
    instances: BTreeMap<String, String>,
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
    /// Source variable -> where it is written (`singleton`).
    uses: singleton::Uses,
    /// Source variables a relation's reference column binds with no type
    /// (`deformation(k, r, _)` and no `r in T`): a reference whose
    /// attributes nothing can read (R-43).
    untyped_refs: BTreeSet<String>,
    /// Source variable -> the resource list its `in` ranges over, `(T, A,
    /// path)`: `set c.p` writes that element (R-69).
    elems: BTreeMap<String, (Term, Term, String)>,
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
    /// The aggregate bindings of the statement being lowered.
    aggs: Vec<aggregate::Agg>,
    /// `__agg_N` helpers generated so far.
    agg_rules: usize,
    /// How deep in `not { }` bodies and comprehensions the lowering is.
    nested: usize,
    /// An object pattern's field reads, appended after the literal that
    /// binds it (`pattern`).
    after: Vec<Lit>,
    /// The columns of a relation with no `decl`, read from its first
    /// source (R-34).
    read_columns: BTreeMap<String, Vec<BindArg>>,
    /// What picks among the resources a bare name shares, in the value
    /// being lowered (R-74).
    want: Want,
}

/// What a value's position says of the resource a bare name two types
/// share names (R-74).
#[derive(Clone, Default, PartialEq, Eq)]
enum Want {
    /// Nothing: the name is an error listing them.
    #[default]
    Nothing,
    /// A typed `let`'s resource type.
    Type(String),
    /// A resource's attribute: its schema's `ref(T)`, once the schema is
    /// known (`types::read`).
    Schema,
}

/// What a call is where it is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Calls {
    /// A function the evaluator applies.
    Function,
    /// A constructor the compiler reads as data: a provider's or stack's
    /// setting (`local("DIR")`, `jwks(...)`), an input relation's source,
    /// a `type_refine` constraint. Each reader checks its own names.
    Data,
}

impl<'u> Lowerer<'u> {
    fn new(units: &'u [Unit], entries: &[usize], deployed: &[Deployed], lenient: bool) -> Self {
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
            aggs: Vec::new(),
            agg_rules: 0,
            nested: 0,
            after: Vec::new(),
            read_columns: BTreeMap::new(),
            want: Want::Nothing,
        };
        l.decls.deployed = deployed.to_vec();
        for (i, u) in units.iter().enumerate() {
            let scope = l.new_scope(PROGRAM);
            l.decls.files.insert(u.file, scope);
            match &u.path {
                // A module's file is its own scope (R-65).
                Some(path) if !entries.contains(&i) => {
                    l.decls.paths.insert(u.file, path.clone());
                    l.decls.modules.insert(
                        path.clone(),
                        ModDecl {
                            scope,
                            component: false,
                        },
                    );
                    l.collect(u.file, &u.root, scope, scope);
                }
                _ => {
                    l.decls.entries.insert(scope);
                    l.collect(u.file, &u.root, PROGRAM, scope);
                }
            }
        }
        l.bind_instances();
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
            .filter(|n| !OPEN_NAMESPACES.contains(&n.as_str()))
            .collect();
        l.collect_aliases();
        l.decls.renamed = renamed(units, deployed);
        l
    }

    /// The types `x in t` ranges over: `t`, and, `t` a provider's type
    /// (`ovh.instance`), the same type under each name a `use .. as`
    /// gives the provider (`ca.instance`, R-115).
    fn covering(&self, t: &str) -> Vec<String> {
        let mut out = vec![t.to_string()];
        if let Some((ns, rest)) = t.split_once('.') {
            for a in self.decls.renamed.get(ns).into_iter().flatten() {
                out.push(format!("{a}.{rest}"));
            }
        }
        out
    }

    /// Each scope's instances, their components' paths resolved, and
    /// each instanced component readable by its last segment there
    /// (`postgres[t]`).
    fn bind_instances(&mut self) {
        for s in 0..self.decls.scopes.len() {
            let used = self.decls.scopes[s].uses.clone();
            for (name, written) in used {
                let path = self.module_path_of(s, &written);
                self.decls.scopes[s].uses.insert(name, path);
            }
        }
        self.classify_resources();
        // A name declared more than once: each declaration's path (R-104).
        for s in 0..self.decls.scopes.len() {
            let bound = self.decls.scopes[s].bound.clone();
            for (name, decls) in bound.into_iter().filter(|(_, d)| d.len() > 1) {
                let mut alts = Vec::new();
                for (written, used, n) in decls {
                    let path = match used {
                        true => self.module_path_of(s, &written),
                        false => match self.component_path(s, &written) {
                            Ok(p) => p,
                            Err(_) => continue,
                        },
                    };
                    if !used {
                        let last = written.rsplit('.').next().unwrap_or(&written).to_string();
                        if self.use_in(s, &last).is_none() && self.stack_in(s, &last).is_none() {
                            self.decls.scopes[s]
                                .copied
                                .entry(last)
                                .or_insert(path.clone());
                        }
                    }
                    alts.push((path, n));
                }
                self.decls.scopes[s].alternatives.insert(name, alts);
            }
        }
        for s in 0..self.decls.scopes.len() {
            let written = self.decls.scopes[s].instances_written.clone();
            for (name, path) in written {
                let Ok(full) = self.component_path(s, &path) else {
                    self.decls.scopes[s].unbound.insert(name);
                    continue;
                };
                // A used module of the name is read by it, and its
                // component's copies by the path (`k3s.k3s[t]`).
                let last = path.rsplit('.').next().unwrap_or(&path).to_string();
                let used = self.use_in(s, &last).is_some() || self.stack_in(s, &last).is_some();
                let scope = &mut self.decls.scopes[s];
                if !used {
                    scope.copied.entry(last).or_insert(full.clone());
                }
                scope.instances.insert(name, full);
            }
        }
    }

    /// Each `resource T n` collected, by what `T` names (R-113): a
    /// component's path (or a signature's, a module's, a stack's, which
    /// `instance` reports) makes a copy, bound as the scope's other names
    /// are; any other is a resource of the type `T`.
    fn classify_resources(&mut self) {
        for (decl, file, n) in std::mem::take(&mut self.decls.pending) {
            let typ = dotted_text(&n, 1);
            let full = self.module_path_of(decl, &typ);
            // A path from the root that is also a type the program or a
            // built-in schema declares is the type: `net.vpc` in net.df
            // is the fake cloud's, not net.df's `component vpc`.
            let head = typ.split('.').next().unwrap_or(&typ);
            let bound = self.chain_of(decl).into_iter().any(|s| {
                let sc = &self.decls.scopes[s];
                sc.components.contains_key(head) || sc.uses.contains_key(head)
            });
            let typed = schema_types().contains(&typ) || self.decls.types.contains(&typ);
            let copy = (bound || !typed)
                && (self.decls.modules.contains_key(&full)
                    || self.decls.deployed.iter().any(|d| d.path == full)
                    || self.signature(decl, &typ).is_some());
            if copy {
                let (path, name) = copy_parts(&n);
                self.decls
                    .copies
                    .insert((file, n.text_range().start().into()));
                let sc = &mut self.decls.scopes[decl];
                sc.bound
                    .entry(name.clone())
                    .or_default()
                    .push((path.clone(), false, n.clone()));
                sc.instances_written.entry(name).or_insert(path);
                continue;
            }
            self.decls.types.insert(typ.clone());
            if let Some(name) = self.static_header(&n) {
                self.decls.scopes[decl]
                    .resources
                    .entry(name)
                    .or_default()
                    .push(typ);
            }
        }
    }

    /// Whether `n`, a statement of the file being lowered, makes a copy of
    /// a component: `resource C n { .. }` (R-113).
    fn is_copy(&self, n: &SyntaxNode) -> bool {
        self.is_copy_in(self.file, n)
    }

    /// [`Self::is_copy`] for a statement of `file`.
    fn is_copy_in(&self, file: u32, n: &SyntaxNode) -> bool {
        n.kind() == RESOURCE
            && self
                .decls
                .copies
                .contains(&(file, n.text_range().start().into()))
    }

    /// The module path a path written in `scope` names: its first segment
    /// a component or a used module in scope, else the path from the root.
    fn module_path_of(&self, scope: usize, written: &str) -> String {
        let (head, rest) = match written.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (written, None),
        };
        let base = self.chain_of(scope).into_iter().find_map(|s| {
            let sc = &self.decls.scopes[s];
            sc.components
                .get(head)
                .or_else(|| sc.uses.get(head))
                .cloned()
        });
        match (base, rest) {
            (Some(b), Some(r)) => format!("{b}.{r}"),
            (Some(b), None) => b,
            (None, _) => written.to_string(),
        }
    }

    /// The component a path written in `scope` names, or the error that
    /// says what it names instead.
    fn component_path(&self, scope: usize, written: &str) -> Result<String, Box<Diagnostic>> {
        let full = self.module_path_of(scope, written);
        match self.decls.modules.get(&full) {
            Some(m) if m.component => Ok(full),
            Some(_) => {
                let mut d =
                    Diagnostic::error(Span::default(), format!("{written} is a module; `use` it"))
                        .with_note(
                            "a module, a file, is imported once by `use`; a component, an item \
                     `component NAME { .. }` of a module, is a type: `resource C NAME { .. }` \
                     makes one",
                        );
                // `resource k3s cluster` for k3s.df's `component k3s`.
                let items: Vec<String> = self
                    .decls
                    .modules
                    .iter()
                    .filter(|(p, m)| {
                        m.component
                            && p.strip_prefix(&full)
                                .and_then(|r| r.strip_prefix('.'))
                                .is_some_and(|r| !r.contains('.'))
                    })
                    .map(|(p, _)| format!("`{written}{}`", &p[full.len()..]))
                    .collect();
                if !items.is_empty() {
                    d = d.with_help(format!(
                        "its components are types by their path: {}",
                        items.join(", ")
                    ));
                }
                Err(Box::new(d))
            }
            None if self.decls.deployed.iter().any(|d| d.path == full) => Err(Box::new(
                Diagnostic::error(
                    Span::default(),
                    format!("{written} is deployed by the tool; `use` it"),
                )
                .with_note(
                    "a stack is a module the tool uses, one deployment per key: `use` binds to its \
                 deployments, and `NAME[k=v].output` reads one",
                ),
            )),
            None => Err(Box::new(
                Diagnostic::error(Span::default(), format!("no component `{written}`"))
                    .with_note("a component is an item of a module, `component NAME { .. }`"),
            )),
        }
    }

    fn new_scope(&mut self, parent: usize) -> usize {
        self.decls.scopes.push(Scope {
            parent: Some(parent),
            ..Scope::default()
        });
        self.decls.scopes.len() - 1
    }
    /// Record the declarations of a statement list in `decl`; a component
    /// block nests in `outer`.
    fn collect(&mut self, file: u32, parent: &SyntaxNode, decl: usize, outer: usize) {
        let arity = |n: &SyntaxNode| n.children().filter(|c| c.kind() == BIND_ARG).count();
        for n in parent.children() {
            match n.kind() {
                INPUT => {
                    let name = word_text(&n, 1);
                    self.decls.scopes[decl].values.insert(name.clone());
                    self.decls.scopes[decl].input_nodes.insert(name, n.clone());
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
                // `input p from ..` or `input p`: rows of `p`, which its
                // `decl` declares (R-55).
                INPUT_RELATION => {
                    let name = word_text(&n, 1);
                    self.decls.relations.insert(name.clone());
                    self.decls.heads.insert(name.clone());
                    self.decls.scopes[decl].heads.insert(name.clone());
                    if terms(&n).next().is_none() {
                        self.decls.scopes[decl].relation_inputs.insert(name);
                    }
                }
                // `output p` alone: a relation exported (R-55).
                OUTPUT_DECL
                    if node(&n, TYPE_EXPR).is_none()
                        && terms(&n).next().is_none()
                        && node(&n, ATTR_DECL).is_none() =>
                {
                    let k = word_text(&n, 1);
                    self.decls.scopes[decl].relation_outputs.insert(k);
                }
                OUTPUT_DECL => {
                    let ty = node(&n, TYPE_EXPR).and_then(|t| self.resource_type(&t));
                    let k = word_text(&n, 1);
                    if let Some(t) = node(&n, TYPE_EXPR) {
                        let text = t.text().to_string().replace(char::is_whitespace, "");
                        self.decls.scopes[decl].output_types.insert(k.clone(), text);
                    }
                    let typed = self.decls.scopes[decl].outputs.get(&k).cloned().flatten();
                    self.decls.scopes[decl].outputs.insert(k, ty.or(typed));
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
                        .decl_nodes
                        .entry(name.clone())
                        .or_insert(n.clone());
                    self.decls.scopes[decl]
                        .arities
                        .entry(name)
                        .or_default()
                        .insert(arity(&n));
                }
                TYPE_DECL => {
                    self.decls.types.insert(dotted_text(&n, 1));
                }
                TYPE_ALIAS if node(&n, SIGNATURE).is_some() => {
                    let name = word_text(&n, 1);
                    self.decls.scopes[decl]
                        .signatures
                        .insert(name, (file, n.clone()));
                }
                // A resource of a provider's type, or a copy of a component
                // (R-113): which, once every module's components and every
                // scope's `use`s are known (`classify_resources`).
                RESOURCE => self.decls.pending.push((decl, file, n.clone())),
                // A provider's `use` binds its namespace, not a module.
                USE if provider_use(&n, self.units, &self.decls.deployed).is_some() => {}
                USE => {
                    // The path as written; `bind_instances` resolves it.
                    let (path, name) = use_parts(&n);
                    match self.decls.deployed.iter().position(|d| d.path == path) {
                        Some(i) => {
                            self.decls.scopes[decl].stacks.insert(name, i);
                        }
                        None if path.split('.').next() != Some("std") => {
                            let sc = &mut self.decls.scopes[decl];
                            sc.bound.entry(name.clone()).or_default().push((
                                path.clone(),
                                true,
                                n.clone(),
                            ));
                            sc.uses.entry(name).or_insert(path);
                        }
                        None => {}
                    }
                }
                COMPONENT => {
                    let scope = self.new_scope(outer);
                    let start: u32 = n.text_range().start().into();
                    self.decls.blocks.insert((file, start), scope);
                    let name = word_text(&n, 1);
                    let path = match self.decls.paths.get(&file) {
                        Some(m) => format!("{m}.{name}"),
                        None => name.clone(),
                    };
                    self.decls.modules.insert(
                        path.clone(),
                        ModDecl {
                            scope,
                            component: true,
                        },
                    );
                    self.decls.scopes[decl].components.insert(name, path);
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
                        self.decls.scopes[decl].heads.insert(name.clone());
                        self.decls.relations.insert(name);
                    }
                }
                _ => {}
            }
        }
    }

    /// A resource header's name when it is static: a string with no holes,
    /// or a bare name (R-76). A bare name the block's clauses bind is the
    /// error `block_stmt` reports, and names nothing here.
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

    /// The name token of a resource header.
    fn header_token(&self, n: &SyntaxNode) -> Option<SyntaxToken> {
        header_name(n)
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
        if self.decls.entries.contains(&scope) {
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

    /// The scopes from `scope` out to the body of the component or module
    /// it is in, that body's included: what is the body's own. At the
    /// program's top level, every scope.
    fn own_scopes(&self, scope: usize) -> Vec<usize> {
        let bodies: BTreeSet<usize> = self.decls.modules.values().map(|m| m.scope).collect();
        let mut out = Vec::new();
        for s in self.chain_of(scope) {
            out.push(s);
            if bodies.contains(&s) {
                break;
            }
        }
        out
    }

    /// An instance's scope, `t`, as a read in `scope` writes it: relative
    /// when the copy is the body's own (expansion puts the body's scope in
    /// front of it), else its user's, marked `scope(t)` so that expansion
    /// leaves it as it is (`modules::ABSOLUTE`).
    fn scope_term(&self, scope: usize, declared: usize, t: Term) -> Term {
        match self.own_scopes(scope).contains(&declared) {
            true => t,
            false => func(crate::modules::ABSOLUTE, vec![t]),
        }
    }

    /// The address of the resource `name` in scope, as a read in `scope`
    /// writes it: the body's own is relative (expansion scopes it), its
    /// user's marked as written (`scope_term`).
    fn resource_addr(&self, scope: usize, name: &str) -> Term {
        let declared = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.decls.scopes[*s].resources.contains_key(name))
            .unwrap_or(scope);
        self.scope_term(scope, declared, str_term(&crate::ir::name_segment(name)))
    }

    /// The instance `name` in scope: the scope that declares it and its
    /// component's path.
    fn instance_in(&self, scope: usize, name: &str) -> Option<(usize, String)> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.decls.scopes[s]
                .instances
                .get(name)
                .map(|p| (s, p.clone()))
        })
    }

    /// The stack `name` a `use` in scope binds.
    fn stack_in(&self, scope: usize, name: &str) -> Option<Deployed> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.decls.scopes[s]
                .stacks
                .get(name)
                .map(|&i| self.decls.deployed[i].clone())
        })
    }

    /// The module `name` a `use` in scope binds: its path.
    fn use_in(&self, scope: usize, name: &str) -> Option<String> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].uses.get(name).cloned())
    }

    /// The component `name` reads as in scope: its path.
    fn component_in(&self, scope: usize, name: &str) -> Option<String> {
        self.chain_of(scope).into_iter().find_map(|s| {
            let sc = &self.decls.scopes[s];
            sc.components
                .get(name)
                .or_else(|| sc.copied.get(name))
                .cloned()
        })
    }

    /// Whether `name` is an instance in scope whose component names
    /// nothing: its `instance` statement is the error.
    fn unbound_instance(&self, scope: usize, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.decls.scopes[s].unbound.contains(name))
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

    /// The input `name` in scope when its type is a resource type, `ref(T)`
    /// or `T` (R-101): `T`.
    fn input_ref(&self, scope: usize, name: &str) -> Option<String> {
        let n = self
            .chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].input_nodes.get(name).cloned())?;
        let ty = node(&n, TYPE_EXPR)?;
        let typ = dotted_text(&ty, 0);
        let typ = match node(&ty, TYPE_EXPR) {
            Some(inner) if typ == "ref" => dotted_text(&inner, 0),
            Some(_) => return None,
            None => typ,
        };
        (typ.contains('.') && self.decls.types.contains(&typ)).then_some(typ)
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

    /// The reference a `let` row's term names, if it names one: a resource
    /// (by name in scope or `T[e]`), a live object, or another `let`
    /// holding one.
    fn term_vtype(&self, scope: usize, t: &SyntaxNode, depth: usize) -> Option<VType> {
        match declared_ref(t) {
            Some(typ) => Some(VType::Ref(typ)),
            None => self.written_vtype(scope, t, depth),
        }
    }

    /// The reference a `let` row's term names as written, whatever the
    /// row declares.
    fn written_vtype(&self, scope: usize, t: &SyntaxNode, depth: usize) -> Option<VType> {
        let c = Chain::of(t)?;
        let index_then_end = |ops: &[Op]| matches!(ops, [Op::Index(ts, _)] if ts.len() == 1);
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

    /// A read whose head names a resource and something else in scope is
    /// an error (R-76): a scope's reads are one namespace, so `nodes` with
    /// `input nodes` and `resource net.vpc nodes` names two things. A
    /// module's item (`config.base_domain`), an instance's output and a
    /// stack's or a component's copy (`platform[env]`) read the other one;
    /// the resource always reads by its type, `net.vpc["nodes"]`.
    fn shared_name(&mut self, scope: usize, c: &Chain, span: Span) -> L<bool> {
        let h = c.head.as_str();
        let Some(types) = self.resource(scope, h) else {
            return Ok(false);
        };
        let field = match c.ops.first() {
            Some(Op::Field(f)) => Some(f.as_str()),
            _ => None,
        };
        let indexed = matches!(c.ops.first(), Some(Op::Index(..) | Op::Keyed(..)));
        let items = |sc: &Scope| -> Vec<String> {
            sc.values
                .iter()
                .chain(sc.outputs.keys())
                .chain(sc.resources.keys())
                .cloned()
                .collect()
        };
        let (what, other) = if let Some(s) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.decls.scopes[*s].values.contains(h))
        {
            let kind = if self.decls.scopes[s].input_nodes.contains_key(h) {
                "the input"
            } else {
                "the let"
            };
            (kind.to_string(), None)
        } else if let Some(path) = self.use_in(scope, h) {
            let own = self
                .decls
                .modules
                .get(&path)
                .map(|m| items(&self.decls.scopes[m.scope]))
                .unwrap_or_default();
            if field.is_some_and(|f| own.iter().any(|i| i == f)) {
                return Ok(true);
            }
            (
                "the module".to_string(),
                own.first().map(|i| format!("`{h}.{i}`")),
            )
        } else if let Some((_, path)) = self.instance_in(scope, h) {
            let outputs: Vec<String> = self
                .decls
                .modules
                .get(&path)
                .map(|m| self.decls.scopes[m.scope].outputs.keys().cloned().collect())
                .unwrap_or_default();
            if field.is_some_and(|f| outputs.iter().any(|o| o == f)) {
                return Ok(true);
            }
            (
                format!("the resource {h} of the component {path}"),
                outputs.first().map(|o| format!("`{h}.{o}`")),
            )
        } else if self.stack_in(scope, h).is_some() || self.component_in(scope, h).is_some() {
            // A stack or a component is read by a copy, `platform[env]`,
            // which a resource never is: no read is ambiguous.
            return Ok(indexed);
        } else {
            return Ok(false);
        };
        let rest: String = c
            .ops
            .iter()
            .map(|o| match o {
                Op::Field(f) => format!(".{f}"),
                Op::Index(ts, _) => format!(
                    "[{}]",
                    ts.iter()
                        .map(|t| t.text().to_string().trim().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Op::Keyed(kv, _) => format!(
                    "[{}]",
                    kv.iter()
                        .map(|(k, v)| format!("{k}={}", v.text().to_string().trim()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
            .collect();
        let resources = types
            .iter()
            .map(|t| format!("{t}[\"{h}\"]"))
            .collect::<Vec<_>>();
        let resource = match &resources[..] {
            [one] => format!("the resource {one}"),
            _ => format!("the resources {}", resources.join(", ")),
        };
        let read = resources
            .iter()
            .map(|r| format!("{r}{rest}"))
            .collect::<Vec<_>>()
            .join(" or ");
        let help = match other {
            Some(o) => format!("read {o} or {read}"),
            None => format!("read {read}, or name one of them otherwise"),
        };
        self.error(span, format!("`{h}` names {what} and {resource}: {help}"))
    }

    /// Inside a module's or a component's body, what the body declares
    /// wins a read over what its user's scope brings in (R-101): the
    /// module's own `resource k8s.namespace traefik` over its user's `use
    /// traefik`. `Some(true)`: the body's resource is read; `Some(false)`:
    /// the body's value, module or copy is; `None`: neither is the body's
    /// alone (the program's top level, where R-76's ambiguity stands).
    fn own_wins(&self, scope: usize, name: &str) -> Option<bool> {
        let own = self.own_scopes(scope);
        let names = |s: &Scope| {
            s.values.contains(name)
                || s.uses.contains_key(name)
                || s.instances.contains_key(name)
                || s.stacks.contains_key(name)
        };
        let resource = |s: &Scope| s.resources.contains_key(name);
        let (res_own, other_own) = own.iter().fold((false, false), |(r, o), s| {
            let sc = &self.decls.scopes[*s];
            (r || resource(sc), o || names(sc))
        });
        if res_own && !other_own && self.names_other(scope, name) {
            return Some(true);
        }
        if other_own && !res_own && self.resource(scope, name).is_some() {
            return Some(false);
        }
        None
    }

    /// Whether `name` names a value, a used module or an instance in
    /// scope: what a resource's bare name may share ambiguously.
    fn names_other(&self, scope: usize, name: &str) -> bool {
        self.is_value(scope, name)
            || self.use_in(scope, name).is_some()
            || self.instance_in(scope, name).is_some()
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

    fn unit(&mut self, i: usize) -> Vec<Stmt> {
        let unit = &self.units[i];
        let (file, root) = (unit.file, unit.root.clone());
        let saved = self.file;
        self.file = file;
        let scope = self.decls.files[&file];
        let mut statements = Vec::new();
        for n in root.children() {
            match n.kind() {
                // `edition` is a syntax error (R-68).
                ERROR | EDITION => {}
                _ => statements.extend(self.stmt(&n, scope, &Rc::default())),
            }
        }
        // A module's file lowers to the module it is (R-65), named by its
        // path.
        if let Some(path) = self.decls.paths.get(&file).cloned() {
            let m = self.decls.modules[&path].clone();
            let r = root.text_range();
            statements = vec![Stmt::Module(Module {
                name: path,
                component: m.component,
                body: statements,
                span: Span {
                    end: u32::from(r.start()),
                    ..self.span_of(r)
                },
            })];
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
        let aggs = std::mem::take(&mut self.aggs);
        let mut out = self.stmt1(n, scope, outer).unwrap_or_default();
        if !self.aggs.is_empty() {
            let span = self.span(n);
            out = self.fold_aggregates(out, span).unwrap_or_default();
        }
        self.aggs = aggs;
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
        // A tuple pattern on the left of `=` may hold `_` (R-58).
        fn side(t: &Term) -> bool {
            match t {
                Term::List(_) => in_func(t),
                t => has(t),
            }
        }
        fn lits(body: &[Lit]) -> bool {
            body.iter().any(|l| match l {
                Lit::Pos(a) | Lit::Not(a) => {
                    a.args.iter().any(in_func)
                        || a.record.as_ref().is_some_and(|r| r.values().any(in_func))
                }
                Lit::Eq(a, b) => side(a) || has(b),
                Lit::Neq(a, b) | Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                    has(a) || has(b)
                }
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
            instances: outer.instances.clone(),
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
            } else if let Some(rhs) = ts.next().and_then(|t| Chain::of(&t)) {
                let rhs = self.type_each(&rc, rhs);
                if let Some(t) = self.chain_type(&rc, &rhs) {
                    // A type under several names (R-115): which one is
                    // the resource's.
                    let typ = match self.covering(&t).len() {
                        1 => str_term(&t),
                        _ => var(&fresh(&mut rc, "Type")),
                    };
                    rc.types.insert(lhs.head, typ);
                } else if let Some(path) = self.component_of(&rc, &rhs) {
                    rc.instances.insert(lhs.head, path);
                } else if let Some(ns) = self.namespace_of(&rc, &rhs) {
                    let tv = fresh(&mut rc, "Type");
                    rc.types.insert(lhs.head.clone(), var(&tv));
                    rc.namespaces.insert(lhs.head, ns);
                }
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
            INPUT => {
                let name = word_text(n, 1);
                // `input k { f: T [= d] [check B] .. }` (R-54).
                let fields = self.input_fields(n, scope, outer)?;
                let ty = match node(n, TYPE_EXPR) {
                    Some(t) => self.input_type(&t),
                    None => crate::inputs::fields_type(&fields),
                };
                let key = is_key(n);
                if key && let Some(path) = self.decls.paths.get(&self.file) {
                    return self.error(
                        span,
                        format!(
                            "key {name} in {path}, which is not a stack: a key selects a \
                             deployment, and only a stack is deployed; a component's inputs \
                             are `input`"
                        ),
                    );
                }
                if key && n.parent().is_some_and(|p| p.kind() != SOURCE_FILE) {
                    return self.error(
                        span,
                        format!(
                            "key {name} inside a block: a key selects the stack's deployment, \
                             so it is declared at the top of the stack's file"
                        ),
                    );
                }
                if key && matches!(&ty, TypeExpr::Apply(t, _) if t == "secret") {
                    let d = Diagnostic::error(span, format!("key {name} is a secret")).with_note(
                        "a key's value names the deployment: its state's directory and \
                             its registry entry",
                    );
                    self.diags.push(d);
                    return Err(Skip);
                }
                let mut rc = self.rc(n, scope, outer);
                let default = match terms(n).next() {
                    // A literal default is read as the declared type (R-31).
                    Some(t) => match crate::types::literal(
                        &crate::types::of_expr(&ty),
                        self.constant(&mut rc, &t)?,
                    ) {
                        Ok(d) => Some(d),
                        Err(why) => {
                            return self.error(self.span(&t), format!("input {name} {why}"));
                        }
                    },
                    None => None,
                };
                let refinement = self.refinement(n, scope)?;
                // `input k: T where B` (R-104): declared where `B` holds. A
                // clause that reads the input itself is a check misspelled.
                if let Some(c) = node(n, CLAUSE)
                    && c.descendants()
                        .filter_map(|x| Chain::of(&x))
                        .any(|x| x.head == name)
                {
                    let head = n.text().to_string();
                    let at: usize = (c.text_range().start() - n.text_range().start()).into();
                    let body = c.text().to_string();
                    let body = body.trim_start().trim_start_matches("where").trim();
                    let d = Diagnostic::error(
                        self.span(&c),
                        format!("the clause of input {name} reads {name}: a clause picks where it is declared"),
                    )
                    .with_help(format!(
                        "a refinement is spelled `check` (R-1): `{} check {body}`",
                        head[..at].trim()
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                let guard = self.clauses(&mut rc, n)?;
                if key && !guard.is_empty() {
                    return self.error(
                        span,
                        format!(
                            "key {name} has a clause: a key names the deployment, so every \
                             deployment has it"
                        ),
                    );
                }
                let mut out = self.inputs_named(n, &name, &guard, span)?;
                out.insert(
                    0,
                    Stmt::Input(InputDecl {
                        name,
                        ty,
                        default,
                        refinement,
                        key,
                        guard,
                        fields,
                        span,
                    }),
                );
                Ok(out)
            }
            INPUT_RELATION => self.relation_input(n, scope, outer),
            OUTPUT_DECL => self.output(n, scope, outer),
            // An alias lowers to nothing: each use is its type.
            TYPE_ALIAS => Ok(Vec::new()),
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
                // R-60: one persistence concept, `memo.first`, said by the
                // program where a value is kept, not by the extern.
                if let Some(t) = tokens(n).find(|t| t.text() == "persist") {
                    let r = t.text_range();
                    let at = Span {
                        start: u32::from(r.start()),
                        end: u32::from(r.end()),
                        ..span
                    };
                    let d = Diagnostic::error(at, format!("extern {name}: `persist` is gone"))
                        .with_help(
                            "keep an answer where it is read: `memo.first(KEY, CANDIDATE, VALUE)` \
                             keeps the first candidate given for KEY",
                        );
                    self.diags.push(d);
                    return Err(Skip);
                }
                one(Stmt::ExternFn(ExternFn { name, args, span }))
            }
            TYPE_DECL => {
                let name = dotted_text(n, 1);
                let attrs = self.attr_decls(n, scope)?;
                one(Stmt::Pending(Pending {
                    kind: PendingKind::TypeDecl { name, attrs },
                    span,
                }))
            }
            DECL => Ok(self.decl(n, scope, span)),
            COMPONENT => {
                let start: u32 = n.text_range().start().into();
                let inner = self.decls.blocks[&(self.file, start)];
                let name = word_text(n, 1);
                let path = self.decls.scopes[self.decl_scope(scope)]
                    .components
                    .get(&name)
                    .cloned()
                    .unwrap_or(name);
                if let Some(t) = node(n, TYPE_EXPR) {
                    self.check_signature(n, &t, scope);
                }
                let body = self.stmts(node(n, STMT_BLOCK), inner, outer);
                one(Stmt::Module(Module {
                    name: path,
                    component: true,
                    body,
                    span,
                }))
            }
            USE => self.use_stmt(n, scope, outer),
            LET => self.let_stmt(n, scope, outer),
            SET => self.set(n, scope, outer),
            RESOURCE if self.is_copy(n) => self.instance(n, scope, outer),
            RESOURCE => self.block_stmt(n, scope, outer),
            RULE | FACT => self.rule(n, scope, outer),
            CHECK => self.check(n, scope, outer),
            k => self.error(span, format!("unexpected {k:?}")),
        }
    }

    /// The fields of an object input's block form (R-54), each a
    /// declaration named by its field: `f: T [= d] [check B]`, a nested
    /// object `f: { .. }`. A field's default is read as its type (R-31).
    fn input_fields(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<InputDecl>> {
        let mut out: Vec<InputDecl> = Vec::new();
        let mut failed = false;
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let r = (|| {
                let span = self.span(&a);
                let path = node(&a, BLOCK_PATH).ok_or(Skip)?;
                let name = self.block_path(&path)?;
                if name.contains('.') || name.contains('[') {
                    return self.error(
                        span,
                        format!(
                            "a field is a name: a nested object is `{}: {{ .. }}`",
                            name.split('.').next().unwrap_or(&name)
                        ),
                    );
                }
                if let Some(first) = out.iter().find(|f| f.name == name) {
                    let d = Diagnostic::error(span, format!("field {name} is declared twice"))
                        .with_label(first.span, "first here");
                    self.diags.push(d);
                    return Err(Skip);
                }
                let fields = self.input_fields(&a, scope, outer)?;
                let ty = match node(&a, TYPE_EXPR) {
                    Some(t) => self.input_type(&t),
                    None => crate::inputs::fields_type(&fields),
                };
                let mut rc = self.rc(&a, scope, outer);
                let default = match terms(&a).next() {
                    Some(t) => match crate::types::literal(
                        &crate::types::of_expr(&ty),
                        self.constant(&mut rc, &t)?,
                    ) {
                        Ok(d) => Some(d),
                        Err(why) => {
                            return self.error(self.span(&t), format!("field {name} {why}"));
                        }
                    },
                    None => None,
                };
                let refinement = self.refinement(&a, scope)?;
                Ok(InputDecl {
                    name,
                    ty,
                    default,
                    refinement,
                    key: false,
                    guard: Vec::new(),
                    fields,
                    span,
                })
            })();
            match r {
                Ok(f) => out.push(f),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    // --- declarations that lower to themselves ----------------------------

    /// `decl p(a, b)` declares the relation `p/2` by its columns (H-11):
    /// one no rule of the program defines is fed from outside (a provider,
    /// a given fact); `decl p(a, b) mixed` lets it have both facts and
    /// rules. The column names are the named-argument form's.
    fn decl(&mut self, n: &SyntaxNode, scope: usize, span: Span) -> Vec<Stmt> {
        let pred = dotted_text(n, 1);
        let binds: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == BIND_ARG).collect();
        let fields: Vec<String> = binds.iter().map(|b| word_text(b, 0)).collect();
        let types = binds
            .iter()
            .map(|b| node(b, TYPE_EXPR).map(|t| self.type_expr(&t)))
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
        } else if !self.decls.scopes[self.decl_scope(scope)]
            .heads
            .contains(&pred)
        {
            // A relation the scope's own statements do not define: one in
            // another module of the same name is another relation (R-65).
            out.push(Stmt::Extern(e));
        }
        out.push(Stmt::Decl(Decl {
            pred,
            fields,
            types,
            span,
        }));
        out
    }

    /// `input p from TERM [where B]` (R-55): rows of the relation `p`, its
    /// columns `decl p(..)`'s. Several lines are one relation, their rows
    /// together, and facts the program states join them. `input p` alone,
    /// in a module or a component, is a relation its user gives the rows
    /// of, in the `use` or `instance` block.
    fn relation_input(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let pred = word_text(n, 1);
        let module = self.decls.paths.contains_key(&self.file)
            || n.parent().is_some_and(|p| p.kind() != SOURCE_FILE);
        let Some(source) = terms(n).next() else {
            if !module {
                return self.error(
                    span,
                    format!(
                        "`input {pred}` with no `from` is a module's relation, which its user \
                         gives: a stack gives a relation's rows, `input {pred} from ..`"
                    ),
                );
            }
            let arity = self
                .relation_decl(scope, &pred)
                .map_or(0, |d| d.children().filter(|c| c.kind() == BIND_ARG).count());
            return Ok(vec![Stmt::RelationInput(Extern { pred, arity, span })]);
        };
        if module {
            return self.error(
                span,
                format!(
                    "a module's relation is given by its user: declare `input {pred}`, and its \
                     user writes `{pred} from ..` in the `use` block"
                ),
            );
        }
        self.reject_facts(&source)?;
        let Some(decl) = self.relation_decl(scope, &pred) else {
            // No `decl`: the columns are the first source's (R-34).
            let cols = match self.read_columns.get(&pred) {
                Some(c) => c.clone(),
                None => {
                    let c = self.first_source_columns(&pred, n, &source, span)?;
                    self.read_columns.insert(pred.clone(), c.clone());
                    c
                }
            };
            let arity = cols.len();
            let mut rc = self.rc(n, scope, outer);
            let body = self.opt_body(&mut rc, n)?;
            let mut out = self.table(&mut rc, &pred, cols, n, body, span)?;
            out.push(Stmt::Mixed(Extern { pred, arity, span }));
            return Ok(out);
        };
        let cols = self.table_columns(&pred, &decl)?;
        let mut rc = self.rc(n, scope, outer);
        let body = self.opt_body(&mut rc, n)?;
        let mut out = self.table(&mut rc, &pred, cols, n, body, span)?;
        out.push(Stmt::Mixed(Extern {
            pred,
            arity: decl.children().filter(|c| c.kind() == BIND_ARG).count(),
            span,
        }));
        Ok(out)
    }

    /// The columns of `input p from FORMAT("path")` with no `decl`: the
    /// first row's of the document at `path`, read now (R-34). A source the
    /// compiler cannot read (a `git(..)` one, a path with holes, a missing
    /// file) is an error that says to declare the columns.
    fn first_source_columns(
        &mut self,
        pred: &str,
        n: &SyntaxNode,
        src: &SyntaxNode,
        span: Span,
    ) -> L<Vec<BindArg>> {
        let declare = format!("declare its columns: `decl {pred}(a: T, ..)`");
        if let Some(e) = self.old_loader(src) {
            return e.map(|_| Vec::new());
        }
        let read = self.loader_of(src).filter(|_| node(n, SELECTOR).is_none());
        let format = read.as_ref().map(|r| r.0.clone()).unwrap_or_default();
        let args: Vec<SyntaxNode> = read
            .and_then(|(_, r)| node(&r, ARG_LIST))
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let path = match args.as_slice() {
            [a] if a.kind() == LITERAL && !format.is_empty() => tokens(a)
                .find(|t| t.kind() == STRING)
                .filter(|t| !has_hole(t.text()))
                .and_then(|t| string_value(t.text()).ok()),
            _ => None,
        };
        let Some(path) = path else {
            let d = Diagnostic::error(
                span,
                format!(
                    "input {pred} from ..: {pred} has no `decl`, so its columns are its first \
                     source's, and that is no file path the compiler can read"
                ),
            )
            .with_help(declare);
            self.diags.push(d);
            return Err(Skip);
        };
        let base = crate::diag::location(span)
            .map(|(file, _, _)| crate::project::base_of(std::path::Path::new(&file)))
            .unwrap_or_default();
        let read = std::fs::read_to_string(base.join(&path))
            .map_err(anyhow::Error::from)
            .and_then(|text| crate::infer::document_columns(&format, pred, &text));
        match read {
            Ok(cols) => Ok(cols
                .into_iter()
                .map(|(name, ty)| BindArg {
                    input: false,
                    name,
                    ty,
                })
                .collect()),
            Err(e) => {
                let d = Diagnostic::error(
                    span,
                    format!(
                        "input {pred} from ..: {pred} has no `decl`, so its columns are its first \
                         source's, and {path} cannot be read for them: {e:#}"
                    ),
                )
                .with_help(declare);
                self.diags.push(d);
                Err(Skip)
            }
        }
    }

    /// The `decl` of the relation `pred` in scope.
    fn relation_decl(&self, scope: usize, pred: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].decl_nodes.get(pred).cloned())
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

    /// An input's type: a resource type `T` is `ref(T)`, a reference to a
    /// resource of it (`input namespace: k8s.namespace`), wherever a type
    /// alias does not name it.
    fn input_type(&mut self, n: &SyntaxNode) -> TypeExpr {
        let t = self.type_expr(n);
        self.refs_of(t)
    }

    /// `t` with each dotted name that is no alias read as `ref(T)`: a type
    /// of a namespace the compiler closes must be one it knows, and a name
    /// under a used module is a missing alias, both left to `check_type`.
    fn refs_of(&self, t: TypeExpr) -> TypeExpr {
        match t {
            TypeExpr::Name(n) => {
                let ns = n.split('.').next().unwrap_or_default();
                let module = self.decls.modules.contains_key(ns)
                    || self
                        .decls
                        .scopes
                        .iter()
                        .any(|s| s.uses.contains_key(ns) || s.components.contains_key(ns));
                let known = self.decls.types.contains(&n) || !self.decls.closed.contains(ns);
                if n.contains('.') && known && !module {
                    TypeExpr::Apply("ref".into(), vec![TypeExpr::Name(n)])
                } else {
                    TypeExpr::Name(n)
                }
            }
            TypeExpr::Apply(n, args) if n != "enum" && n != "ref" => {
                TypeExpr::Apply(n, args.into_iter().map(|a| self.refs_of(a)).collect())
            }
            TypeExpr::Object(fs) => {
                TypeExpr::Object(fs.into_iter().map(|(k, t)| (k, self.refs_of(t))).collect())
            }
            t => t,
        }
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
                    // R-134: a uri is RFC 3986's, not a browser's url.
                    if name == "url" && aliases && self.alias(n, &name).is_none() {
                        let d = Diagnostic::error(self.span(n), "unknown type url").with_help(
                            "the type is `uri`, RFC 3986's generic syntax: `let u: uri = \
                             \"https://example.com\"`",
                        );
                        self.diags.push(d);
                    }
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

    /// A block path or keypath as a stored path: `a.b`, `a[0].b`, a quoted
    /// segment unquoted unless it holds `.`, `[`, `]`, `/` or `"` (R-77).
    fn block_path(&mut self, n: &SyntaxNode) -> L<String> {
        let mut out = String::new();
        for t in tokens(n) {
            match t.kind() {
                DOT => out.push('.'),
                L_BRACKET | R_BRACKET | INT => out.push_str(t.text()),
                STRING => out.push_str(&crate::ir::path_key(&self.string(&t)?)),
                _ => out.push_str(t.text()),
            }
        }
        Ok(out)
    }

    /// A selector's quoted step `."k"` is one key: `.`, `[` or `]` inside
    /// it would read as a second step (a path's quoted segment carries
    /// any character, R-77).
    fn segment(&mut self, t: &SyntaxToken) -> L<String> {
        let s = self.string(t)?;
        if s.contains(['.', '[', ']']) {
            return self.error(
                self.span_of(t.text_range()),
                format!(
                    "the key {s:?} holds `.`, `[` or `]`, which a selector's step cannot carry"
                ),
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

    /// `input p from DOC [where B]`: a table (`crate::tables`), its
    /// columns `cols`. Its rows are the answers of the extern
    /// `table.FORMAT.p`, asked once the source is known: `p(Cols) :- B,
    /// reads, Path = SOURCE, table.FORMAT.p(Path, At, Cols)`; of a
    /// document value, `table.value.p` (`doc_source`).
    fn table(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        cols: Vec<BindArg>,
        n: &SyntaxNode,
        mut body: Vec<Lit>,
        span: Span,
    ) -> L<Vec<Stmt>> {
        let vars: Vec<Term> = cols
            .iter()
            .map(|c| var(&fresh(rc, &capitalise(&c.name))))
            .collect();
        let mut out = self.doc_source(rc, n, pred, cols, vars.clone(), &mut body)?;
        self.check_bound(rc, &body, &[])?;
        out.push(Stmt::Rule(RuleStmt {
            head: atom_at(pred, vars, span),
            body,
        }));
        Ok(out)
    }

    /// A table's columns, its relation's `decl`'s: each typed, with an
    /// input's types, never a secret.
    fn table_columns(&mut self, pred: &str, decl: &SyntaxNode) -> L<Vec<BindArg>> {
        let mut cols: Vec<BindArg> = Vec::new();
        for b in decl.children().filter(|c| c.kind() == BIND_ARG) {
            let name = word_text(&b, 0);
            let ty = node(&b, TYPE_EXPR).map(|t| self.type_expr(&t));
            let at = self.span(&b);
            if ty.is_none() {
                return self.error(
                    at,
                    format!("{pred} is read from a table: its column {name} needs a type"),
                );
            }
            if cols.iter().any(|c| c.name == name) {
                return self.error(at, format!("{pred}: two columns are named {name}"));
            }
            if let Some(Err(e)) = ty.as_ref().map(crate::inputs::check_type) {
                return self.error(at, format!("{pred}: column {name}: {e}"));
            }
            if matches!(&ty, Some(TypeExpr::Apply(t, _)) if t == "secret") {
                let d = Diagnostic::error(at, format!("{pred}: column {name} is a secret"))
                    .with_note(
                        "a table's rows are read in the clear and recorded in the plan file; \
                         a secret comes from a secret input, an extern's secret column or a std function such as `random.password`",
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
        Ok(cols)
    }

    /// The stack's settings as dform.toml gives them (R-29): each a
    /// constant, held by the program's `stack`.
    fn stack_settings(&mut self, st: &StackSource, entries: &[usize]) -> Option<Config> {
        let &first = entries.first()?;
        let scope = self.decls.files[&self.units[first].file];
        let (saved_file, saved_offset) = (self.file, self.offset);
        let mut config = Vec::new();
        for s in &st.settings {
            let (text, offset) = match &s.value {
                SettingValue::Plain(t) => {
                    config.push((s.key.clone(), t.clone(), s.span));
                    continue;
                }
                SettingValue::Term { text, offset } => (text.replace("{stack}", &st.name), *offset),
            };
            self.file = s.span.file;
            self.offset = offset;
            let _ = self.setting_term(&text, s.span).and_then(|src| {
                let mut rc = self.rc(&src, scope, &Rc::default());
                self.calls(Calls::Data, |l| l.constant(&mut rc, &src))
                    .map(|t| config.push((s.key.clone(), t, s.span)))
            });
        }
        (self.file, self.offset) = (saved_file, saved_offset);
        Some(Config {
            name: st.name.clone(),
            of: None,
            config,
            span: st.span,
        })
    }

    /// A setting's value, parsed as a term.
    fn setting_term(&mut self, text: &str, at: Span) -> L<SyntaxNode> {
        let parse = parse::parse_term(text);
        if let Some(e) = parse.errors.first() {
            let span = Span {
                start: self.offset + e.start as u32,
                end: self.offset + e.end as u32,
                ..at
            };
            return self.error(
                span,
                e.message
                    .replace("the end of the file", "the end of the value"),
            );
        }
        terms(&parse.syntax()).next().ok_or(Skip)
    }

    /// The rows of `table` a `from` statement `n` reads (R-39): its term a
    /// loader call, `FORMAT(PATH)` (`table_body`), or any other document
    /// value (an input, a `let`, a selection into one), read by the extern
    /// `table.value.TABLE(+doc, -at, ..)`; and its selector, `.f` and `[_]`
    /// steps into the document, in the table's name (`tables::selected`).
    fn doc_source(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
        outs: Vec<Term>,
        body: &mut Vec<Lit>,
    ) -> L<Vec<Stmt>> {
        let src = terms(n).next().ok_or(Skip)?;
        let selector = node(n, SELECTOR)
            .map(|sel| self.selector(&sel))
            .transpose()?
            .unwrap_or_default();
        let name = crate::tables::selected(table, &selector);
        self.reject_facts(&src)?;
        // A read (`io.read`, a decode of one), or a call that is no
        // function: a table's source (the error names the formats).
        let callee = (src.kind() == CALL).then(|| self.callee(&src)).flatten();
        if self.loader_of(&src).is_some()
            || callee.is_some_and(|c| crate::functions::registry().get(&c).is_none())
        {
            return self.table_body(rc, &src, &name, cols, outs, body);
        }
        let doc = self.term(rc, &src, Pos::Content, body)?;
        let d = var(&fresh(rc, "Doc"));
        body.push(Lit::Eq(d.clone(), doc));
        let at = var(&fresh(rc, "At"));
        let mut args = vec![d, at];
        args.extend(outs);
        let ext = crate::tables::extern_name(crate::tables::VALUE, &name);
        let span = self.span(&src);
        body.push(Lit::Pos(atom_at(&ext, args, span)));
        let mut ins = vec![
            BindArg {
                input: true,
                name: "doc".into(),
                ty: None,
            },
            BindArg {
                input: false,
                name: "at".into(),
                ty: None,
            },
        ];
        ins.extend(cols);
        Ok(vec![Stmt::ExternFn(ExternFn {
            name: ext,
            args: ins,
            span,
        })])
    }

    /// A read as a term (R-39, R-155): `io.read(LOCATION)` (the whole of
    /// it, a string) and a decode of one, `yaml.decode(io.read(LOCATION))`,
    /// `toml`, `json`, `csv` (a list of objects by its header), a location
    /// a path or a uri (R-153), is the document, a value: `V` reading
    /// `table.FORMAT.document(Location, At, V)`, whose answer the plan file
    /// records and the controller watches as a table's, and whose rows keep
    /// their place (R-131). `None` for any other call; an old loader
    /// (`yaml(..)`) no relation of the program names is an error naming
    /// the composition.
    fn loader_call(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> Option<L<Term>> {
        if let Some(e) = self.old_loader(n) {
            return Some(e);
        }
        let (format, _) = self.loader_of(n)?;
        let table = crate::tables::DOCUMENT.to_string();
        let v = var(&fresh(rc, &capitalise(&format)));
        let cols = vec![BindArg {
            input: false,
            name: "value".into(),
            ty: Some(TypeExpr::Name("any".into())),
        }];
        Some(
            self.table_body(rc, n, &table, cols, vec![v.clone()], pre)
                .map(|stmts| {
                    for st in stmts {
                        let known = |h: &Stmt| matches!((h, &st), (Stmt::ExternFn(a), Stmt::ExternFn(b)) if a.name == b.name);
                        if !self.helpers.iter().any(known) {
                            self.helpers.push(st);
                        }
                    }
                    v
                }),
        )
    }

    /// A read's format and its `io.read(..)` call (R-155): `io.read(L)`
    /// is `text`, `F.decode(io.read(L))` is `F` for a format the tables
    /// read; `None` for any other term.
    fn loader_of(&self, n: &SyntaxNode) -> Option<(String, SyntaxNode)> {
        if n.kind() != CALL {
            return None;
        }
        let name = self.callee(n)?;
        if name == crate::tables::READ {
            return Some(("text".to_string(), n.clone()));
        }
        let format = name.strip_suffix(".decode")?;
        if !crate::tables::FORMATS.contains(&format) || format == "text" {
            return None;
        }
        let list = node(n, ARG_LIST)?;
        let args: Vec<SyntaxNode> = terms(&list).collect();
        match args.as_slice() {
            [a] if a.kind() == CALL && self.callee(a).as_deref() == Some(crate::tables::READ) => {
                Some((format.to_string(), a.clone()))
            }
            _ => None,
        }
    }

    /// `yaml(LOCATION)`, `text(..)` and the other loaders a program wrote
    /// before R-155, when no relation of the program has the name: an
    /// error naming the read and its decode.
    fn old_loader(&mut self, n: &SyntaxNode) -> Option<L<Term>> {
        if n.kind() != CALL {
            return None;
        }
        let name = self.callee(n)?;
        if !crate::tables::FORMATS.contains(&name.as_str()) || self.decls.relations.contains(&name)
        {
            return None;
        }
        let arg = node(n, ARG_LIST)
            .and_then(|l| terms(&l).next())
            .map(|a| a.text().to_string())
            .unwrap_or_else(|| "LOCATION".to_string());
        let read = format!("{}({arg})", crate::tables::READ);
        let new = match name.as_str() {
            "text" => read,
            f => format!("{f}.decode({read})"),
        };
        let d = Diagnostic::error(
            self.span(n),
            format!("`{name}(..)` is gone (R-155): a location is read by `io.read`"),
        )
        .with_help(format!(
            "`{new}`: `io.read` reads the text, a format's `decode` the document in it, its \
             rows at their lines"
        ));
        self.diags.push(d);
        Some(Err(Skip))
    }

    /// `from facts(..)` is gone (R-39): an error naming what replaces it.
    fn reject_facts(&mut self, src: &SyntaxNode) -> L<()> {
        if src.kind() != CALL || self.callee(src).as_deref() != Some("facts") {
            return Ok(());
        }
        let d = Diagnostic::error(
            self.span(src),
            "`facts(..)` is gone (R-39): a `.df` file of facts is a module",
        )
        .with_help(
            "`use data.releases` reads data/releases.df, its relations `releases.p(..)`; rows \
             from outside are a table, `input p from csv.decode(io.read(\"data/p.csv\"))`",
        );
        self.diags.push(d);
        Err(Skip)
    }

    /// A selector's steps as text: `.teams[_].services`.
    fn selector(&mut self, n: &SyntaxNode) -> L<String> {
        let mut out = String::new();
        for t in tokens(n) {
            match t.kind() {
                DOT | L_BRACKET | R_BRACKET => out.push_str(t.text()),
                STRING => out.push_str(&self.segment(&t)?),
                _ => out.push_str(t.text()),
            }
        }
        Ok(out)
    }

    /// A table's source, `FORMAT(LOCATION)`, a path or a uri (R-153),
    /// read into `body` and asked of its extern there, its outputs `outs`;
    /// the extern's declaration.
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
        let bad = |l: &mut Self, what: &str| -> L<Vec<Stmt>> {
            let d = Diagnostic::error(
                span,
                format!("{what}: a table's source is FORMAT.decode(io.read(LOCATION))"),
            )
            .with_help(format!(
                "FORMAT is one of {}; LOCATION is a path, `\"data/p.csv\"`, or a uri, \
                     `\"git+https://HOST/OWNER/REPO/PATH?ref=TAG\"`, `\"ssh://USER@HOST/PATH\"`",
                crate::tables::DECODERS.join(", ")
            ));
            l.diags.push(d);
            Err(Skip)
        };
        if let Some(e) = self.old_loader(src) {
            return e.map(|_| Vec::new());
        }
        let (format, read) = match (self.loader_of(src), self.callee(src)) {
            (Some(r), _) => r,
            (None, Some(f)) if src.kind() == CALL => {
                return bad(self, &format!("unknown format {f}"));
            }
            _ => return bad(self, "not a format"),
        };
        let args: Vec<SyntaxNode> = node(&read, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let [arg] = args.as_slice() else {
            return bad(self, "`io.read` takes one location");
        };
        if arg.kind() == CALL && self.callee(arg).as_deref() == Some("git") {
            let d = Diagnostic::error(
                self.span(arg),
                "`git(..)` is gone (R-153): a repository's file is a location",
            )
            .with_help(format!(
                "`io.read(\"git+https://HOST/OWNER/REPO/PATH?ref=TAG\")` (`git+ssh://` over \
                     ssh, `git+file:REPO/PATH?ref=TAG` for a repository in the project), read at \
                     the commit the ref names, which the plan file records"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let t = self.term(rc, arg, Pos::Content, body)?;
        let given = var(&fresh(rc, "Path"));
        body.push(Lit::Eq(given.clone(), t));
        let at = var(&fresh(rc, "At"));
        let mut args = vec![given, at];
        args.extend(outs);
        let mut ins = vec![
            BindArg {
                input: true,
                name: "path".into(),
                ty: None,
            },
            BindArg {
                input: false,
                name: "at".into(),
                ty: None,
            },
        ];
        ins.extend(cols);
        let name = crate::tables::extern_name(&format, table);
        body.push(Lit::Pos(atom_at(&name, args, span)));
        Ok(vec![Stmt::ExternFn(ExternFn {
            name,
            args: ins,
            span,
        })])
    }

    /// `output k [: T] = t [where B]` (H-7): the declaration, when typed, and
    /// its value; a value that reads, or one with a condition, is the rule
    /// `output(k, t') :- B, reads`.
    fn output(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if node(n, ATTR_DECL).is_some() {
            return self.output_object(n, scope, outer);
        }
        // `output p`: the relation `p` exported (R-55), read as
        // `copy.p(..)`, `c[t].p(..)`, `stack[k=v].p(..)`.
        if node(n, TYPE_EXPR).is_none() && terms(n).next().is_none() {
            let arities: BTreeSet<usize> = self
                .chain_of(scope)
                .into_iter()
                .filter_map(|s| self.decls.scopes[s].arities.get(&name))
                .flatten()
                .copied()
                .collect();
            match arities.iter().collect::<Vec<_>>().as_slice() {
                [n] => {
                    // A column the `decl` types by a resource type holds
                    // the copy's resource.
                    let refs = match self.relation_decl(scope, &name) {
                        Some(d) => d
                            .children()
                            .filter(|c| c.kind() == BIND_ARG)
                            .map(|b| {
                                node(&b, TYPE_EXPR).is_some_and(|t| {
                                    self.resource_type(&t).is_some() || dotted_text(&t, 0) == "ref"
                                })
                            })
                            .collect(),
                        None => vec![false; **n],
                    };
                    return Ok(vec![Stmt::Output(OutputDecl {
                        name,
                        ty: None,
                        value: None,
                        relation: Some(refs),
                        span,
                    })]);
                }
                [] => {}
                _ => {
                    return self.error(
                        span,
                        format!("output {name}: the relation {name} has several arities"),
                    );
                }
            }
        }
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
        let addr_typed = matches!(&ty, TypeExpr::Name(n) if n == "addr");
        if self.outputs.insert((scope, name.clone())) {
            out.push(Stmt::Output(OutputDecl {
                name: name.clone(),
                ty: Some(ty),
                value: None,
                relation: None,
                span,
            }));
        }
        let Some(t) = terms(n).next() else {
            let d =
                Diagnostic::error(span, format!("output {name} has no value")).with_help(format!(
                    "an output is one statement, `output {name}: T = term`; `output {name}` \
                     alone exports the relation {name}, which is declared or defined here"
                ));
            self.diags.push(d);
            return Err(Skip);
        };
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        // An output typed by a resource type holds its address (`scoped` by
        // the module); any other output is a value, a resource in it the
        // reference (R-43).
        let value = match Chain::of(&t) {
            Some(c)
                if addr_typed
                    && c.is_bare()
                    && !rc.vars.contains_key(&c.head)
                    && self.resource(scope, &c.head).is_some() =>
            {
                str_term(&c.head)
            }
            _ if addr_typed => self.term(&mut rc, &t, Pos::Whole, &mut pre)?,
            _ => self.term(&mut rc, &t, Pos::Value, &mut pre)?,
        };
        if pre.is_empty() && !has_body {
            self.check_bound(&rc, &[], &[&value])?;
            out.push(Stmt::Output(OutputDecl {
                name,
                ty: None,
                value: Some(value),
                relation: None,
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

    /// `output k { f [: T] = t, g: { .. } } [where B]` (R-55): an object
    /// output by its fields, one value, typed by its fields' types (`any`
    /// where a field gives none).
    fn output_object(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let (value, ty) = self.output_fields(&mut rc, n, &mut pre)?;
        let mut out = Vec::new();
        if self.outputs.insert((scope, name.clone())) {
            out.push(Stmt::Output(OutputDecl {
                name: name.clone(),
                ty: Some(ty),
                value: None,
                relation: None,
                span,
            }));
        }
        let head = atom_at("output", vec![str_term(&name), value], span);
        self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?;
        out.push(if pre.is_empty() {
            Stmt::Output(OutputDecl {
                name,
                ty: None,
                value: Some(head.args[1].clone()),
                relation: None,
                span,
            })
        } else {
            Stmt::Rule(RuleStmt { head, body: pre })
        });
        Ok(out)
    }

    /// An object output's fields: the object term and its type.
    fn output_fields(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<(Term, TypeExpr)> {
        let mut value = BTreeMap::new();
        let mut types = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let span = self.span(&a);
            let field = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            if field.contains('.') || field.contains('[') || value.contains_key(&field) {
                return self.error(
                    span,
                    format!("field {field}: a field is a name, given once"),
                );
            }
            let (v, t) = if node(&a, ATTR_DECL).is_some() {
                self.output_fields(rc, &a, pre)?
            } else {
                let Some(t) = terms(&a).next() else {
                    return self.error(span, format!("field {field} has no value: `{field} = t`"));
                };
                let v = self.term(rc, &t, Pos::Value, pre)?;
                let ty = node(&a, TYPE_EXPR)
                    .map(|t| self.type_expr(&t))
                    .unwrap_or_else(|| TypeExpr::Name("any".into()));
                (v, ty)
            };
            types.push((field.clone(), t));
            value.insert(field, v);
        }
        Ok((Term::Obj(value), TypeExpr::Object(types)))
    }

    /// The clause of a block statement: the `where` body after its block.
    fn clauses(&mut self, rc: &mut Rc, stmt: &SyntaxNode) -> L<Vec<Lit>> {
        let mut lits = Vec::new();
        for c in stmt.children().filter(|c| c.kind() == CLAUSE) {
            lits.extend(node(&c, BODY).ok_or(Skip)?.children());
        }
        self.lits(rc, &lits)
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
                let value = self.entry_value(rc, &a, Pos::Value, reads)?;
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

    /// The component signature `name` in scope: the file and the `type`
    /// statement. A dotted name is another module's, by its path or the
    /// name its `use` binds (`db.database`).
    fn signature(&self, scope: usize, name: &str) -> Option<(u32, SyntaxNode)> {
        match name.rsplit_once('.') {
            Some((m, s)) => {
                let path = self.module_path_of(scope, m);
                let at = self.decls.modules.get(&path)?.scope;
                self.decls.scopes[at].signatures.get(s).cloned()
            }
            None => self
                .chain_of(scope)
                .into_iter()
                .find_map(|s| self.decls.scopes[s].signatures.get(name).cloned()),
        }
    }

    /// A type as its declaration in `file` reads, aliases expanded: what
    /// a signature and a component compare.
    fn type_text_in(&mut self, file: u32, t: Option<SyntaxNode>) -> String {
        let Some(t) = t else {
            return "any".to_string();
        };
        let saved = std::mem::replace(&mut self.file, file);
        let ty = self.type_expr(&t);
        self.file = saved;
        crate::inputs::type_text(&ty)
    }

    /// `component C: T { .. }` (R-104): C declares every input and output
    /// the signature `T` does, of its type; an input `T` does not declare
    /// has a default, so a copy picked by `T` can be given what `T` says
    /// alone. Each difference is an error at the component.
    fn check_signature(&mut self, n: &SyntaxNode, t: &SyntaxNode, scope: usize) {
        let span = self.span(n);
        let comp = word_text(n, 1);
        let sig = dotted_text(t, 0);
        let Some((file, decl)) = self.signature(scope, &sig) else {
            self.diags.push(
                Diagnostic::error(self.span(t), format!("no component signature `{sig}`"))
                    .with_note("a signature is `type NAME = component { input ..  output .. }`"),
            );
            return;
        };
        let items = |b: Option<SyntaxNode>| -> Vec<SyntaxNode> {
            b.into_iter()
                .flat_map(|b| b.children().collect::<Vec<_>>())
                .collect()
        };
        let sig_items = items(node(&decl, SIGNATURE).and_then(|s| node(&s, STMT_BLOCK)));
        let own = items(node(n, STMT_BLOCK));
        let find = |kind: SyntaxKind, name: &str| {
            own.iter()
                .find(|c| c.kind() == kind && word_text(c, 1) == name)
                .cloned()
        };
        let at = self.span(&decl);
        let mut errors = Vec::new();
        for c in &sig_items {
            let name = word_text(c, 1);
            match c.kind() {
                INPUT | OUTPUT_DECL => {
                    let what = if c.kind() == INPUT { "input" } else { "output" };
                    let want = self.type_text_in(file, node(c, TYPE_EXPR));
                    match find(c.kind(), &name) {
                        None => errors.push(format!(
                            "component {comp} has no {what} {name}, which {sig} declares ({name}: {want})"
                        )),
                        Some(o) => {
                            let got = self.type_text_in(self.file, node(&o, TYPE_EXPR));
                            if got != want {
                                errors.push(format!(
                                    "{what} {name} of component {comp} is {got}; {sig} declares {want}"
                                ));
                            }
                        }
                    }
                }
                INPUT_RELATION => {
                    if find(INPUT_RELATION, &name).is_none() {
                        errors.push(format!(
                            "component {comp} takes no relation {name}, which {sig} declares"
                        ));
                    }
                }
                _ => errors.push(format!(
                    "{sig} is a signature: it declares inputs and outputs, not `{}`",
                    c.text()
                        .to_string()
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                )),
            }
        }
        // An input the signature does not declare has a default: a copy
        // picked by the signature is given what the signature says.
        for c in own.iter().filter(|c| c.kind() == INPUT) {
            let name = word_text(c, 1);
            let declared = sig_items
                .iter()
                .any(|s| s.kind() == INPUT && word_text(s, 1) == name);
            if !declared && terms(c).next().is_none() {
                errors.push(format!(
                    "input {name} of component {comp} is not in {sig} and has no default"
                ));
            }
        }
        for e in errors {
            self.diags
                .push(Diagnostic::error(span, e).with_label(at, format!("the signature {sig}")));
        }
    }

    /// The checks of an input declared more than once beside `n` (R-104):
    /// each under a clause, of one type; its own `__declared` row, and at
    /// the first the denies of two that both hold.
    fn inputs_named(
        &mut self,
        n: &SyntaxNode,
        name: &str,
        guard: &[Lit],
        span: Span,
    ) -> L<Vec<Stmt>> {
        let same: Vec<SyntaxNode> = n
            .parent()
            .into_iter()
            .flat_map(|p| p.children())
            .filter(|c| c.kind() == INPUT && word_text(c, 1) == name)
            .collect();
        if same.len() < 2 {
            return Ok(Vec::new());
        }
        if same.first() != Some(n) && !same.iter().all(|c| node(c, CLAUSE).is_some()) {
            let d = Diagnostic::error(
                span,
                format!("`{name}` is declared twice; give each a `where`"),
            )
            .with_label(self.span(&same[0]), "first here")
            .with_help(format!(
                "an input is declared once, or several times each under a clause that \
                     picks it (`input {name}: T where cloud == \"gcp\"`)"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let ty = |c: &SyntaxNode| {
            node(c, TYPE_EXPR).map(|t| t.text().to_string().replace(char::is_whitespace, ""))
        };
        if same.first() == Some(n) && same.iter().any(|c| ty(c) != ty(n)) {
            let mut d = Diagnostic::error(
                span,
                format!("input {name} is declared with another type in each declaration"),
            );
            for c in &same {
                d = d.with_label(
                    self.span(c),
                    format!("{name}: {}", ty(c).unwrap_or_default()),
                );
            }
            self.diags.push(d.with_help(
                "one input has one type: give each declaration the same, or each its own name",
            ));
            return Err(Skip);
        }
        let group = format!("input {name}");
        let i = same.iter().position(|c| c == n).unwrap_or_default();
        let mut out = vec![crate::modules::declared(&group, i, guard.to_vec(), span)];
        if i == 0 {
            let sites: Vec<(String, Span)> = same
                .iter()
                .map(|c| (format!("input {name}"), self.span(c)))
                .collect();
            out.extend(crate::modules::denies(&group, &sites));
        }
        Ok(out)
    }

    /// The declarations of `name` by a `use` or an `instance` beside `n`,
    /// in source order: the names of a scope's copies and imports are one
    /// namespace. A name may be declared more than once when every
    /// declaration of it has a clause (R-104); else the second is the
    /// error, naming the first.
    fn redeclared(&mut self, n: &SyntaxNode, name: &str) -> L<()> {
        let Some(parent) = n.parent() else {
            return Ok(());
        };
        // A provider's name is in the scope's one namespace too (R-112):
        // `use db` of a provider beside `use db` of a module is the error
        // two uses are.
        let same: Vec<SyntaxNode> = parent
            .children()
            .filter(|c| c.kind() == USE || self.is_copy(c))
            .filter(|c| {
                let other = match c.kind() {
                    USE => use_parts(c).1,
                    _ => copy_parts(c).1,
                };
                other == name
            })
            .collect();
        if same.len() < 2
            || same.first() == Some(n)
            || same.iter().all(|c| node(c, CLAUSE).is_some())
        {
            return Ok(());
        }
        let span = self.span(n);
        let first = self.span(&same[0]);
        let d = Diagnostic::error(
            span,
            format!("`{name}` is declared twice; give each a `where`"),
        )
        .with_label(first, "first here")
        .with_help(
            "a name is declared once in a scope, or several times each under a clause that \
             picks it (`resource pg_aws db { .. } where cloud == \"aws\"`); another name is \
             another statement, `resource C NAME { .. }`, `use PATH as NAME`",
        );
        self.diags.push(d);
        Err(Skip)
    }

    /// `use PATH [as NAME] [{ k = v }] [where B]` (R-65): a module imported
    /// once under NAME, its inputs the block's, its rules and denies run
    /// over what this scope sees, its items read as `NAME.x`; or a stack's
    /// deployments, read as `NAME[k=v].out`.
    fn use_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let (written, name) = use_parts(n);
        self.redeclared(n, &name)?;
        if provider_use(n, self.units, &self.decls.deployed).is_some() {
            return self.provider(n, scope, outer);
        }
        if let Some(rest) = written.strip_prefix("std.") {
            let fns = crate::functions::registry();
            if !fns.packages().contains(&rest) {
                return self.error(
                    span,
                    format!(
                        "no module `{written}` in the standard library: its modules are {}",
                        fns.packages().join(", ")
                    ),
                );
            }
            // `std` is used everywhere already.
            return Ok(Vec::new());
        }
        if self.decls.deployed.iter().any(|d| d.path == written) {
            if node(n, CLAUSE).is_some() || node(n, BLOCK).is_some() {
                return self.error(
                    span,
                    format!(
                        "`use {written}` binds a stack's deployments, which take no block and \
                         no clause: put the `where` on what reads them"
                    ),
                );
            }
            return Ok(Vec::new());
        }
        let path = self.module_path_of(scope, &written);
        match self.decls.modules.get(&path) {
            None => return self.error(span, format!("no module `{written}`")),
            Some(m) if m.component => {
                let d = Diagnostic::error(
                    span,
                    format!(
                        "{written} is a component, a type: make a resource of it, `resource \
                         {written} NAME {{ .. }}`"
                    ),
                )
                .with_note(
                    "`use` imports a module, a file, once under its name; a component, an item \
                     `component NAME { .. }`, is a type the program defines",
                );
                self.diags.push(d);
                return Err(Skip);
            }
            Some(_) => {}
        }
        // A module is stamped once under the name the `use` gives it, its
        // inputs the block's, else their defaults.
        match self.copy(n, scope, outer, path, name)? {
            Stmt::Instance(u) => Ok(vec![Stmt::Use(u)]),
            _ => unreachable!("a copy"),
        }
    }

    /// `resource PATH NAME { k = v } [where B]` of a component (R-113,
    /// R-65): one copy of the component, named NAME.
    fn instance(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let (written, name) = copy_parts(n);
        let module = match self.component_path(scope, &written) {
            Ok(m) => m,
            Err(_) if self.signature(scope, &written).is_some() => {
                let d = Diagnostic::error(
                    span,
                    format!("{written} is a component signature, which has no resources"),
                )
                .with_help(format!(
                    "make a resource of a component that has it, `component C: {written} {{ .. \
                     }}`, and pick one by a clause: `resource C {name} {{ .. }} where ..`"
                ));
                self.diags.push(d);
                return Err(Skip);
            }
            Err(d) => {
                self.diags.push(Diagnostic { span, ..*d });
                return Err(Skip);
            }
        };
        self.redeclared(n, &name)?;
        if let Some(t) = header_name(n).filter(|t| t.kind() == STRING && has_hole(t.text())) {
            return self.error(
                self.span_of(t.text_range()),
                format!(
                    "a resource of the component {written} is named statically: {} takes its \
                     name from the clause, which a copy cannot yet",
                    t.text()
                ),
            );
        }
        if tokens(n).any(|t| t.kind() == RANK) {
            return self.error(
                span,
                format!(
                    "a resource of the component {written} takes no rank: its own resources do"
                ),
            );
        }
        if name == "_" {
            return self.error(
                span,
                format!("a resource is written by its name: `resource {written} _` names nothing"),
            );
        }
        if self.is_value(scope, &name) && header_name(n).is_some_and(|t| t.kind() != STRING) {
            return self.error(
                span,
                format!(
                    "`{name}` is a value in scope, but a resource's header name is literal: this \
                     is the resource {name} of {written}; name it otherwise"
                ),
            );
        }
        self.copy(n, scope, outer, module, name).map(|c| vec![c])
    }

    /// One copy of the component `module`, named `name`, its inputs the
    /// block of `n` (an `instance` or a `use`) and its clause `n`'s.
    fn copy(
        &mut self,
        n: &SyntaxNode,
        scope: usize,
        outer: &Rc,
        module: String,
        name: String,
    ) -> L<Stmt> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, n)?;
        let clause = body.clone();
        let mut reads = Vec::new();
        let fields = match node(n, BLOCK) {
            Some(block) => self.fields(&mut rc, &block, &mut reads)?,
            None => Vec::new(),
        };
        body.extend(reads);
        let mut inputs = Vec::new();
        for f in fields {
            if matches!(f.op, FieldOp::Add) {
                return self.error(f.span, "an input of a copy is set with `=`, not `+=`");
            }
            if f.rank.is_some() {
                return self.error(f.span, "an input of a copy takes no rank");
            }
            inputs.push((f.key, f.value, f.span));
        }
        let values: Vec<&Term> = inputs.iter().map(|(_, v, _)| v).collect();
        self.check_bound(&rc, &body, &values)?;
        let rows = match node(n, BLOCK) {
            Some(block) => self.block_rows(&block, &module, scope, outer)?,
            None => Vec::new(),
        };
        Ok(Stmt::Instance(Instance {
            module,
            name,
            inputs,
            rows,
            body: (!body.is_empty()).then_some(body),
            clause: (!clause.is_empty()).then_some(clause),
            span,
        }))
    }

    /// The rows a `use` or `instance` block gives the relations its
    /// module takes, `input p` (R-55): `p(t, ..) [where B]`, a rule in
    /// this scope, and `p from FORMAT(..) [where B]`, a table, its columns
    /// the module's `decl p`. Each head is the module's own `p`, made the
    /// copy's by `modules`.
    fn block_rows(
        &mut self,
        block: &SyntaxNode,
        module: &str,
        scope: usize,
        outer: &Rc,
    ) -> L<Vec<Stmt>> {
        let Some(inner) = self.decls.modules.get(module).map(|m| m.scope) else {
            return Ok(Vec::new());
        };
        let takes = self.decls.scopes[inner].relation_inputs.clone();
        let mut out = Vec::new();
        let mut failed = false;
        for n in block
            .children()
            .filter(|c| matches!(c.kind(), RULE | FACT | INPUT_RELATION))
        {
            let span = self.span(&n);
            let pred = match n.kind() {
                INPUT_RELATION => word_text(&n, 0),
                _ => n
                    .children()
                    .find(|c| c.kind() == CALL)
                    .and_then(|c| self.callee(&c))
                    .unwrap_or_default(),
            };
            if !takes.contains(&pred) {
                let mut d = Diagnostic::error(span, format!("{module} takes no relation {pred}"));
                d = if takes.is_empty() {
                    d.with_note(format!("{module} declares no relation input, `input p`"))
                } else {
                    d.with_note(format!(
                        "the relations it takes: {}",
                        takes.iter().cloned().collect::<Vec<_>>().join(", ")
                    ))
                };
                self.diags.push(d);
                failed = true;
                continue;
            }
            let r = match n.kind() {
                INPUT_RELATION => (|| {
                    let Some(decl) = self.decls.scopes[inner].decl_nodes.get(&pred).cloned() else {
                        return self.error(
                            span,
                            format!(
                                "{pred} from ..: {module} declares no columns for {pred}, \
                                 `decl {pred}(a: T, ..)`"
                            ),
                        );
                    };
                    let cols = self.table_columns(&pred, &decl)?;
                    let mut rc = self.rc(&n, scope, outer);
                    let body = self.opt_body(&mut rc, &n)?;
                    self.table(&mut rc, &pred, cols, &n, body, span)
                })(),
                _ => self.rule(&n, scope, outer),
            };
            match r {
                Ok(stmts) => out.extend(stmts),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// `resource T n { f = t ... } where B`, `T` a provider's type or one
    /// the program declares.
    fn block_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let block = node(n, BLOCK);
        let header = self.header_token(n).ok_or(Skip)?;
        // Rows are a component's relation inputs (R-55); a type's
        // resource has attributes.
        if let Some(row) = block
            .iter()
            .flat_map(|b| b.children())
            .find(|c| matches!(c.kind(), RULE | FACT | INPUT_RELATION))
        {
            let typ = dotted_text(n, 1);
            return self.error(
                self.span(&row),
                format!(
                    "{typ} is no component, so its resource takes no rows: a row gives a \
                     relation a component takes (`input p`)"
                ),
            );
        }
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, n)?;
        let mut reads = Vec::new();
        let fields = match &block {
            Some(block) => self.wanting(Want::Schema, |l| l.fields(&mut rc, block, &mut reads))?,
            None => self.value_body(&mut rc, n, &mut reads)?,
        };
        let reads_at = body.len()..body.len() + reads.len();
        body.extend(reads);
        // The header: a string with holes is bound last, by `format`; a
        // bare name is the literal name, always (R-76). The name is one
        // segment of the address, quoted when it holds a dot (R-112).
        let name = if header.kind() == STRING && has_hole(header.text()) {
            let mut pre = Vec::new();
            let t = self.string_term(&mut rc, &header, &mut pre)?;
            body.extend(pre);
            let v = fresh(&mut rc, "Addr");
            body.push(Lit::Eq(var(&v), func(crate::ir::NAME_SEGMENT, vec![t])));
            var(&v)
        } else if header.kind() == STRING {
            str_term(&crate::ir::name_segment(&self.string(&header)?))
        } else {
            let text = header.text();
            let bound = bound_vars(&body);
            match rc.vars.get(text) {
                Some(v) if bound.contains(v) || rc.outer.contains(v) => {
                    let d = Diagnostic::error(
                        self.span_of(header.text_range()),
                        format!(
                            "`{text}` is bound by the clause, but a bare header name is the \
                             resource's literal name: a name from the clause is a string, \
                             `\"${{{text}}}\"`"
                        ),
                    )
                    .with_help(format!(
                        "write `resource {} \"${{{text}}}\" {{ .. }}` for the resource the \
                         clause names, or `\"{text}\"` quoted for the one named \"{text}\"",
                        dotted_text(n, 1)
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                _ if text == "_" => {
                    return self.error(
                        self.span_of(header.text_range()),
                        "a resource is written by its name: `_` names nothing; name it, or \
                         name it from the clause (`resource T \"${n}\" { .. } where p(n)`)",
                    );
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
        Ok(vec![Stmt::Resource(Resource {
            typ: str_term(&dotted_text(n, 1)),
            name,
            rank,
            fields,
            body,
            reads: reads_at,
            span,
        })])
    }

    /// `resource T N = VALUE` (R-126): the body is a value of the type, a
    /// document. An object written out is the block of its entries, one
    /// per key, checked as a block's are; any other value is one
    /// contribution at the root, an entry per key of the object it is when
    /// the rule runs (`transform::resource_to_stmts`).
    fn value_body(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        reads: &mut Vec<Lit>,
    ) -> L<Vec<FieldAssign>> {
        let t = terms(n).next().ok_or(Skip)?;
        let span = self.span(&t);
        let value = self.wanting(Want::Schema, |l| l.term(rc, &t, Pos::Value, reads))?;
        let entry = |key: &str, value: Term| FieldAssign {
            key: crate::ir::path_join("", key),
            op: FieldOp::Assign,
            value,
            rank: None,
            span,
        };
        Ok(match value {
            Term::Obj(m) => m.into_iter().map(|(k, v)| entry(&k, v)).collect(),
            Term::Val(Value::Obj(m)) => m
                .into_iter()
                .map(|(k, v)| entry(&k, Term::Val(v)))
                .collect(),
            Term::Val(_) | Term::List(_) => {
                return self.error(
                    span,
                    format!(
                        "the body of a resource is a value of its type, an object: not `{}`",
                        t.text().to_string().trim()
                    ),
                );
            }
            value => vec![FieldAssign {
                key: String::new(),
                op: FieldOp::Assign,
                value,
                rank: None,
                span,
            }],
        })
    }

    /// `set from DOC [@rank] [where B]` (R-38): every leaf of the document
    /// a contribution to the input at its path, `arg(input, "", P, V, Rank)
    /// :- B, reads, table.FORMAT.set(Path, At, P, V)`, which
    /// `tables::expand_set_from` makes one rule per input path, and a deny
    /// for a leaf at a path that is no input's.
    fn set_from(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let src = terms(n).next().ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let rank = self.rank_tok(n)?.unwrap_or(Rank::Normal);
        let (path, value) = (var(&fresh(&mut rc, "Path")), var(&fresh(&mut rc, "Value")));
        let cols = [("path", "string"), ("value", "any")]
            .map(|(name, ty)| BindArg {
                input: false,
                name: name.into(),
                ty: Some(TypeExpr::Name(ty.into())),
            })
            .to_vec();
        let _ = src;
        let mut out = self.doc_source(
            &mut rc,
            n,
            crate::tables::SET_DOC,
            cols,
            vec![path.clone(), value.clone()],
            &mut body,
        )?;
        self.check_bound(&rc, &body, &[])?;
        out.push(Stmt::Rule(RuleStmt {
            head: atom_at(
                "arg",
                vec![
                    str_term(crate::modules::INPUT),
                    str_term(""),
                    path,
                    value,
                    str_term(rank.name()),
                ],
                span,
            ),
            body,
        }));
        Ok(out)
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
            _ => Calls::Function,
        };
        // Another copy's or deployment's relation is read, never written.
        if let Some(c) = head_node.children().find_map(|c| Chain::of(&c))
            && !c.ops.is_empty()
            && (self.instance_in(scope, &c.head).is_some()
                || self.stack_in(scope, &c.head).is_some()
                || matches!(c.ops.first(), Some(Op::Index(..) | Op::Keyed(..))))
        {
            return self.error(
                span,
                format!(
                    "`{}` is another copy's relation, read in a body; its rows are its own",
                    c.fields().join(".")
                ),
            );
        }
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

    /// `let k = t [@rank] [where B]` (H-6, R-3): a contribution to the cell
    /// `k`, written `let(k, t, rank)` for `modules` to scope (the cell is
    /// `(let, SCOPE, k)`, read by name as `k(V)`). When `t` is a reference,
    /// `k`'s value is that reference and a dot on `k` reads through it.
    fn let_stmt(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Err(e) = self.value_type(scope, &name) {
            return self.error(span, e);
        }
        // `let NAME: T = t` (R-74): its rows agree on `T`, and a literal is
        // read as one, as in any typed position (R-31).
        let declared = node(n, TYPE_EXPR).map(|t| self.type_expr(&t));
        let typed_rows: Vec<SyntaxNode> = self
            .find_let(scope, &name)
            .map(|(_, rows)| rows)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.parent().and_then(|l| node(&l, TYPE_EXPR)))
            .collect();
        if let (Some(here), Some(first)) = (node(n, TYPE_EXPR), typed_rows.first())
            && here.text() != first.text()
        {
            return self.error(
                self.span(&here),
                format!(
                    "`let {name}` is declared `{}` in one row and `{}` in another",
                    first.text(),
                    here.text()
                ),
            );
        }
        let ty = declared.as_ref().map(crate::types::of_expr);
        let want = match &ty {
            Some(crate::types::Ty::Ref(t)) => Want::Type(t.clone()),
            _ => Want::Nothing,
        };
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let t = terms(n).next().ok_or(Skip)?;
        let value = if t.kind() == CALL && self.aggregate_name(&t).is_some() {
            // `let n = count(x) where B`: one group (R-59).
            let call = self.aggregate_call(&mut rc, &t, &mut body)?;
            let v = fresh(&mut rc, &capitalise(&name));
            self.aggs.push(aggregate::Agg {
                var: v.clone(),
                text: t.text().to_string(),
                span: self.span(&t),
            });
            body.push(Lit::Eq(var(&v), call));
            var(&v)
        } else {
            self.wanting(want, |l| l.let_value(&mut rc, &t, &mut body))?
        };
        let value = match &ty {
            Some(ty) => self.let_typed(scope, &name, ty, &t, value)?,
            None => value,
        };
        let rank = self.rank_tok(n)?.unwrap_or(Rank::Normal);
        let head = Atom {
            pred: crate::modules::LET.to_string(),
            args: vec![str_term(&name), value, str_term(rank.name())],
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        let mut out = vec![if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        }];
        // Declared secret (R-153): the cell is, each path of it that is
        // (`modules::lets` scopes it).
        for (q, _) in declared.iter().flat_map(crate::types::secret_fields) {
            out.push(Stmt::Fact(Atom {
                pred: crate::modules::SECRET_LET.into(),
                args: vec![str_term(&name), str_term(&q)],
                record: None,
                span,
            }));
        }
        // The cell's type is its reader's column, `decl NAME(NAME: T)`, as
        // R-34 types a relation: once, at the first typed row. A resource
        // type is the reference's own (`value_type`).
        if let (Some(d), Some(ty)) = (declared, &ty)
            && !matches!(ty, crate::types::Ty::Ref(_))
            && typed_rows.first().and_then(|t| t.parent()).as_ref() == Some(n)
        {
            out.push(Stmt::Decl(Decl {
                pred: name.clone(),
                fields: vec![name],
                types: vec![Some(d)],
                span,
            }));
        }
        Ok(out)
    }

    /// A typed `let`'s value `value` (lowered from `t`) checked against
    /// its type `ty`, a literal read as one (R-31, R-74).
    fn let_typed(
        &mut self,
        scope: usize,
        name: &str,
        ty: &crate::types::Ty,
        t: &SyntaxNode,
        value: Term,
    ) -> L<Term> {
        use crate::types::Ty;
        let span = self.span(t);
        // A reference: its type is the row's, read from its text.
        let got = match Chain::of(t) {
            Some(c) if c.is_bare() && !self.is_value(scope, &c.head) => {
                self.resource(scope, &c.head).map(|types| match ty {
                    Ty::Ref(want) if types.contains(want) => want.clone(),
                    _ => types[0].clone(),
                })
            }
            _ => match self.written_vtype(scope, t, 0) {
                Some(VType::Ref(typ)) => Some(typ),
                _ => None,
            },
        };
        match got {
            Some(typ) => {
                let r = func(
                    crate::ir::REF,
                    vec![str_term(&typ), value.clone(), str_term("")],
                );
                match crate::types::mismatch(ty, &r) {
                    Some(why) => self.error(span, format!("let {name} {why}")),
                    None => Ok(value),
                }
            }
            None => crate::types::literal(ty, value)
                .or_else(|why| self.error(span, format!("let {name} {why}"))),
        }
    }

    /// A `let`'s value: a reference is its key (a resource's address, a
    /// live object's name); anything else the term.
    fn let_value(&mut self, rc: &mut Rc, t: &SyntaxNode, body: &mut Vec<Lit>) -> L<Term> {
        if let Some(c) = Chain::of(t) {
            // A resource by its bare name.
            if c.is_bare()
                && !rc.vars.contains_key(&c.head)
                && !self.is_value(rc.scope, &c.head)
                && self.resource(rc.scope, &c.head).is_some()
            {
                let span = self.span(t);
                return Ok(self.reference(rc, &c, body, span)?.1);
            }
            let mut pre = Vec::new();
            let mut rc2 = rc.clone();
            if let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) {
                let key = match &res {
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
        self.term(rc, t, Pos::Value, body)
    }

    /// `set chain (=|+=) t [@rank] [where B]` (H-5): a contribution to a
    /// resource's attribute (`arg(T, A, p, t)`) or an input (the stack's
    /// own, a field of an object one, a used module's, a copy's), normal
    /// unless ranked (R-38). `set { chain = t .. } [@rank] [where B]` is
    /// several under one clause and rank; `set from DOC [@rank] [where B]`
    /// a document's leaves, each to the input at its path.
    fn set(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        if tokens(n).nth(1).is_some_and(|t| t.text() == "from")
            && !tokens(n).any(|t| matches!(t.kind(), EQ | PLUS_EQ))
        {
            return self.set_from(n, scope, outer);
        }
        let mut rc = self.rc(n, scope, outer);
        let body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let rank = self.rank_tok(n)?;
        let Some(block) = node(n, BLOCK) else {
            let lhs = n.children().find(|c| c.kind() == CHAIN).ok_or(Skip)?;
            let rhs = terms(n).find(|t| *t != lhs).ok_or(Skip)?;
            let add = tokens(n).any(|t| t.kind() == PLUS_EQ);
            let span = self.span(n);
            let st = self.contribution(&mut rc, &lhs, &rhs, add, rank, body, has_body, span)?;
            return Ok(vec![st]);
        };
        let mut out = Vec::new();
        let mut failed = false;
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let r = (|| {
                let lhs = a.children().find(|c| c.kind() == CHAIN).ok_or(Skip)?;
                let rhs = terms(&a).find(|t| *t != lhs).ok_or(Skip)?;
                let add = tokens(&a).any(|t| t.kind() == PLUS_EQ);
                let rank = self.rank_tok(&a)?.or(rank);
                let span = self.span(&a);
                let mut rc = rc.clone();
                self.contribution(&mut rc, &lhs, &rhs, add, rank, body.clone(), has_body, span)
            })();
            match r {
                Ok(st) => out.push(st),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// One contribution of a `set`: `lhs (=|+=) rhs` under the clause
    /// `body` (`has_body`: one is written).
    #[allow(clippy::too_many_arguments)]
    fn contribution(
        &mut self,
        rc: &mut Rc,
        lhs: &SyntaxNode,
        rhs: &SyntaxNode,
        add: bool,
        rank: Option<Rank>,
        mut body: Vec<Lit>,
        has_body: bool,
        span: Span,
    ) -> L<Stmt> {
        let c = Chain::of(lhs).ok_or(Skip)?;
        if add && rank.is_some() {
            return self.error(span, "a rank applies to `=`, not `+=`");
        }
        let (typ, addr, path, block) = match self.set_target(rc, &c, &mut body, span)? {
            Target::Cell(typ, addr, path, block) => (typ, addr, path, block),
            Target::Element(typ, addr, list, key, rest, block) => {
                if add {
                    return self.error(
                        span,
                        "`+=` adds to a list or a set; an element of a keyed list is written \
                         with `=`",
                    );
                }
                if let Some(block) = block
                    && !has_body
                {
                    let d = Diagnostic::error(
                        span,
                        format!(
                            "`set {}` with no condition is an entry of `{block}`, declared in \
                             the same scope",
                            lhs.text()
                        ),
                    )
                    .with_help(format!("write the element in the block's `{list}`"));
                    self.diags.push(d);
                    return Err(Skip);
                }
                let value =
                    self.wanting(Want::Schema, |l| l.term(rc, rhs, Pos::Value, &mut body))?;
                let head = atom_at(
                    "arg",
                    vec![
                        typ,
                        addr,
                        str_term(&list),
                        element_write(key, &rest, value),
                        str_term(rank.unwrap_or(Rank::Normal).name()),
                    ],
                    span,
                );
                self.check_bound(rc, &body, &atom_terms(&head))?;
                return Ok(if body.is_empty() && !has_body {
                    Stmt::Fact(head)
                } else {
                    Stmt::Rule(RuleStmt { head, body })
                });
            }
            Target::Input(k) => {
                // A stack input: `input(k, t)`, as `--set k=t` gives it.
                if !has_body {
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
                if add {
                    return self.error(span, "an input is set with `=`");
                }
                // A contribution to the input's cell, normal unless ranked
                // (R-38); written to the cell itself, so that its condition
                // may read another input.
                let value = self.term(rc, rhs, Pos::Value, &mut body)?;
                let head = atom_at(
                    "arg",
                    vec![
                        str_term(crate::modules::INPUT),
                        str_term(""),
                        str_term(&k),
                        value,
                        str_term(rank.unwrap_or(Rank::Normal).name()),
                    ],
                    span,
                );
                self.check_bound(rc, &body, &atom_terms(&head))?;
                return Ok(if body.is_empty() && !has_body {
                    Stmt::Fact(head)
                } else {
                    Stmt::Rule(RuleStmt { head, body })
                });
            }
        };
        if let Some(block) = block
            && !has_body
        {
            let d = Diagnostic::error(
                self.span(lhs),
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
        let value = self.wanting(Want::Schema, |l| l.term(rc, rhs, Pos::Value, &mut body))?;
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
        self.check_bound(rc, &body, &atom_terms(&head))?;
        Ok(if body.is_empty() && !has_body {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        })
    }

    /// What a `set` sets: a cell `(T, A, path)` and, when the block that
    /// owns the cell is declared in the same scope, how that block is
    /// written; or a stack input.
    fn set_target(&mut self, rc: &mut Rc, c: &Chain, body: &mut Vec<Lit>, span: Span) -> L<Target> {
        let scope = rc.scope;
        // `[_]`: a variable per step, the clause form (R-162).
        let each;
        let c = if c.ops.iter().any(each::is_each) {
            each = self.each_target(rc, c, body, span)?;
            &each
        } else {
            c
        };
        // A stack input, set by name, or a field of an object input by its
        // path (R-54); a field of an input that is a reference is the
        // referenced resource's attribute (`set role.policies` in a module
        // given `input role: iam.role`), as a `let` of one's is.
        let through_ref = !c.ops.is_empty() && self.input_ref(scope, &c.head).is_some();
        if c.ops.iter().all(|o| matches!(o, Op::Field(_)))
            && self.is_value(scope, &c.head)
            && self.find_let(scope, &c.head).is_none()
            && !through_ref
        {
            return Ok(Target::Input(c.fields().join(".")));
        }
        // A used module's input, `m.k` or a field of an object one,
        // `m.k.f` (R-55): its cell `(input, m, k)`.
        if !c.ops.is_empty()
            && c.ops.iter().all(|o| matches!(o, Op::Field(_)))
            && let Some(at) = self
                .chain_of(scope)
                .into_iter()
                .find(|&s| self.decls.scopes[s].uses.contains_key(&c.head))
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                str_term(crate::modules::INPUT),
                self.scope_term(scope, at, str_term(&c.head)),
                c.fields()[1..].join("."),
                own.then(|| format!("use {}", c.head)),
            ));
        }
        // An element of a keyed list a variable ranges over: `set c.p = v
        // where c in w.containers` writes `w.containers[c].p` (R-69).
        if let Some((typ, addr, list)) = rc.elems.get(&c.head).cloned()
            && (!c.ops.is_empty() || c.head.starts_with("_[_]"))
        {
            let mut rest = Vec::new();
            for op in &c.ops {
                match op {
                    Op::Field(f) => rest.push(f.clone()),
                    Op::Index(_, r) | Op::Keyed(_, r) => {
                        return self.error(
                            self.span_of(*r),
                            format!(
                                "`{}` is an element of `{list}`: what `set` writes in it is a \
                                 path of fields, `{}.p.q`",
                                c.head, c.head
                            ),
                        );
                    }
                }
            }
            let key = var(&self.var_named(rc, &c.head, span));
            let block = self.owning_block(scope, &typ, &addr);
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        // An instance's input: `n.k`.
        if let [Op::Field(k)] = c.ops.as_slice()
            && let Some((at, path)) = self.instance_in(scope, &c.head)
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                str_term(crate::modules::INPUT),
                self.scope_term(scope, at, str_term(&c.head)),
                k.clone(),
                own.then(|| format!("resource {path} {}", c.head)),
            ));
        }
        let mut pre = Vec::new();
        let res = self.resolve(rc, c, &mut pre)?;
        body.extend(pre);
        let (typ, addr, path) = match res {
            Res::Ref { typ, addr, path } if !path.is_empty() => (typ, addr, path),
            _ => {
                return self.error(
                    span,
                    "`set` sets a resource's attribute (`r.tags`, `T[e].p`) or an input",
                );
            }
        };
        // `containers[k]`: the element of a keyed list whose key is `k`, any
        // term but an integer, which stays the position (R-35, R-69).
        let keyed = path.iter().position(|s| match s {
            Seg::I(t) => !matches!(t, Term::Val(Value::Int(_))),
            Seg::K(_) => true,
            Seg::F(_) => false,
        });
        if let Some(i) = keyed
            && !matches!(&typ, Term::Val(Value::Str(t)) if t == "settings")
        {
            let (Seg::I(key) | Seg::K(key)) = path[i].clone() else {
                unreachable!("found above")
            };
            let Some(list) = path_string(&path[..i]).filter(|l| !l.is_empty()) else {
                return self.error(span, "a contribution's path is constant");
            };
            let mut rest = Vec::new();
            for s in &path[i + 1..] {
                match s {
                    Seg::F(f) => rest.push(f.clone()),
                    Seg::I(_) | Seg::K(_) => {
                        return self.error(
                            span,
                            format!(
                                "one index per written path: below the element of `{list}` \
                                 the path is fields"
                            ),
                        );
                    }
                }
            }
            let block = self.owning_block(scope, &typ, &addr);
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        let Some(path) = path_string(&path) else {
            return self.error(span, "a contribution's path is constant");
        };
        let block = self.owning_block(scope, &typ, &addr);
        Ok(Target::Cell(typ, addr, path, block))
    }

    /// The block that owns the cells of `(T, A)` when it is declared in
    /// `scope`: `resource T A`.
    fn owning_block(&self, scope: usize, typ: &Term, addr: &Term) -> Option<String> {
        let s = &self.decls.scopes[self.decl_scope(scope)];
        match (typ, addr) {
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) => s
                .resources
                .get(a)
                .is_some_and(|ts| ts.contains(t))
                .then(|| format!("resource {t} {a}")),
            _ => None,
        }
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
                    crate::modules::INPUT if s(1).as_deref() == Some("") => p.clone(),
                    crate::modules::INPUT => format!("{}.{p}", s(1).unwrap_or_default()),
                    _ => format!("{t}[{}]{}", a[1], crate::ir::path_suffix(&p)),
                };
                Some(format!("`set {target} {op} {}{cond}`", a[3]))
            }
            ("output", 2) if s(0).is_some() => {
                Some(format!("`output {} = {}{cond}`", s(0).unwrap(), a[1]))
            }
            ("input", 2) if s(0).is_some() && has_body => {
                Some(format!("`set {} = {}{cond}`", s(0).unwrap(), a[1]))
            }
            ("input", 2) if s(0).is_some() => Some(format!(
                "a default, `input {} .. = {}`, or `--set {}=..`",
                s(0).unwrap(),
                a[1],
                s(0).unwrap()
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
        // A relation read through a copy or a deployment (R-55) is the
        // compiler's lowering, not a core relation written.
        if self.callee(n).as_deref() != Some(atom.pred.as_str()) {
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
        let unbound = self.unbound_aggregates(rc, body);
        let mut failed = !unbound.is_empty();
        let mut bound = bound_vars(body);
        // A comprehension binds its own variables, wherever it stands.
        let holder: Vec<Lit> = heads
            .iter()
            .map(|t| Lit::Neq((*t).clone(), (*t).clone()))
            .collect();
        bound.extend(bound_vars(&holder));
        bound.extend(rc.outer.iter().cloned());
        for (src, low) in &rc.vars {
            if rc.outer.contains(low)
                || (bound.contains(low) && rc.binders.contains(src))
                || unbound.contains(low)
            {
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
        if failed {
            Err(Skip)
        } else {
            self.singletons(rc)
        }
    }

    // --- bodies -------------------------------------------------------------

    fn body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        let lits: Vec<SyntaxNode> = n.children().collect();
        self.lits(rc, &lits)
    }

    /// A statement's body (its clauses together), checked for what binds
    /// and lowered in the order its literals hold (R-10): each after what
    /// binds what it reads, otherwise as written. A body nested in it is
    /// checked with it.
    fn lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<Vec<Lit>> {
        if self.nested > 0 {
            return self.lits_as_written(rc, lits);
        }
        let saved = (
            rc.clone(),
            self.helpers.len(),
            self.negs,
            self.aggs.len(),
            self.agg_rules,
        );
        let outer = rc.outer.clone();
        let out = self.lits_as_written(rc, lits)?;
        let order = self.check_order(rc, lits, &outer_names(rc, &outer))?;
        if order.iter().enumerate().all(|(i, &j)| i == j) {
            return Ok(out);
        }
        let (rc0, helpers, negs, aggs, agg_rules) = saved;
        *rc = rc0;
        self.helpers.truncate(helpers);
        self.negs = negs;
        self.aggs.truncate(aggs);
        self.agg_rules = agg_rules;
        let lits: Vec<SyntaxNode> = order.iter().map(|&i| lits[i].clone()).collect();
        self.lits_as_written(rc, &lits)
    }

    fn lits_as_written(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<Vec<Lit>> {
        let mut out = Vec::new();
        let mut failed = false;
        for l in lits {
            if self.lit(rc, l, &mut out).is_err() {
                failed = true;
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// One literal, with the reads it hoists before it, into `out`.
    fn lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let saved = std::mem::take(&mut self.after);
        let r = self.bind(true, |l| l.lit1(rc, n, out));
        out.append(&mut self.after);
        self.after = saved;
        r
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
                let c = terms(n).next().and_then(|t| Chain::read(&t)).ok_or(Skip)?;
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
                let t = terms(n).next().ok_or(Skip)?;
                let c = Chain::read(&t).ok_or(Skip)?;
                if self.bare_resource(rc, &c) {
                    let (typ, addr) = self.reference(rc, &c, out, span)?;
                    out.push(Lit::Pos(atom_at(IDENTITY, vec![typ, addr], span)));
                    return Ok(());
                }
                let res = self.resolve(rc, &c, out)?;
                match &res {
                    Res::Ref { typ, addr, path } if path.is_empty() => {
                        let a = atom_at(IDENTITY, vec![typ.clone(), addr.clone()], span);
                        out.push(Lit::Pos(a));
                        return Ok(());
                    }
                    Res::Ref { path, .. } if is_identity(path) => {
                        return self.has_identity(&t, span);
                    }
                    _ => {}
                }
                let start = out.len();
                let marked = has_atom(&res, span);
                match self.read_atom(rc, &res, Term::Wildcard, span) {
                    Some(a) => out.push(Lit::Pos(a)),
                    // A nested path, or a field of a value: it has a value
                    // when the walk to it does.
                    None if matches!(
                        res,
                        Res::Ref { .. } | Res::Var { .. } | Res::Value { .. }
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
                            "`has` takes an attribute of a resource (`has r.p`), a value name \
                             or a field of a value",
                        );
                    }
                }
                mark_has(out, start, marked, false);
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
            LIT_IN if !n.descendants().any(|d| d.kind() == TUPLE) => {
                let lit = self.membership(rc, n, out)?;
                out.push(negate(lit));
                return Ok(());
            }
            // `not has r` of a resource: a helper over its identity.
            LIT_HAS
                if terms(n)
                    .next()
                    .and_then(|t| Chain::read(&t))
                    .is_some_and(|c| self.bare_resource(rc, &c)) => {}
            LIT_TRUTH | LIT_HAS => {
                let t = terms(n).next().ok_or(Skip)?;
                let c = Chain::read(&t).ok_or(Skip)?;
                let value = if n.kind() == LIT_HAS {
                    Term::Wildcard
                } else {
                    Term::Val(Value::Bool(true))
                };
                let mut pre = Vec::new();
                let res = self.resolve(rc, &c, &mut pre)?;
                match &res {
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && path.is_empty() => {
                        return self.neg_helper(rc, n, out, span);
                    }
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && is_identity(path) => {
                        return self.has_identity(&t, span);
                    }
                    _ => {}
                }
                let marked = (n.kind() == LIT_HAS)
                    .then(|| has_atom(&res, span))
                    .flatten();
                if let Some(a) = self.read_atom(rc, &res, value, span) {
                    out.extend(pre);
                    let start = out.len();
                    out.push(Lit::Not(a));
                    mark_has(out, start, marked, true);
                    return Ok(());
                }
                if marked.is_some() {
                    let start = out.len();
                    self.neg_helper(rc, n, out, span)?;
                    mark_has(out, start, marked, true);
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
        self.nested += 1;
        let lowered = if n.kind() == BODY {
            self.body(&mut rc2, n).map(|b| inner = b)
        } else {
            self.lit(&mut rc2, n, &mut inner)
        };
        self.nested -= 1;
        lowered?;
        // Keep the variable table: names the helper introduced are its own.
        for (k, v) in &rc2.vars {
            rc.vars.entry(k.clone()).or_insert_with(|| v.clone());
        }
        rc.reserved.extend(rc2.reserved.iter().cloned());
        // The body is written once in the rule: its names count there.
        rc.uses = rc2.uses;
        for (k, at) in rc2.first {
            rc.first.entry(k).or_insert(at);
        }
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
        // An aggregate's value is not the helper's: it is folded after.
        let results: BTreeSet<&String> = self.aggs.iter().map(|a| &a.var).collect();
        let reads_result = |l: &Lit| {
            let mut vs = BTreeSet::new();
            lit_vars(l, &mut vs);
            vs.iter().any(|v| results.contains(v))
        };
        if inner.iter().any(reads_result) {
            return self.error(
                span,
                "an aggregate's value is compared after the fold, not inside `not { }`",
            );
        }
        let mut body: Vec<Lit> = out
            .iter()
            .filter(|l| !matches!(l, Lit::Not(_)) && !reads_result(l))
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
        if let Some(r) = self.aggregate_binding(rc, n, out) {
            return r;
        }
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
        // `P = e` with a tuple or object pattern on the left (R-58); an
        // element `P = e[i]` (or `e[i] = P`) is `member(e, i, P)` (H-9).
        let pattern = |t: &SyntaxNode| Self::is_pattern(t) || t.kind() == LIST;
        if ts.len() == 2 && ops.as_slice() == [EQ] && (pattern(&ts[0]) || pattern(&ts[1])) {
            let (p, c) = match pattern(&ts[0]) {
                true => (&ts[0], &ts[1]),
                false => (&ts[1], &ts[0]),
            };
            let element = Chain::of(c)
                .filter(|ch| matches!(ch.ops.last(), Some(Op::Index(ix, _)) if ix.len() == 1));
            if let Some(ch) = element
                && let Some(Op::Index(ix, _)) = ch.ops.last()
            {
                let ix = ix[0].clone();
                let mut list = ch.clone();
                list.ops.pop();
                let res = self.resolve(rc, &list, out)?;
                let l = self.realize(rc, res, Pos::Content, out, span)?;
                let i = self.bind(true, |x| x.term(rc, &ix, Pos::Content, out))?;
                let pat = self.pattern(rc, p, out)?;
                out.push(Lit::Pos(atom_at("member", vec![l, i, pat], span)));
                return Ok(());
            }
            if pattern(&ts[0]) {
                return self.pattern_eq(rc, p, c, out);
            }
        }
        // An input that is a reference (`input role: iam.role`) compares
        // as the resource it names, not as its relation's row: `r == role`
        // compares addresses.
        let refs = ts.len() == 2
            && ts.iter().any(|t| {
                Chain::of(t).is_some_and(|c| {
                    c.is_bare()
                        && !rc.vars.contains_key(&c.head)
                        && self.input_ref(rc.scope, &c.head).is_some()
                })
            });
        if ts.len() == 2 && matches!(ops.as_slice(), [EQ | EQ2]) && !refs {
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
                if let Res::Ref { path, .. } = &res
                    && matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
                {
                    return self.identity_read(r, self.span(r));
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
        // `r == "main"`: a reference is never a string (R-31).
        if ts.len() == 2 && matches!(ops.as_slice(), [EQ2 | NEQ]) {
            for (r, s) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                if s.kind() == LITERAL
                    && tokens(s).any(|t| t.kind() == STRING)
                    && self.is_reference(rc, r)
                {
                    let text = r.text().to_string();
                    let lit = s.text().to_string();
                    let d = Diagnostic::error(
                        span,
                        format!("`{text}` is a reference and {lit} a string: they are never equal"),
                    )
                    .with_help(format!(
                        "compare with the resource: `{text} {op} {}`, or `{text} {op} T[{lit}]` \
                         (R-31)",
                        lit.trim_matches('"'),
                        op = if ops[0] == NEQ { "!=" } else { "==" },
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
            }
        }
        // `r == main`, `r != T[e]`: a resource compares as a reference, and
        // so does the other side (R-42).
        if ts.len() == 2
            && matches!(ops.as_slice(), [EQ | EQ2 | NEQ])
            && (refs || ts.iter().any(|t| self.names_resource(rc, t)))
        {
            let mut lowered = Vec::new();
            for (i, t) in ts.iter().enumerate() {
                let binding = i == 0 && ops[0] == EQ;
                lowered.push(self.bind(binding, |l| l.ref_term(rc, t, Pos::Content, out))?);
            }
            let (a, b) = (lowered[0].clone(), lowered[1].clone());
            out.push(if ops[0] == NEQ {
                Lit::Neq(a, b)
            } else {
                Lit::Eq(a, b)
            });
            return Ok(());
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
            let sign = match op {
                EQ => "=",
                EQ2 => "==",
                NEQ => "!=",
                LT => "<",
                LE => "<=",
                GT => ">",
                _ => ">=",
            };
            let (a, b) = match crate::types::operands(sign, a, b) {
                Ok(ab) => ab,
                Err(why) => return self.error(span, why),
            };
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
        let rhs = ts.get(1).and_then(Chain::of).map(|c| self.type_each(rc, c));
        // `v in PATH` with a `[_]` in it binds `v` to each value the path
        // reaches (R-162): the steps enumerate.
        if let (Some(c), Some(rhs_node)) = (&rhs, ts.get(1))
            && c.ops.iter().any(each::is_each)
        {
            if lhs_node.kind() == TUPLE {
                return self.error(
                    span,
                    format!(
                        "`{}` binds each value of a path with `[_]`, one name: `v in {}`",
                        lhs_node.text(),
                        rhs_node.text()
                    ),
                );
            }
            let value = self.bind(false, |l| l.term(rc, rhs_node, Pos::Content, out))?;
            let item = self.term(rc, lhs_node, Pos::Content, out)?;
            return Ok(Lit::Eq(item, value));
        }
        if !any_type && let (Some(c), Some(rhs_node)) = (&rhs, ts.get(1)) {
            // `x in T`, `T` an enum type (R-70): each of its values, in
            // order, as a range is enumerated.
            if let Some(e) = self.enum_type(rc, c, rhs_node)? {
                return self.enum_member(rc, lhs_node, e, out, span);
            }
            // `r in NS`, a provider's namespace (R-49): a resource of any
            // of its types.
            if let Some(ns) = self.namespace_of(rc, c) {
                return self.namespace_member(rc, lhs_node, &ns, out, span);
            }
            // `x in network`, a component (R-67): each of its copies;
            // `blue in network`, a copy in scope by its name (R-113), is
            // the test that it is one of them.
            if let Some(path) = self.component_of(rc, c) {
                let copy = Chain::of(lhs_node)
                    .filter(|l| l.is_bare() && !rc.vars.contains_key(&l.head))
                    .filter(|l| self.instance_in(rc.scope, &l.head).is_some())
                    .map(|l| str_term(&l.head));
                let x = match copy {
                    Some(name) => name,
                    None => self.term(rc, lhs_node, Pos::Content, out)?,
                };
                let parent = self.copies_scope(rc, &path);
                return Ok(Lit::Pos(atom_at(
                    crate::modules::INSTANCE_OF,
                    vec![str_term(&path), parent, x],
                    span,
                )));
            }
        }
        let mut typ = if any_type {
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
        // `x in ovh.instance` with the provider also used as `ca` (R-115):
        // a resource of `ovh.instance` or `ca.instance`, `__provider_type(
        // "ovh.instance", Type), want(Type, x)`.
        let mut renamed = None;
        if let Some(Term::Val(Value::Str(t))) = typ.clone()
            && world.is_none()
            && self.covering(&t).len() > 1
        {
            let lhs = Chain::of(lhs_node).filter(Chain::is_bare).map(|c| c.head);
            let tv = match lhs.as_ref().and_then(|h| rc.types.get(h)) {
                Some(v @ Term::Var(_)) => v.clone(),
                _ => var(&fresh(rc, "Type")),
            };
            for each in self.covering(&t) {
                self.helpers.push(Stmt::Fact(atom_at(
                    membership::PROVIDER_TYPE,
                    vec![str_term(&t), str_term(&each)],
                    span,
                )));
            }
            renamed = Some(Lit::Pos(atom_at(
                membership::PROVIDER_TYPE,
                vec![str_term(&t), tv.clone()],
                span,
            )));
            typ = Some(tv);
        }
        if (typ.is_some() || world.is_some()) && lhs_node.kind() == TUPLE {
            return self.error(
                span,
                format!(
                    "`{}` is a pattern: a resource is enumerated by a name, `r in {}`",
                    lhs_node.text(),
                    ts.get(1).map(|t| t.text().to_string()).unwrap_or_default()
                ),
            );
        }
        if typ.is_some() || world.is_some() {
            // The left side: an element, bound or checked; a bare resource
            // name is its address; a computed name is bound first.
            let lhs = match Chain::of(lhs_node) {
                Some(c) if c.is_bare() && self.resource(rc.scope, &c.head).is_some() => {
                    // The type on the right picks among resources of one name.
                    let named = self.resource(rc.scope, &c.head).unwrap_or_default();
                    match &typ {
                        Some(Term::Val(Value::Str(t))) if named.contains(t) => {
                            self.resource_addr(rc.scope, &c.head)
                        }
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
            // `r in T` with `r` a reference column's (`deformation(k, r,
            // _)`): the column's `ref(T, R, "")` tests the type, so a row
            // of a deleted resource passes (R-42).
            if let (Some(t), Some(c)) = (&typ, Chain::of(lhs_node))
                && world.is_none()
                && c.is_bare()
                && Self::ref_bound(n, &c.head)
            {
                if let Some(test) = renamed {
                    return Ok(test);
                }
                let have = rc.types.get(&c.head).cloned().unwrap_or_else(|| t.clone());
                return Ok(Lit::Eq(have, t.clone()));
            }
            if let Some(w) = world {
                let typ = w.fields()[1..].join(".");
                return Ok(Lit::Pos(atom_at(
                    "cloud_exists",
                    vec![str_term(&typ), lhs],
                    span,
                )));
            }
            self.address_key(&lhs, span)?;
            out.extend(renamed);
            return Ok(Lit::Pos(atom_at("want", vec![typ.unwrap(), lhs], span)));
        }
        let rhs = ts.get(1).ok_or(Skip)?;
        let of = self.resource_list(rc, rhs);
        let list = if rhs.kind() == RANGE {
            self.range(rc, rhs, out)?
        } else {
            self.bind(false, |l| l.term(rc, rhs, Pos::Content, out))?
        };
        // The element's name, for `set c.p` (R-69): `c in L`, `(i, c) in L`.
        let elem = match lhs_node.kind() {
            TUPLE => terms(lhs_node).nth(1),
            _ => Some(lhs_node.clone()),
        };
        if let (Some(of), Some(c)) = (of, elem.as_ref().and_then(Chain::of))
            && c.is_bare()
        {
            rc.elems.insert(c.head, of);
        }
        if lhs_node.kind() == TUPLE {
            return self.pattern_in(rc, lhs_node, list, out, span);
        }
        let item = self.term(rc, lhs_node, Pos::Content, out)?;
        Ok(Lit::Pos(atom_at("member", vec![list, item], span)))
    }

    /// The resource attribute `t` names, `(T, A, path)`, when it is one
    /// at a constant path (`w.spec.template.spec.containers`). Resolved on
    /// the side: the caller lowers `t` itself.
    fn resource_list(&mut self, rc: &Rc, t: &SyntaxNode) -> Option<(Term, Term, String)> {
        let c = Chain::of(t).filter(|c| !c.is_bare())?;
        let (diags, helpers, negs) = (self.diags.len(), self.helpers.len(), self.negs);
        let mut rc = rc.clone();
        let res = self.probe(|l| l.resolve(&mut rc, &c, &mut Vec::new()));
        self.diags.truncate(diags);
        self.helpers.truncate(helpers);
        self.negs = negs;
        match res {
            Ok(Res::Ref { typ, addr, path }) => path_string(&path)
                .filter(|p| !p.is_empty())
                .map(|p| (typ, addr, p)),
            _ => None,
        }
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
            let typ = match &self.want {
                _ if types.len() == 1 => &types[0],
                Want::Type(t) if types.contains(t) => t,
                _ => return self.ambiguous(&c.head, &types, span),
            };
            return Ok((str_term(typ), self.resource_addr(rc.scope, &c.head)));
        }
        match self.resolve(rc, c, out)? {
            Res::Ref { typ, addr, path } if path.is_empty() => Ok((typ, addr)),
            _ => self.error(span, "expected a resource: a name in scope, or `T[e]`"),
        }
    }

    /// `f` with `want` picking among the resources a bare name shares.
    fn wanting<T>(&mut self, want: Want, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.want, want);
        let r = f(self);
        self.want = saved;
        r
    }

    fn ambiguous<T>(&mut self, name: &str, types: &[String], span: Span) -> L<T> {
        self.error(span, crate::types::ambiguous_resource(name, types))
    }

    /// A resource's attribute given a bare name two types share: every
    /// candidate, for the schema's `ref(T)` to pick (`types::read`, R-74).
    fn deferred_ref(&self, rc: &Rc, name: &str, span: Span) -> Option<Term> {
        let types = self.resource(rc.scope, name)?;
        if self.want != Want::Schema || types.len() < 2 {
            return None;
        }
        let addr = self.resource_addr(rc.scope, name);
        let candidates = types
            .iter()
            .map(|t| {
                func(
                    crate::ir::REF,
                    vec![str_term(t), addr.clone(), str_term("")],
                )
            })
            .collect();
        Some(crate::types::ambiguous_ref(name, span, candidates))
    }

    /// A relation atom, its arguments lowered at `pos`: positional, or
    /// named by the relation's columns (`p(a: x)`, H-12).
    fn atom(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Atom> {
        let span = self.span(n);
        if let Some(a) = self.exported_relation(rc, n, pos, pre)? {
            return Ok(a);
        }
        let pred = self.callee(n).ok_or(Skip);
        let Ok(mut pred) = pred else {
            return self.error(span, "a relation is named by a plain name (`p` or `m.p`)");
        };
        // A used module's relation: `releases.release(..)` is the
        // activation's `releases::release` (R-65).
        if let Some((m, p)) = pred.split_once('.')
            && let Some(path) = self.use_in(rc.scope, m)
        {
            let Some(scope) = self.decls.modules.get(&path).map(|m| m.scope) else {
                return Err(Skip);
            };
            if !self.decls.scopes[scope].arities.contains_key(p) {
                return self.error(span, format!("the module {path} has no relation `{p}`"));
            }
            pred = format!("{m}::{p}");
        }
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
        let args = match crate::zset::REF_RELATIONS.iter().find(|(p, ..)| *p == pred) {
            Some((_, arity, shape)) => {
                let list: Vec<SyntaxNode> = list
                    .as_ref()
                    .map(|l| terms(l).collect())
                    .unwrap_or_default();
                if list.len() != *arity {
                    return self.error(
                        span,
                        format!("`{pred}` takes {arity} arguments: `{shape}` (R-42)"),
                    );
                }
                let at = crate::zset::ref_column(&pred, *arity);
                let mut args = Vec::new();
                for (i, t) in list.iter().enumerate() {
                    args.push(if Some(i) == at {
                        self.ref_term(rc, t, pos, pre)?
                    } else {
                        self.term(rc, t, pos, pre)?
                    });
                }
                // `moved(T, "old-address", r)`: the old address is a path
                // too (R-112).
                if pred == "moved" {
                    self.address_key(&args[1], span)?;
                }
                args
            }
            // In a body an argument is a pattern (R-58): `pair((a, b))`;
            // a relation of several named columns takes one object
            // pattern, `zone({ name, index })`, its record pattern.
            None if pos == Pos::Content => {
                let list: Vec<SyntaxNode> = list
                    .as_ref()
                    .map(|l| terms(l).collect())
                    .unwrap_or_default();
                if let [o] = list.as_slice()
                    && o.kind() == OBJECT
                    && self.named_columns(rc.scope, &pred)
                {
                    let record = self.record_pattern(rc, o, pre)?;
                    return Ok(Atom {
                        pred,
                        args: Vec::new(),
                        record: Some(record),
                        span,
                    });
                }
                let mut args = Vec::new();
                for t in &list {
                    args.push(if t.kind() == TUPLE {
                        self.pattern(rc, t, pre)?
                    } else {
                        self.term(rc, t, pos, pre)?
                    });
                }
                args
            }
            None => self.args(rc, n, pos, pre)?,
        };
        Ok(Atom {
            pred,
            args,
            record: None,
            span,
        })
    }

    /// A relation another copy or deployment exports, `output p` (R-55):
    /// a copy's, `blue.p(x, y)`, the copy's rows `__rows(blue, "p", [x,
    /// y])`; every copy's, `c[t].p(x, y)`, those of each copy `t` of `c`;
    /// a deployment's, `platform[env=e].p(x, y)`, a row of what it
    /// published, `output("platform[env=e]", "p", Rows), member(Rows, [x,
    /// y])`. `None` for any other call.
    fn exported_relation(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Option<Atom>> {
        let span = self.span(n);
        let Some(c) = n.children().find_map(|c| Chain::of(&c)) else {
            return Ok(None);
        };
        let Some(Op::Field(p)) = c.ops.last() else {
            return Ok(None);
        };
        let p = p.clone();
        if rc.vars.contains_key(&c.head) {
            return Ok(None);
        }
        if let Some(d) = self.stack_in(rc.scope, &c.head) {
            let Res::Output { inst, key, .. } = self.deployed_path(rc, &c, &d, pre, span)? else {
                return Err(Skip);
            };
            let rows = self.read_var(rc, "output", vec![inst, str_term(&key)], 2, &p, pre, span);
            let cols = self.args(rc, n, pos, pre)?;
            return Ok(Some(atom_at("member", vec![rows, Term::List(cols)], span)));
        }
        let named = self
            .instance_in(rc.scope, &c.head)
            .filter(|_| c.ops.len() == 1);
        let every = matches!(c.ops.first(), Some(Op::Index(..)));
        if named.is_none() && !every {
            return Ok(None);
        }
        if let Some((_, path)) = &named {
            let exports = self
                .decls
                .modules
                .get(path)
                .is_some_and(|m| self.decls.scopes[m.scope].relation_outputs.contains(&p));
            if !exports {
                let d = Diagnostic::error(
                    span,
                    format!("{} exports no relation {p}", path),
                )
                .with_help(format!(
                    "a copy's relations are its own; `output {p}` in {path} exports it, read as \
                     `{}.{p}(..)`",
                    c.head
                ));
                self.diags.push(d);
                return Err(Skip);
            }
        }
        let mut prefix = c.clone();
        prefix.ops.pop();
        let Some(Res::Val(inst)) = self.scope_path(rc, &prefix, pre, span)? else {
            return Ok(None);
        };
        let cols = self.args(rc, n, pos, pre)?;
        Ok(Some(atom_at(
            crate::modules::ROWS,
            vec![inst, str_term(&p), Term::List(cols)],
            span,
        )))
    }

    /// Is `n` a reference: a resource, a typed variable, or a variable a
    /// reference column binds?
    fn is_reference(&mut self, rc: &Rc, n: &SyntaxNode) -> bool {
        if let Some(c) = Chain::of(n)
            && c.is_bare()
            && (rc.types.contains_key(&c.head) || rc.untyped_refs.contains(&c.head))
        {
            return true;
        }
        self.names_resource(rc, n)
    }

    /// Does `n` name one resource, by its name in scope or `T[e]`?
    fn names_resource(&mut self, rc: &Rc, n: &SyntaxNode) -> bool {
        let Some(c) = Chain::of(n) else {
            return false;
        };
        if c.is_bare() {
            return !rc.vars.contains_key(&c.head)
                && !rc.types.contains_key(&c.head)
                && self.resource(rc.scope, &c.head).is_some();
        }
        if rc.types.contains_key(&c.head) {
            return false;
        }
        let mut rc2 = rc.clone();
        let mut pre = Vec::new();
        matches!(
            self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)),
            Ok(Res::Ref { ref path, .. }) if path.is_empty()
        )
    }

    /// A resource where a relation takes one (R-42): by its name in scope,
    /// `T[e]`, or a variable, as a reference value. A variable `in T` binds
    /// is `ref(T, A, "")`: the reference taken apart, its address `A` what
    /// `r in T` and `r.path` read.
    /// The component a membership's right side names (`x in network`,
    /// `x in net.vpc`): its path.
    fn component_of(&self, rc: &Rc, c: &Chain) -> Option<String> {
        if rc.vars.contains_key(&c.head) || !c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            return None;
        }
        let fields = c.fields();
        let mut at = self
            .component_in(rc.scope, &c.head)
            .or_else(|| self.use_in(rc.scope, &c.head))?;
        for f in &fields[1..] {
            at = format!("{at}.{f}");
        }
        self.decls
            .modules
            .get(&at)
            .is_some_and(|m| m.component)
            .then_some(at)
    }

    /// The scope whose copies of the component `path` a read in this body
    /// sees: the body's own when it makes one, else its user's (the
    /// `instance_of` user column, as `c[t]` reads it).
    fn copies_scope(&self, rc: &Rc, path: &str) -> Term {
        let own = self
            .own_scopes(rc.scope)
            .iter()
            .find(|s| self.decls.scopes[**s].instances.values().any(|p| p == path))
            .copied();
        self.scope_term(rc.scope, own.unwrap_or(PROGRAM), str_term(""))
    }

    /// A copy where a resource is taken (R-67): its name in scope (`blue`)
    /// or a variable `x in network` binds, as a reference `ref(PATH, name,
    /// "")`, the copy's address `network["blue"]`.
    fn instance_ref(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> Option<Term> {
        if !c.is_bare() {
            return None;
        }
        if let Some(path) = rc.instances.get(&c.head).cloned() {
            let v = self.var_named(rc, &c.head, span);
            return Some(func(
                crate::ir::REF,
                vec![str_term(&path), var(&v), str_term("")],
            ));
        }
        if rc.vars.contains_key(&c.head) || self.resource(rc.scope, &c.head).is_some() {
            return None;
        }
        let (at, path) = self.instance_in(rc.scope, &c.head)?;
        let name = self.scope_term(rc.scope, at, str_term(&c.head));
        Some(func(
            crate::ir::REF,
            vec![str_term(&path), name, str_term("")],
        ))
    }

    fn ref_term(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<Term> {
        let span = self.span(n);
        if n.kind() == LITERAL {
            return self.error(
                span,
                "a resource here, not a value: its name in scope, `T[\"a\"]`, or a variable \
                 (R-42)",
            );
        }
        let Some(c) = Chain::of(n) else {
            return self.term(rc, n, pos, pre);
        };
        if let Some(r) = self.instance_ref(rc, &c, span) {
            return Ok(r);
        }
        if c.is_bare()
            && !rc.vars.contains_key(&c.head)
            && self.resource(rc.scope, &c.head).is_some()
        {
            let (typ, addr) = self.reference(rc, &c, pre, span)?;
            return Ok(func(crate::ir::REF, vec![typ, addr, str_term("")]));
        }
        match self.resolve(rc, &c, pre)? {
            Res::Ref { typ, addr, path } if path.is_empty() => {
                Ok(func(crate::ir::REF, vec![typ, addr, str_term("")]))
            }
            res => {
                if c.is_bare() && matches!(res, Res::Val(Term::Var(_))) {
                    rc.untyped_refs.insert(c.head.clone());
                }
                self.realize(rc, res, pos, pre, span)
            }
        }
    }

    /// A function's literal arguments read as its parameters' types (R-31):
    /// `inet.subnet("10.0.0.0/16", 8, 1)` takes an `inet`; a literal that
    /// cannot be one is an error at the call. Only the scalar types a
    /// literal can be checked against are; a `string` or `any` parameter
    /// takes what it is given.
    fn typed_args(&mut self, name: &str, args: Vec<Term>, span: Span) -> L<Vec<Term>> {
        // Read leniently (a refinement's text), a name is its own string.
        let Some(f) = crate::functions::get(name).filter(|_| !self.lenient && !self.text) else {
            return Ok(args);
        };
        // A call takes the arguments its signature declares (R-134: no
        // function takes any number of values but `format`).
        let lowering = f.internal;
        if !lowering && !f.takes(args.len()) {
            return self.error(
                span,
                format!(
                    "`{name}` is called with {} arguments: its signature is `{}`",
                    args.len(),
                    f.signature
                ),
            );
        }
        let mut out = Vec::with_capacity(args.len());
        for (i, a) in args.into_iter().enumerate() {
            let p = f
                .params
                .get(i)
                .or(if f.variadic { f.params.last() } else { None });
            let ty = match p.map(|p| crate::types::Ty::parse(&p.ty)) {
                Some(t @ crate::types::Ty::Scalar(_)) if p.is_some_and(|p| p.ty != "string") => t,
                _ => {
                    out.push(a);
                    continue;
                }
            };
            // A literal is read as the parameter's type at compile time; a
            // computed value is the body's to read (`engine::as_params`).
            let a = match a {
                Term::Val(_) => a,
                Term::Func { ref name, .. } if name == crate::types::AMBIGUOUS => a,
                a => {
                    out.push(a);
                    continue;
                }
            };
            match crate::types::literal(&ty, a) {
                Ok(a) => out.push(a),
                Err(why) => {
                    let p = p.map(|p| p.name.as_str()).unwrap_or("");
                    return self.error(span, format!("`{name}`'s argument `{p}` {why}"));
                }
            }
        }
        Ok(out)
    }

    /// `ref(r)`: the reference to the resource `r` (its name in scope,
    /// `T[e]`, a typed variable), written out where an attribute that is no
    /// `ref(T)` takes one: the provider gives it the resource's id (R-43).
    /// It lowers to `ref(ref(T, A, ""))`, the reference marked as asked
    /// for (`types` lets it into a `string` attribute); evaluated, it is
    /// the reference.
    fn ref_call(
        &mut self,
        rc: &mut Rc,
        list: &SyntaxNode,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Term> {
        let arg = terms(list).next().ok_or(Skip)?;
        match self.ref_term(rc, &arg, Pos::Content, pre)? {
            r @ Term::Func { .. } => Ok(func(crate::ir::REF, vec![r])),
            _ => self.error(
                span,
                "`ref(r)` takes a resource: its name in scope, `T[\"a\"]`, or a variable `in T`",
            ),
        }
    }

    /// Whether the bare name `c` is a resource in scope, not a variable, a
    /// type or a value name: `has warm_cache`.
    fn bare_resource(&self, rc: &Rc, c: &Chain) -> bool {
        c.is_bare()
            && c.head != "_"
            && !rc.vars.contains_key(&c.head)
            && !rc.types.contains_key(&c.head)
            && !self.is_value(rc.scope, &c.head)
            && self.resource(rc.scope, &c.head).is_some()
    }

    /// `has x.id`: an error naming `has x` (R-152), which asks whether the
    /// resource's identity is known.
    fn has_identity<T>(&mut self, n: &SyntaxNode, span: Span) -> L<T> {
        let text = n.text().to_string();
        let r = text
            .trim()
            .strip_suffix(&format!(".{}", crate::schema::IDENTITY))
            .unwrap_or("r")
            .to_string();
        let d = Diagnostic::error(
            span,
            format!(
                "`has {}`: a program does not read an id; write `has {r}`",
                text.trim()
            ),
        )
        .with_help(format!(
            "`has {r}` holds once `{r}` exists: its identity is known (R-152)"
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `x.id`: an error naming the reference (R-43). A program never reads
    /// a resource's id; the provider resolves a reference to it.
    fn identity_read<T>(&mut self, n: &SyntaxNode, span: Span) -> L<T> {
        let text = n.text().to_string();
        let r = text
            .trim()
            .strip_suffix(&format!(".{}", crate::schema::IDENTITY))
            .unwrap_or("r")
            .to_string();
        let d = Diagnostic::error(
            span,
            format!(
                "`{}`: a program does not read an id; a reference is the resource",
                text.trim()
            ),
        )
        .with_help(format!(
            "write `{r}` itself where an attribute takes the resource, or `ref({r})` where an \
             attribute that is not a `ref(T)` needs its id (R-43)"
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `sum` of a value known here not to be an int, `min`/`max` of one
    /// neither an int nor a string: an error at the call rather than a
    /// deny of every group.
    fn check_aggregated(&mut self, name: &str, args: &[Term], span: Span) {
        if self.lenient {
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
            self.diags.push(
                Diagnostic::error(
                    span,
                    format!("`{name}` is an aggregate: it is bound in a body, `n = {name}(x)`"),
                )
                .with_help(format!(
                    "`p(k, n) where n = {name}(x), B` folds per group of the head's other \
                     variables; `let n = {name}(x) where B` over one group"
                )),
            );
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
                    // A quantity (R-66): `500m` and `0.5` wait for the
                    // type of their position (`types::literal`, the
                    // schema's in `types::read`).
                    QUANTITY => match crate::quantity::literal(t.text()) {
                        Ok(crate::quantity::Literal::Known(q)) => Ok(Term::Val(Value::Quantity(q))),
                        Ok(crate::quantity::Literal::Ambiguous) => {
                            Ok(crate::types::ambiguous_literal(t.text(), span))
                        }
                        Err(why) => self.error(span, why),
                    },
                    STRING => self.string_term(rc, &t, pre),
                    TRUE_KW => Ok(Term::Val(Value::Bool(true))),
                    _ => Ok(Term::Val(Value::Bool(false))),
                }
            }
            CHAIN | CALL_CHAIN => {
                let mut c = Chain::read(n).ok_or(Skip)?;
                // `x.len` (R-155): the length of a list, a string or an
                // object, the one field that is a computation; `x."len"`
                // is an object's key.
                let len = matches!(c.ops.last(), Some(Op::Field(f)) if f == "len")
                    && tokens(n)
                        .filter(|t| !t.kind().is_trivia())
                        .last()
                        .is_some_and(|t| t.kind() == IDENT);
                if len {
                    c.ops.pop();
                    let res = self.resolve(rc, &c, pre)?;
                    let t = self.realize(rc, res, Pos::Content, pre, span)?;
                    return Ok(func(crate::ir::LEN, vec![t]));
                }
                // A resource by its bare name, given as a value: the
                // reference, in its module or out of it (R-43).
                if pos == Pos::Value
                    && c.is_bare()
                    && c.head != "_"
                    && !rc.vars.contains_key(&c.head)
                    && !rc.types.contains_key(&c.head)
                    && !self.is_value(rc.scope, &c.head)
                    && self.resource(rc.scope, &c.head).is_some()
                {
                    if let Some(t) = self.deferred_ref(rc, &c.head, span) {
                        return Ok(t);
                    }
                    let (typ, addr) = self.reference(rc, &c, pre, span)?;
                    return Ok(func(crate::ir::REF, vec![typ, addr, str_term("")]));
                }
                let res = self.resolve(rc, &c, pre)?;
                if let Res::Ref { path, .. } = &res
                    && matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
                {
                    return self.identity_read(n, span);
                }
                self.realize(rc, res, pos, pre, span)
            }
            CALL => {
                if let Some(t) = self.env_var_call(rc, n, pos, pre) {
                    return t;
                }
                if let Some(t) = self.loader_call(rc, n, pre) {
                    return t;
                }
                let name = self.callee(n);
                let Some(name) = name else {
                    return self.error(span, "a function is named by a plain name");
                };
                if name == "ref"
                    && let Some(list) = node(n, ARG_LIST)
                    && terms(&list).count() == 1
                {
                    return self.ref_call(rc, &list, pre, span);
                }
                let named: Vec<(String, SyntaxNode)> = node(n, ARG_LIST)
                    .into_iter()
                    .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG))
                    .filter_map(|a| Some((word_text(&a, 0), terms(&a).next()?)))
                    .collect();
                let declared = crate::functions::get(&name).filter(|f| !f.internal);
                if !named.is_empty() && declared.is_none() {
                    return self.error(
                        span,
                        format!(
                            "`{name}` is a function here: named arguments name a relation's \
                             columns in an atom"
                        ),
                    );
                }
                self.check_function(&name, span);
                let mut args = self.bind(false, |l| l.args(rc, n, Pos::Content, pre))?;
                if let (false, Some(f)) = (named.is_empty(), declared) {
                    let mut given = Vec::new();
                    for (k, t) in named {
                        given.push((k, self.bind(false, |l| l.term(rc, &t, Pos::Content, pre))?));
                    }
                    args = match crate::functions::with_named(f, args, given) {
                        Ok(a) => a,
                        Err(why) => return self.error(span, why),
                    };
                }
                self.check_aggregated(&name, &args, span);
                // `cloud_ref(T, name, path)`, a form of the language, is
                // the lowering's `__cloud_ref` (R-155).
                let name = match name.as_str() {
                    "cloud_ref" => crate::ir::CLOUD_REF.to_string(),
                    "ref" => crate::ir::REF.to_string(),
                    _ => name,
                };
                let args = self.typed_args(&name, args, span)?;
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
                // A key with holes (`{ "${k}": v }`) is computed: the
                // object is built at run time (`functions::OBJECT`).
                let computed = n
                    .children()
                    .filter(|c| c.kind() == OBJECT_FIELD)
                    .filter_map(|f| tokens(&f).next())
                    .any(|k| k.kind() == STRING && !self.text && has_hole(k.text()));
                if computed {
                    return self.computed_object(rc, n, pos, pre);
                }
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
                                call: None,
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
                self.nested += 1;
                let body = self.body(rc, &node(n, BODY).ok_or(Skip)?);
                self.nested -= 1;
                let mut body = body?;
                let item_node = terms(n).next().ok_or(Skip)?;
                // A resource collected into a value is its reference.
                let item_pos = if pos == Pos::Value {
                    Pos::Value
                } else {
                    Pos::Whole
                };
                let item = self.bind(false, |l| l.term(rc, &item_node, item_pos, &mut body))?;
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
            TUPLE => self.tuple_value(n),
            BIN_EXPR => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let op = tokens(n).next().ok_or(Skip)?;
                // `us-test-1a`, names and a quantity with no spaces (R-66
                // lexes `1a` as one token): meant as a string.
                let word =
                    |k: SyntaxKind| matches!(k, IDENT | QUANTITY | INT | MINUS) || k.is_keyword();
                let toks: Vec<_> = n
                    .descendants_with_tokens()
                    .filter_map(|e| e.into_token())
                    .collect();
                if op.kind() == MINUS
                    && toks.iter().all(|t| word(t.kind()))
                    && toks.iter().any(|t| t.kind() == QUANTITY)
                    && toks.iter().any(|t| t.kind() == IDENT)
                {
                    let d = Diagnostic::error(
                        span,
                        format!("`{}` is arithmetic, not a name", n.text()),
                    )
                    .with_help(format!(
                        "`-` is always an operator; a name with one is a string: \"{}\"",
                        n.text()
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
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
                match crate::types::operands(op.text(), a, b) {
                    Ok((a, b)) => Ok(func(name, vec![a, b])),
                    Err(why) => self.error(span, why),
                }
            }
            UNARY_EXPR => {
                let inner = terms(n).next().ok_or(Skip)?;
                Ok(
                    match self.bind(false, |l| l.term(rc, &inner, Pos::Content, pre))? {
                        Term::Val(Value::Int(i)) if inner.kind() == LITERAL => {
                            Term::Val(Value::Int(-i))
                        }
                        Term::Val(Value::Quantity(q)) if inner.kind() == LITERAL => {
                            match crate::quantity::scale(&q, -1) {
                                Some(q) => Term::Val(Value::Quantity(q)),
                                None => return self.error(span, "quantity out of range"),
                            }
                        }
                        t => func("sub", vec![Term::Val(Value::Int(0)), t]),
                    },
                )
            }
            // A range is enumerated (R-56); it is never a value, so it
            // never becomes a list by accident.
            RANGE => {
                let d = Diagnostic::error(
                    span,
                    format!(
                        "a range is enumerated with `in`; `[{}]` is not a list",
                        n.text()
                    ),
                )
                .with_help(format!(
                    "write `i in {}` in a body, or `int.range(lo, hi, step)` for a list",
                    n.text()
                ));
                self.diags.push(d);
                Err(Skip)
            }
            k => self.error(span, format!("unexpected {k:?} as a term")),
        }
    }

    /// `lo..hi` (half-open) or `lo..=hi` (inclusive) after `in`: the list
    /// `int.range(lo, hi, 1)` whose members `in` enumerates in order
    /// (R-56). Both ends must be bound integers.
    fn range(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<Term> {
        let ends: Vec<SyntaxNode> = terms(n).collect();
        let [lo, hi] = ends.as_slice() else {
            return Err(Skip);
        };
        let lo = self.bind(false, |l| l.term(rc, lo, Pos::Content, out))?;
        let mut hi = self.bind(false, |l| l.term(rc, hi, Pos::Content, out))?;
        // A literal end is checked as the `int` it must be (R-31).
        for end in [&lo, &hi] {
            if let Term::Val(v) = end
                && !matches!(v, Value::Int(_))
            {
                let d = Diagnostic::error(
                    self.span(n),
                    format!(
                        "a range's ends are integers: `{}` has {}",
                        n.text(),
                        crate::partition::fmt_value(v)
                    ),
                );
                self.diags.push(d);
                return Err(Skip);
            }
        }
        if tokens(n).any(|t| t.kind() == DOT2_EQ) {
            hi = match hi {
                Term::Val(Value::Int(i)) => Term::Val(Value::Int(i + 1)),
                t => func("add", vec![t, Term::Val(Value::Int(1))]),
            };
        }
        Ok(func("int.range", vec![lo, hi, Term::Val(Value::Int(1))]))
    }

    /// A string literal: `"a${e}b"` is `str.format("a%sb", e)` (H-13), `$${`
    /// is a literal `${`, and a brace is itself.
    /// `{ "${k}": v, b: w }`: an object with a computed key, built at run
    /// time, each key then its value (`functions::OBJECT`). A key is a
    /// string term like any other: its holes are read now.
    fn computed_object(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let mut args = Vec::new();
        let mut keys = BTreeSet::new();
        for f in n.children().filter(|c| c.kind() == OBJECT_FIELD) {
            let k = tokens(&f).next().ok_or(Skip)?;
            let key = match k.kind() {
                STRING => self.string_term(rc, &k, pre)?,
                _ => str_term(k.text()),
            };
            if let Term::Val(Value::Str(name)) = &key
                && !keys.insert(name.clone())
            {
                return self.error(self.span(&f), format!("key `{name}` given twice"));
            }
            let v = match terms(&f).next() {
                Some(t) => self.term(rc, &t, pos, pre)?,
                // `{ a }` is `{ a: a }`.
                None => {
                    let c = Chain {
                        head: k.text().to_string(),
                        head_kind: k.kind(),
                        call: None,
                        range: k.text_range(),
                        ops: Vec::new(),
                    };
                    let res = self.resolve(rc, &c, pre)?;
                    self.realize(rc, res, pos, pre, self.span_of(k.text_range()))?
                }
            };
            args.extend([key, v]);
        }
        Ok(func(crate::functions::OBJECT, args))
    }

    fn string_term(&mut self, rc: &mut Rc, t: &SyntaxToken, pre: &mut Vec<Lit>) -> L<Term> {
        let text = t.text();
        if self.text || !text.contains("${") {
            return Ok(str_term(&self.string(t)?));
        }
        let span = self.span_of(t.text_range());
        let base: u32 = t.text_range().start().into();
        let mut fmt = String::new();
        let mut args = Vec::new();
        let flush = |lit: &str, fmt: &mut String, l: &mut Self| -> L<()> {
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
            Ok(())
        };
        let ps = match scan(text) {
            Ok(ps) => ps,
            Err(at) => {
                let at = base + at as u32;
                let span = self.span_of(rowan::TextRange::new(at.into(), (at + 2).into()));
                return self.error(
                    span,
                    "an interpolation `${` is never closed; a literal `${` is `$${`",
                );
            }
        };
        for p in ps {
            match p {
                Piece::Text(lit) => flush(&lit, &mut fmt, self)?,
                Piece::Hole(hole, at) => {
                    fmt.push_str("%s");
                    let at = base + at as u32;
                    args.push(self.bind(false, |l| l.hole(rc, hole, at, pre))?);
                }
            }
        }
        if args.is_empty() {
            return Ok(str_term(&fmt));
        }
        let mut all = vec![str_term(&fmt)];
        all.extend(args);
        Ok(func(crate::ir::FORMAT, all))
    }

    /// An interpolation hole: a term, read now (a content position).
    fn hole(&mut self, rc: &mut Rc, src: &str, at: u32, pre: &mut Vec<Lit>) -> L<Term> {
        // A variable `in T` types is a reference: it interpolates as its
        // address, `T["A"]`, as an untyped one does (R-42).
        let parse = parse::parse_term(src.trim());
        let typed = terms(&parse.syntax())
            .next()
            .and_then(|t| Chain::of(&t))
            .is_some_and(|c| c.is_bare() && rc.types.contains_key(&c.head));
        let pos = if typed { Pos::Value } else { Pos::Content };
        self.text_term(rc, src, at, pos, pre)
    }

    /// A block entry's value: its term, or, for an entry that is only a
    /// path, the pun: the path's last segment as a term, resolved where the
    /// value would be (R-33), `color` for `spec.selector.color`.
    fn entry_value(
        &mut self,
        rc: &mut Rc,
        a: &SyntaxNode,
        pos: Pos,
        reads: &mut Vec<Lit>,
    ) -> L<Term> {
        if let Some(t) = terms(a).next() {
            return self.term(rc, &t, pos, reads);
        }
        let path = node(a, BLOCK_PATH).ok_or(Skip)?;
        let seg = tokens(&path).last().ok_or(Skip)?;
        if !seg.kind().is_word()
            || matches!(
                seg.kind(),
                NOT_KW | IN_KW | HAS_KW | WHERE_KW | IF_KW | TRUE_KW | FALSE_KW
            )
        {
            let d = Diagnostic::error(
                self.span(a),
                format!(
                    "`{}` names no value: an entry is `path = term`",
                    path.text()
                ),
            )
            .with_help("an entry that is only a path takes the value its last segment names");
            self.diags.push(d);
            return Err(Skip);
        }
        let at: u32 = seg.text_range().start().into();
        self.text_term(rc, seg.text(), at, pos, reads)
    }

    /// The term `src`, written at byte `at`, lowered as a term in `pos`.
    fn text_term(
        &mut self,
        rc: &mut Rc,
        src: &str,
        at: u32,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
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
            self.term(rc, &t, pos, pre)
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
        rc.uses.add(name, span);
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
        if let Some(call) = &c.call {
            return self.call_read(rc, call, &c.ops, pre);
        }
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
        // A copy whose `instance` is the error reads nothing, and says so
        // there.
        if !rc.vars.contains_key(h) && self.unbound_instance(rc.scope, h) {
            return Err(Skip);
        }
        // A copy `x in network` binds (R-67): its name, `x.k` its output.
        if let Some(path) = rc.instances.get(h).cloned() {
            let v = self.var_named(rc, h, span);
            return self.output_of(rc, var(&v), &path, &c.ops, pre, span);
        }
        // A typed variable: a reference.
        if let Some(t) = rc.types.get(h).cloned() {
            if let Some(Op::Field(first)) = c.ops.first() {
                self.namespace_attr(rc, h, first, span)?;
            }
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Ref {
                typ: t,
                addr: var(&v),
                path,
            });
        }
        // A name a resource shares, read as the other thing's (R-76); in a
        // module's or a component's body, the body's own (R-101).
        let own = match rc.vars.contains_key(h) {
            true => None,
            false => self.own_wins(rc.scope, h),
        };
        let shared = !rc.vars.contains_key(h)
            && match own {
                Some(resource) => !resource,
                None => self.shared_name(rc.scope, c, span)?,
            };
        if !rc.vars.contains_key(h) {
            if self.is_value(rc.scope, h) && own != Some(true) {
                return self.value(rc, c, pre, span);
            }
            // `settings.x` names a resource called `settings` in scope; a
            // settings row, `settings[e]`, is gone (R-38).
            let resource =
                matches!(c.ops.first(), Some(Op::Field(_))) && self.resource(rc.scope, h).is_some();
            if c.head_kind == SETTINGS_KW && !c.is_bare() && !resource {
                return self.settings_read(c, span);
            }
            if h == "world" && !c.ops.is_empty() {
                return self.world(rc, c, pre, span);
            }
        }
        if c.is_bare() {
            if c.head_kind == SETTINGS_KW && !rc.vars.contains_key(h) {
                return self.settings_read(c, span);
            }
            return self.bare(rc, h, span);
        }
        if !rc.vars.contains_key(h) {
            if let Some(types) = self.resource(rc.scope, h).filter(|_| !shared) {
                if types.len() > 1 {
                    return self.ambiguous(h, &types, span);
                }
                let path = self.segs(rc, &c.ops, pre)?;
                return Ok(Res::Ref {
                    typ: str_term(&types[0]),
                    addr: self.resource_addr(rc.scope, h),
                    path,
                });
            }
            if let Some(r) = self.scope_path(rc, c, pre, span)? {
                return Ok(r);
            }
            if let Some(r) = self.typed_path(rc, c, pre, span)? {
                return Ok(r);
            }
        }
        if rc.untyped_refs.contains(h) {
            let d = Diagnostic::error(
                span,
                format!("`{h}` is a reference of no known type: `{h}.path` reads nothing"),
            )
            .with_help(format!(
                "bind its type first, `{h} in T`, and `{h}.path` reads that resource (R-43)"
            ));
            self.diags.push(d);
            return Err(Skip);
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
            "no resource, module, copy, input, `let` or type in scope is named `{h}`; a \
             string is quoted: {quoted}"
        ));
        if c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            d = d.with_fix(format!("quote it: {quoted}"), vec![(span, quoted)]);
        }
        self.diags.push(d);
        Err(Skip)
    }

    /// `f(x).p[i]` (R-71): the call bound to a variable, once per rule, as
    /// `v = f(x)` would, and the path read from it.
    fn call_read(
        &mut self,
        rc: &mut Rc,
        call: &SyntaxNode,
        ops: &[Op],
        pre: &mut Vec<Lit>,
    ) -> L<Res> {
        let t = self.bind(false, |l| l.term(rc, call, Pos::Content, pre))?;
        let key = format!("call {t:?}");
        let v = match rc.reads.get(&key) {
            Some(v) => v.clone(),
            None => {
                let name = self.callee(call).unwrap_or_default();
                let base = capitalise(name.rsplit('.').next().unwrap_or_default());
                let v = var(&fresh(rc, &base));
                pre.push(Lit::Eq(v.clone(), t));
                rc.reads.insert(key, v.clone());
                v
            }
        };
        let path = self.segs(rc, ops, pre)?;
        Ok(Res::Var { var: v, path })
    }

    /// A value name: `k(V)`, read once per rule. A `let` holding a
    /// reference (H-6) reads through it: `cfg.x` is `cfg(E), setting(E,
    /// "x", V)`.
    fn value(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let pred = c.head.clone();
        // An input typed `ref(T)` holds the reference itself: a dot reads
        // through it, the address taken out of the reference (R-101). It
        // is its user's, so no copy's scope goes in front of it.
        if !c.is_bare()
            && let Some(typ) = self.input_ref(rc.scope, &pred)
        {
            let key = format!("ref {pred}");
            let addr = match rc.values.get(&key) {
                Some(v) => var(v),
                None => {
                    let name = fresh(rc, &capitalise(&pred));
                    let mark = func(crate::modules::ABSOLUTE, vec![var(&name)]);
                    let whole = func(crate::ir::REF, vec![str_term(&typ), mark, str_term("")]);
                    pre.push(Lit::Pos(atom_at(&pred, vec![whole], span)));
                    rc.values.insert(key, name.clone());
                    var(&name)
                }
            };
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Ref {
                typ: str_term(&typ),
                addr: func(crate::modules::ABSOLUTE, vec![addr]),
                path,
            });
        }
        let ty = match self.value_type(rc.scope, &pred) {
            Ok(t) => t,
            Err(e) => return self.error(span, e),
        };
        // A `let` holding a live object is its name alone; one holding a
        // resource is the reference (R-43).
        let Some(ty) = ty.filter(|t| !c.is_bare() || matches!(t, VType::Ref(_))) else {
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
        // A type named by one word (a program's own, `widget`) is that type.
        if !rc.vars.contains_key(h) && self.decls.types.contains(h) {
            return Ok(Res::Type(h.to_string()));
        }
        if !rc.vars.contains_key(h) {
            let what = if self.resource(rc.scope, h).is_some() {
                Some("the resource")
            } else if self.instance_in(rc.scope, h).is_some() {
                Some("the component's resource")
            } else if self.use_in(rc.scope, h).is_some() || self.stack_in(rc.scope, h).is_some() {
                Some("the module")
            } else if self.component_in(rc.scope, h).is_some() {
                Some("the component")
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
                    let keyed = match ts[0].kind() {
                        LITERAL => tokens(&ts[0]).next().is_some_and(|t| t.kind() == STRING),
                        CALL => true,
                        _ => Chain::of(&ts[0]).is_some_and(|c| !c.is_bare()),
                    };
                    let t = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                    out.push(if keyed { Seg::K(t) } else { Seg::I(t) });
                }
                Op::Keyed(_, r) => {
                    return self.error(
                        self.span_of(*r),
                        "`[k=v]` names a stack's deployment by its keys, after the name a \
                         `use` of the stack binds: `platform[env=e].output`",
                    );
                }
            }
        }
        Ok(out)
    }

    /// `settings[e].path`, `settings` alone: the settings rows and their
    /// pseudo-type are gone (R-38); a setting is an input, read by its name.
    fn settings_read(&mut self, c: &Chain, span: Span) -> L<Res> {
        let path: Vec<String> = c
            .ops
            .iter()
            .skip_while(|o| !matches!(o, Op::Field(_)))
            .filter_map(|o| match o {
                Op::Field(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        let read = match path.is_empty() {
            true => "the input by its name".to_string(),
            false => format!("the input by its name, `{}`", path.join(".")),
        };
        let d = Diagnostic::error(span, "settings rows are gone (R-38): a setting is an input")
            .with_help(format!(
                "declare it, `input k: T = default`, give it per deployment with `set k = v \
                 where env == \"prod\"`, and read {read}"
            ));
        self.diags.push(d);
        Err(Skip)
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

    /// What a chain whose head names a scope reads (R-65): `n.k`, an
    /// output of the instance `n` (`n` alone, its scope); `c[t].k`, an
    /// output of each instance of the component `c` (`instance_of(c, T),
    /// output(T, k, V)`); `m.x`, the value `x` of a used module `m`; and a
    /// stack's deployment, `platform[env=e].k`.
    fn scope_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let h = c.head.as_str();
        if let Some(Op::Field(x)) = c.ops.first() {
            self.alternatives_agree(rc.scope, h, x, span)?;
        }
        if let Some((at, path)) = self.instance_in(rc.scope, h) {
            let inst = self.scope_term(rc.scope, at, str_term(h));
            return self.output_of(rc, inst, &path, &c.ops, pre, span).map(Some);
        }
        if let Some(d) = self.stack_in(rc.scope, h) {
            return self.deployed_path(rc, c, &d, pre, span).map(Some);
        }
        // A component by a name in scope (`network`), a used module's
        // (`net.vpc`), or by its path from the root (`modules.net.vpc`).
        let mut ops = &c.ops[..];
        let mut path = match self.component_in(rc.scope, h) {
            Some(p) => Some(p),
            None => self.use_in(rc.scope, h),
        };
        if path.is_none() {
            let mut at = h.to_string();
            while let Some(Op::Field(f)) = ops.first() {
                if self.decls.modules.contains_key(&at) {
                    break;
                }
                at = format!("{at}.{f}");
                ops = &ops[1..];
            }
            if !self.decls.modules.contains_key(&at) {
                return Ok(None);
            }
            path = Some(at);
        }
        let Some(mut at) = path.take() else {
            return Ok(None);
        };
        while let Some(Op::Field(f)) = ops.first() {
            let next = format!("{at}.{f}");
            if !self.decls.modules.get(&next).is_some_and(|m| m.component) {
                break;
            }
            at = next;
            ops = &ops[1..];
        }
        let component = self.decls.modules.get(&at).is_some_and(|m| m.component);
        match ops.first() {
            Some(Op::Index(ts, _)) if component && ts.len() == 1 => {
                let t = self.bind(true, |l| l.term(rc, &ts[0], Pos::Content, pre))?;
                // The copies of `at` this body makes, or its user's.
                let own = self
                    .own_scopes(rc.scope)
                    .iter()
                    .find(|s| self.decls.scopes[**s].instances.values().any(|p| *p == at))
                    .copied();
                let at_scope = own.unwrap_or(PROGRAM);
                let parent = self.scope_term(rc.scope, at_scope, str_term(""));
                pre.push(Lit::Pos(atom_at(
                    crate::modules::INSTANCE_OF,
                    vec![str_term(&at), parent, t.clone()],
                    span,
                )));
                let inst = self.scope_term(rc.scope, at_scope, t);
                self.output_of(rc, inst, &at, &ops[1..], pre, span)
                    .map(Some)
            }
            _ if component => self
                .error(
                    span,
                    format!(
                        "{} is a component: read a copy's outputs by its name, `NAME.output`, \
                         or every copy's, `{}[t].output`",
                        at,
                        c.fields()[..c.fields().len() - ops.len().min(c.fields().len() - 1)]
                            .join(".")
                    ),
                )
                .map(Some),
            // A used module's item: a value (`config.region`, read as
            // `config::region`), an output, or a resource (`synapse.vm`,
            // the address `synapse::vm`).
            Some(Op::Field(x))
                if self.use_in(rc.scope, h).is_some() && ops.len() == c.ops.len() =>
            {
                let Some(module) = self.decls.modules.get(&at).cloned() else {
                    return Err(Skip);
                };
                let declared = self
                    .chain_of(rc.scope)
                    .into_iter()
                    .find(|s| self.decls.scopes[*s].uses.contains_key(h))
                    .unwrap_or(PROGRAM);
                let own = &self.decls.scopes[module.scope];
                if own.values.contains(x) {
                    let path = self.segs(rc, &ops[1..], pre)?;
                    return Ok(Some(Res::Value {
                        pred: format!("{h}::{x}"),
                        path,
                    }));
                }
                if own.outputs.contains_key(x) {
                    let inst = self.scope_term(rc.scope, declared, str_term(h));
                    return self.output_of(rc, inst, &at, ops, pre, span).map(Some);
                }
                if let Some(types) = own.resources.get(x).cloned() {
                    if types.len() > 1 {
                        return self.ambiguous(x, &types, span).map(Some);
                    }
                    let scope = self.scope_term(rc.scope, declared, str_term(h));
                    let path = self.segs(rc, &ops[1..], pre)?;
                    return Ok(Some(Res::Ref {
                        typ: str_term(&types[0]),
                        addr: func(
                            crate::ir::SCOPED,
                            vec![scope, str_term(&crate::ir::name_segment(x))],
                        ),
                        path,
                    }));
                }
                self.error(
                    span,
                    format!("the module {at} has no value, output or resource `{x}`"),
                )
                .map(Some)
            }
            _ => self
                .error(
                    span,
                    format!("{at} is a module: `use {at}` to read its items, `NAME.x`"),
                )
                .map(Some),
        }
    }

    /// `h.x` where `h` is declared more than once, each under a clause
    /// (R-104): every declaration has `x`, of one type, else the read is
    /// an error naming every declaration.
    fn alternatives_agree(&mut self, scope: usize, h: &str, x: &str, span: Span) -> L<()> {
        let Some(alts) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.decls.scopes[*s].bound.contains_key(h))
            .and_then(|s| self.decls.scopes[s].alternatives.get(h).cloned())
        else {
            return Ok(());
        };
        let mut found = Vec::new();
        for (path, n) in &alts {
            let what = match n.kind() {
                USE => format!("use {}", use_parts(n).0),
                _ => {
                    let (c, name) = copy_parts(n);
                    format!("resource {c} {name}")
                }
            };
            let item = self.decls.modules.get(path).and_then(|m| {
                let sc = &self.decls.scopes[m.scope];
                if sc.outputs.contains_key(x) {
                    let ty = sc.output_types.get(x).map_or("any", String::as_str);
                    return Some(format!("output {x}: {ty}"));
                }
                if let Some(i) = sc.input_nodes.get(x) {
                    let ty = node(i, TYPE_EXPR)
                        .map(|t| t.text().to_string().replace(char::is_whitespace, ""))
                        .unwrap_or_default();
                    return Some(format!("input {x}: {ty}"));
                }
                if sc.values.contains(x) {
                    return Some(format!("let {x}"));
                }
                sc.resources
                    .get(x)
                    .map(|ts| format!("resource {}", ts.join(", ")))
            });
            found.push((what, item, self.span(n)));
        }
        if let Some((what, _, _)) = found.iter().find(|(_, i, _)| i.is_none()) {
            let mut d = Diagnostic::error(span, format!("`{h}.{x}`: `{what}` has no `{x}`"));
            for (w, i, at) in &found {
                d = d.with_label(*at, format!("{w}: {}", i.as_deref().unwrap_or("none")));
            }
            self.diags.push(d.with_help(format!(
                "each declaration of `{h}` must have `{x}`: a component signature says what \
                 they all have, `type T = component {{ output {x}: TYPE }}`"
            )));
            return Err(Skip);
        }
        let first = &found[0].1;
        if found.iter().any(|(_, i, _)| i != first) {
            let mut d = Diagnostic::error(
                span,
                format!("`{h}.{x}` has another type in each declaration of `{h}`"),
            );
            for (w, i, at) in &found {
                d = d.with_label(*at, format!("{w}: {}", i.as_deref().unwrap_or("")));
            }
            self.diags.push(d.with_help(
                "a component signature unifies them: `type T = component { .. }` declares the \
                 inputs and outputs, and `component C: T { .. }` is checked against it",
            ));
            return Err(Skip);
        }
        Ok(())
    }

    /// `.k.path` after an instance's scope `inst`, of the component at
    /// `path`: its output `k`, a reference when the output is typed by a
    /// resource type.
    fn output_of(
        &mut self,
        rc: &mut Rc,
        inst: Term,
        path: &str,
        ops: &[Op],
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        match ops.first() {
            None => Ok(Res::Val(inst)),
            Some(Op::Field(k)) => {
                let segs = self.segs(rc, &ops[1..], pre)?;
                let typed = self
                    .decls
                    .modules
                    .get(path)
                    .and_then(|m| self.decls.scopes[m.scope].outputs.get(k).cloned())
                    .flatten();
                if let Some(t) = &typed
                    && !segs.is_empty()
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
                    return Ok(Res::Ref {
                        typ: str_term(t),
                        addr: v,
                        path: segs,
                    });
                }
                Ok(Res::Output {
                    inst,
                    key: k.clone(),
                    path: segs,
                    typ: typed.map(|t| str_term(&t)),
                })
            }
            Some(_) => self.error(span, "after a component's resource: `.output`"),
        }
    }

    /// `NAME[k=v, ..].out.path`, or `NAME.out.path` unkeyed: an output of
    /// a stack's deployment (R-65), the instance `NAME[k=v,..]`, read from
    /// what it published. Each key is given once.
    fn deployed_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        d: &Deployed,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let m = &c.head;
        let spelled = |keys: &[String]| {
            let ks: Vec<String> = keys.iter().map(|k| format!("{k}=..")).collect();
            format!("{m}[{}].OUTPUT", ks.join(", "))
        };
        let keys = d.keys.clone();
        // `NAME[env]`, every entry a bare name: the pun `NAME[env = env]`
        // (R-33), as beside a `k = v`.
        let punned: Option<Op> = match c.ops.first() {
            Some(Op::Index(ts, r)) if !keys.is_empty() => ts
                .iter()
                .map(|t| Some((bare_name(t)?, t.clone())))
                .collect::<Option<Vec<_>>>()
                .map(|entries| Op::Keyed(entries, *r)),
            _ => None,
        };
        let (name, rest) = match punned.as_ref().or(c.ops.first()) {
            Some(Op::Keyed(entries, r)) => {
                let given: BTreeSet<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
                let want: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
                if given != want || given.len() != entries.len() {
                    return self.error(
                        self.span_of(*r),
                        format!(
                            "a deployment of {} is named by each of its keys once: `{}`",
                            d.path,
                            spelled(&keys)
                        ),
                    );
                }
                let mut values = Vec::new();
                for k in &keys {
                    let (_, t) = entries.iter().find(|(x, _)| x == k).expect("checked");
                    let v = self.bind(true, |l| l.term(rc, t, Pos::Content, pre))?;
                    values.push(match v {
                        Term::Val(Value::Str(s)) => str_term(&crate::stack::escape(&s)),
                        Term::Val(v) => {
                            str_term(&crate::stack::escape(&crate::partition::fmt_bare(&v)))
                        }
                        v => v,
                    });
                }
                let text = |vs: Vec<String>| {
                    let kv: Vec<String> = keys
                        .iter()
                        .zip(vs)
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    format!("{}[{}]", d.name, kv.join(","))
                };
                let constant: Option<Vec<String>> = values
                    .iter()
                    .map(|v| match v {
                        Term::Val(Value::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                let name = match constant {
                    Some(vs) => str_term(&text(vs)),
                    None => {
                        let holes = text(vec!["%s".to_string(); keys.len()]);
                        func(
                            crate::ir::FORMAT,
                            std::iter::once(str_term(&holes)).chain(values).collect(),
                        )
                    }
                };
                (name, &c.ops[1..])
            }
            Some(Op::Field(_)) if keys.is_empty() => (str_term(&d.name), &c.ops[..]),
            _ if keys.is_empty() => {
                return self.error(
                    span,
                    format!(
                        "{} is deployed: what is read of it is an output, `{m}.OUTPUT`",
                        d.path
                    ),
                );
            }
            _ => {
                return self.error(
                    span,
                    format!(
                        "{} is deployed, keyed by {}: read an output of one deployment, `{}`",
                        d.path,
                        keys.join(", "),
                        spelled(&keys)
                    ),
                );
            }
        };
        let Some(Op::Field(k)) = rest.first() else {
            return self.error(
                span,
                format!(
                    "{} is deployed: what is read of it is an output, `{m}[..].OUTPUT`",
                    d.path
                ),
            );
        };
        let path = self.segs(rc, &rest[1..], pre)?;
        // The keyed read of a copy's output (`network[t].k`), the
        // deployment the instance: `instance_of(PATH, "", NAME), output(NAME,
        // k, V)`, both served from what it published (I-modules section 6).
        let inst = self.scope_term(rc.scope, PROGRAM, name);
        let user = self.scope_term(rc.scope, PROGRAM, str_term(""));
        pre.push(Lit::Pos(atom_at(
            crate::modules::INSTANCE_OF,
            vec![str_term(&d.path), user, inst.clone()],
            span,
        )));
        Ok(Res::Output {
            inst,
            key: k.clone(),
            path,
            typ: None,
        })
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
            // `T["n"]` for a resource `n` in scope: it is named `n` (H-10),
            // unless `n` names something else too (R-76).
            if !self.any_type
                && let Some(LITERAL) = ts.first().map(|t| t.kind())
                && let Some(s) = tokens(&ts[0]).find(|t| t.kind() == STRING)
                && let Ok(n) = string_value(s.text())
                && !has_hole(s.text())
                && self
                    .resource(rc.scope, &n)
                    .is_some_and(|types| types == vec![name.clone()])
                && !self.names_other(rc.scope, &n)
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
            self.address_key(&addr, span)?;
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

    /// An address written as a constant, `T["blue/vpc"]` or `"blue/vpc"
    /// in T`, is a path (R-112): `/` (and R-72's `::` before it) is an
    /// error naming the dot form, `"blue.vpc"`.
    fn address_key(&mut self, addr: &Term, span: Span) -> L<()> {
        if let Term::Val(Value::Str(a)) = addr
            && let Some(fixed) = crate::ir::old_scope(a)
        {
            let d = Diagnostic::error(
                span,
                format!("\"{a}\": an address is a path, its scope separated by `.`, not `/`"),
            )
            .with_help(format!("write \"{fixed}\" (R-112)"));
            self.diags.push(d);
            return Err(Skip);
        }
        Ok(())
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
            Res::Ref { typ, addr, path } if path.is_empty() => Ok(match pos {
                Pos::Value => func(crate::ir::REF, vec![typ, addr, str_term("")]),
                _ => addr,
            }),
            Res::Ref { typ, addr, path } if pos != Pos::Content => {
                let Some(p) = path_string(&path) else {
                    return self.error(span, "a reference's path is constant");
                };
                Ok(func(crate::ir::REF, vec![typ, addr, str_term(&p)]))
            }
            Res::Ref { typ, addr, path } => {
                let Seg::F(first) = &path[0] else {
                    return self.error(span, "a resource's attribute is `r.name`");
                };
                let first = crate::ir::path_key(first).into_owned();
                let v = self.read_var(
                    rc,
                    "attr",
                    vec![typ.clone(), addr, str_term(&first)],
                    3,
                    &first,
                    pre,
                    span,
                );
                let at = (typ, first.clone());
                self.keyed_path_of(rc, v, path[1..].to_vec(), Some(at), pre, span)
            }
            Res::Output {
                inst,
                key,
                path,
                typ,
            } => {
                let v = self.read_var(rc, "output", vec![inst, str_term(&key)], 2, &key, pre, span);
                match typ {
                    // A typed output holds an address: given as a value, it
                    // is the reference (R-43).
                    Some(typ) if pos == Pos::Value && path.is_empty() => {
                        Ok(func(crate::ir::REF, vec![typ, v, str_term("")]))
                    }
                    _ => self.path_of(rc, v, path, pre, span),
                }
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
        v: Term,
        path: Vec<Seg>,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Term> {
        self.keyed_path_of(rc, v, path, None, pre, span)
    }

    /// `path_of` from the attribute `at` (the resource's type and the
    /// attribute's top segment) of a resource: an element by its key,
    /// `containers["api"]`, is the element of the keyed list whose one key
    /// field (`type_list_key`) has that value (R-35). Elsewhere a key is a
    /// position.
    fn keyed_path_of(
        &mut self,
        rc: &mut Rc,
        mut v: Term,
        path: Vec<Seg>,
        mut at: Option<(Term, String)>,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Term> {
        let mut fields: Vec<String> = Vec::new();
        let flush = |v: Term, fields: &mut Vec<String>| {
            if fields.is_empty() {
                return v;
            }
            let p = fields
                .iter()
                .map(|f| crate::ir::path_key(f))
                .collect::<Vec<_>>()
                .join(".");
            fields.clear();
            func("__path", vec![v, str_term(&p)])
        };
        for s in path {
            match s {
                Seg::F(f) => {
                    if let Some((_, list)) = &mut at {
                        list.push('.');
                        list.push_str(&crate::ir::path_key(&f));
                    }
                    fields.push(f)
                }
                Seg::K(k) if at.is_some() => {
                    let (typ, list) = at.take().expect("matched");
                    v = flush(v, &mut fields);
                    let item = var(&fresh(rc, "Item"));
                    let keys = var(&fresh(rc, "Keys"));
                    let key = var(&fresh(rc, "Key"));
                    pre.extend([
                        Lit::Pos(atom_at("member", vec![v, item.clone()], span)),
                        Lit::Pos(atom_at(
                            "type_list_key",
                            vec![typ, str_term(&list), keys.clone()],
                            span,
                        )),
                        Lit::Eq(keys, Term::List(vec![key.clone()])),
                        Lit::Eq(func("__path", vec![item.clone(), key]), k),
                    ]);
                    v = item;
                }
                Seg::I(i) | Seg::K(i) => {
                    at = None;
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
                    vec![
                        typ.clone(),
                        addr.clone(),
                        str_term(&crate::ir::path_key(p)),
                        value,
                    ],
                    span,
                )),
                Seg::I(_) | Seg::K(_) => None,
            },
            Res::Output {
                inst, key, path, ..
            } if path.is_empty() => Some(atom_at(
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
    /// One element of the keyed list at `(T, A, path)` (R-35, R-69): the
    /// key, the fields below the element, and the owning block.
    Element(Term, Term, String, Term, Vec<String>, Option<String>),
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
/// `${`); an unclosed one is one, for the lowering to report.
fn has_hole(text: &str) -> bool {
    pieces(text).is_none_or(|ps| ps.iter().any(|p| matches!(p, Piece::Hole(..))))
}

/// A string literal with no holes as its value: escapes, and `$${` as
/// `${`.
fn string_value(text: &str) -> Result<String, String> {
    Ok(unescape(text)?.replace("$${", "${"))
}

/// Mark the literals `out[start..]` that test `has r.PATH` by its value
/// with `__has(T, A, "PATH", N)` before them (`not` when `negated`), N
/// their count: the compiler keeps them, or puts the schema's answer in
/// their place (R-106, `partition::answer_has`).
fn mark_has(out: &mut Vec<Lit>, start: usize, marked: Option<Atom>, negated: bool) {
    let Some(mut a) = marked else { return };
    a.args
        .push(Term::Val(Value::Int((out.len() - start) as i64)));
    out.insert(start, if negated { Lit::Not(a) } else { Lit::Pos(a) });
}

/// `has r` of a resource (R-152): `__identity(T, A)`, which the compiler
/// makes a read of the resource's identity (`partition::IDENTITY`).
const IDENTITY: &str = crate::partition::IDENTITY;

/// A path that reads a resource's id: `r.id`.
fn is_identity(path: &[Seg]) -> bool {
    matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
}

/// `has r.PATH` of a resource's attribute (a path of fields):
/// `__has(T, A, "PATH")`, which the compiler answers from the schema in a
/// rule that writes under PATH ([`mark_has`]).
fn has_atom(res: &Res, span: Span) -> Option<Atom> {
    let Res::Ref { typ, addr, path } = res else {
        return None;
    };
    let keys: Vec<String> = path
        .iter()
        .map(|s| match s {
            Seg::F(p) => Some(crate::ir::path_key(p).into_owned()),
            Seg::I(_) | Seg::K(_) => None,
        })
        .collect::<Option<_>>()?;
    (!keys.is_empty()).then(|| {
        atom_at(
            crate::partition::HAS,
            vec![typ.clone(), addr.clone(), str_term(&keys.join("."))],
            span,
        )
    })
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

/// What `set L[k].p.q = v` writes at the list `L`, before the transform
/// lowers it to the core's element write (`transform::ELEM`): an object
/// with the key under `transform::ELEM_KEY` beside the element's content,
/// `{"[key]": k, p: {q: v}}`, so that `types::read` reads a quantity in it
/// at its schema path (`L.p.q`). Content that is not an object literal is
/// held whole under `transform::ELEM_VALUE`.
fn element_write(key: Term, rest: &[String], value: Term) -> Term {
    let content = rest
        .iter()
        .rev()
        .fold(value, |v, k| Term::Obj(BTreeMap::from([(k.clone(), v)])));
    let mut m = match content {
        Term::Obj(m) => m,
        Term::Val(Value::Obj(m)) => m.into_iter().map(|(k, v)| (k, Term::Val(v))).collect(),
        v => BTreeMap::from([(crate::transform::ELEM_VALUE.to_string(), v)]),
    };
    m.insert(crate::transform::ELEM_KEY.to_string(), key);
    Term::Obj(m)
}

/// A constant path as a stored path: `a.b[0].c`, `a."b.c"`.
fn path_string(path: &[Seg]) -> Option<String> {
    let mut out = String::new();
    for s in path {
        match s {
            Seg::F(f) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&crate::ir::path_key(f));
            }
            Seg::I(Term::Val(Value::Int(i))) => out.push_str(&format!("[{i}]")),
            Seg::I(_) | Seg::K(_) => return None,
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
                // A relation's column takes a reference apart (R-42).
                for t in &a.args {
                    if let Term::Func { name, args } = t
                        && name == crate::ir::REF
                    {
                        args.iter().for_each(|t| pattern(t, &mut out));
                    }
                }
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

/// The source names of the lowered variables in `outer`.
fn outer_names(rc: &Rc, outer: &BTreeSet<String>) -> BTreeSet<String> {
    rc.vars
        .iter()
        .filter(|(_, low)| outer.contains(*low))
        .map(|(src, _)| src.clone())
        .collect()
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

/// A piece of a string literal (H-13): the text between holes as written
/// (escapes kept, `$${` read as `${`), or a `${..}` hole's text with its
/// byte offset in the token.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece<'a> {
    Text(String),
    Hole(&'a str, usize),
}

/// The pieces of a string token's text, quotes included: a text before
/// each hole and one after the last. `None` when a hole is never closed.
/// The one interpolation scanner: the lowering, the binding check and
/// `why`'s printer read a string through it.
pub fn pieces(text: &str) -> Option<Vec<Piece<'_>>> {
    scan(text).ok()
}

/// `pieces`, or the byte offset in the token of the `${` that is never
/// closed. A hole runs to its matching `}`, a string in it skipped whole
/// with its own holes (`lexer::hole_end`, R-175).
fn scan(text: &str) -> Result<Vec<Piece<'_>>, usize> {
    let inner = (text.len().checked_sub(1))
        .and_then(|e| text.get(1..e))
        .ok_or(0usize)?;
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // `\u{...}` keeps its braces.
                let end = if bytes.get(i + 1) == Some(&b'u') {
                    inner[i..].find('}').map_or(i + 2, |e| i + e + 1)
                } else {
                    i + 2
                };
                lit.push_str(inner.get(i..end.min(inner.len())).ok_or(i)?);
                i = end;
            }
            b'$' if bytes.get(i + 1) == Some(&b'$') && bytes.get(i + 2) == Some(&b'{') => {
                lit.push_str("${");
                i += 3;
            }
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                let j = crate::lexer::hole_end(bytes, i + 2).ok_or(i + 1)?;
                out.push(Piece::Text(std::mem::take(&mut lit)));
                // +1: the opening quote.
                out.push(Piece::Hole(&inner[i + 2..j - 1], i + 2 + 1));
                i = j;
            }
            _ => {
                let c = inner[i..].chars().next().ok_or(i)?;
                lit.push(c);
                i += c.len_utf8();
            }
        }
    }
    out.push(Piece::Text(lit));
    Ok(out)
}

/// A string literal's value: escapes `\"` `\\` `\n` `\t` `\u{...}`, and
/// `\` at a line end, which joins the line with the next, whose leading
/// whitespace is kept (R-61).
pub fn unescape(lit: &str) -> Result<String, String> {
    let inner = &lit[1..lit.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {}
            Some('\r') if chars.peek() == Some(&'\n') => {
                chars.next();
            }
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
    /// writable), or, `file`, as a program file.
    fn parse_as(src: &str, file: bool) -> anyhow::Result<crate::ast::Program> {
        let file_id = crate::diag::add_source("t.df", src);
        let parse = crate::syntax::parser::parse(src);
        assert!(parse.errors.is_empty(), "{:?}", parse.errors);
        let units = [super::Unit {
            file: file_id,
            root: parse.syntax(),
            path: None,
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
                "let(\"serving\", \"blue\", \"normal\") :- q(1)",
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
               vpc        = vpc\n\
               cidr       = inet.subnet(vpc.cidr, 4, zone_index[z])\n\
               zone       = z\n\
               visibility = \"private\"\n\
             } where data(\"zone\", z)\n",
        );
        assert_eq!(
            got[2],
            "resource \"net.subnet\" Addr { vpc = __ref(\"net.vpc\", \"vpc\", \"\"), \
             cidr = inet.subnet(Cidr, 4, ZoneIndex), zone = Z, visibility = \"private\" } :- \
             data(\"zone\", Z), attr(\"net.vpc\", \"vpc\", \"cidr\", Cidr), \
             zone_index(Z, ZoneIndex), Addr = __segment(str.format(\"private-%s\", Z))"
        );
    }

    /// An entry that is only a path is the pun of its last segment (R-33),
    /// resolved as the value would be: a clause variable, a `let` (read
    /// through its cell), with a rank.
    #[test]
    fn a_bare_entry_is_its_last_segment() {
        let got = lower(
            "let tags = { team: \"x\" }\n\
             resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet \"s-${zone}\" {\n\
               zone\n\
               meta.zone\n\
               tags @default\n\
             } where data(\"zone\", zone)\n",
        );
        assert_eq!(
            got[got.len() - 1],
            "resource \"net.subnet\" Addr { zone = Zone, meta.zone = Zone, tags = Tags } \
             :- data(\"zone\", Zone), tags(Tags), Addr = __segment(str.format(\"s-%s\", Zone))"
        );
        let ranked = parse("let tags = {}\nresource net.vpc v {\n  tags @default\n}\n")
            .unwrap()
            .statements
            .into_iter()
            .any(|s| matches!(s, Stmt::Resource(r) if r.fields.iter().any(|f| f.rank.is_some())));
        assert!(ranked);
        let e = error("resource net.vpc v {\n  a[0]\n}\n");
        assert!(e.contains("`a[0]` names no value"), "{e}");
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
                "resource \"k8s.deployment\" \"a\" { namespace = __ref(\"k8s.namespace\", \"web\", \"name\") } :- ",
                "resource \"k8s.deployment\" \"b\" { namespace = Ns } :- attr(\"k8s.namespace\", \"web\", \"name\", Ns)",
                "p(__ref(\"k8s.namespace\", \"web\", \"name\"), X) :- attr(\"k8s.namespace\", \"web\", \"name\", X)",
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
             let pg = db.pg[\"main\"]\n\
             resource net.vpc v { cidr = pg.net.cidr, name = \"${pg.name}-vpc\" }\n\
             deny \"x\" where pg.size > 3\n",
        );
        assert_eq!(
            &got[1..],
            [
                "let(\"pg\", \"main\", \"normal\")",
                "resource \"net.vpc\" \"v\" { cidr = __ref(\"db.pg\", Pg, \"net.cidr\"), name = \
                 str.format(\"%s-vpc\", Name) } :- pg(Pg), attr(\"db.pg\", Pg, \"name\", Name)",
                "deny(\"x\") :- pg(Pg), attr(\"db.pg\", Pg, \"size\", Size), Size > 3",
            ]
        );
        let e = error(
            "resource db.pg a {}\nlet x = db.pg[\"a\"]\nlet x = 1 where q(1)\np(x.y) where q(1)\n",
        );
        assert!(
            e.contains("`let x` is a db.pg reference in one row and a value in another"),
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
                "let(\"xs\", [1, 2], \"normal\")",
                "let(\"ys\", [{name: \"a\", net: 1}], \"normal\")",
                "q(X) :- xs(Xs), member(Xs, I, Item), X = Item, I >= 0, not member([3], X)",
                // `has` of a resource's attribute is marked for the
                // compiler, which answers it from the schema or the value.
                "ok(1) :- want(\"db.postgres\", \"pg\"), __has(\"db.postgres\", \"pg\", \"public\", 1), attr(\"db.postgres\", \"pg\", \"public\", _), not want(\"db.postgres\", \"other\")",
                "big(N) :- cloud_exists(\"net.vpc\", N), cloud_attr(\"net.vpc\", N, \"size\", Size), Size > 3",
                "pair(N, C) :- ys(Ys), member(Ys, _, Obj), N = __path(Obj, \"name\"), C = __path(Obj, \"net\")",
            ]
        );
    }

    #[test]
    fn modules_instances_and_outputs() {
        let got = lower(
            "component m {\n  input n: int\n  resource net.vpc vpc { size = n }\n  \
             output vpc: net.vpc = vpc\n  output ids: list(ref(net.vpc)) = [vpc]\n}\n\
             resource m a { n = 1 }\n\
             inst(\"a\")\n\
             p(v, s) where inst(i), v = m[i].vpc, s = a.vpc.size\n\
             q(x) where x = a.ids, \"a.vpc\" in net.vpc\n",
        );
        assert_eq!(
            got[0],
            "module m { resource \"net.vpc\" \"vpc\" { size = N } :- n(N); output vpc = None; \
             output vpc = Some(\"\\\"vpc\\\"\"); output ids = None; \
             output ids = Some(\"[__ref(\\\"net.vpc\\\", \\\"vpc\\\", \\\"\\\")]\") }"
        );
        assert_eq!(
            &got[3..],
            [
                "p(V, S) :- inst(I), instance_of(\"m\", \"\", I), output(I, \"vpc\", V), output(\"a\", \"vpc\", Vpc), attr(\"net.vpc\", Vpc, \"size\", S)",
                "q(X) :- output(\"a\", \"ids\", X), want(\"net.vpc\", \"a.vpc\")",
            ]
        );
    }

    #[test]
    fn interpolation_and_lookups() {
        let got = lower(
            "extern file.json(+path, -value)\n\
             p(\"{x} $${x} ${x}%\") where q(x)\n\
             r(v) where v = file.json[\"a.json\"]\n\
             s(y) where q(x), y = \"n-${x}\", \"n-${x}\" in net.route_table\n",
        );
        assert_eq!(
            &got[..],
            [
                "p(str.format(\"{x} $${x} %s%\", X)) :- q(X)",
                "r(V) :- file.json(\"a.json\", V)",
                "s(Y) :- q(X), Y = str.format(\"n-%s\", X), Name = str.format(\"n-%s\", X), want(\"net.route_table\", Name)",
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

    /// R-2: a variable written once joins nothing.
    #[test]
    fn a_variable_written_once_is_an_error() {
        let e = error("reaches(a, c) where reaches(a, b), link(bb, c)\n");
        assert!(
            e.contains("variable `b` is used once; a typo, or `_b`")
                && e.contains("variable `bb` is used once"),
            "{e}"
        );
        // The header, the clause, the entries and the holes count together;
        // a `not { }` body counts once; `_x` opts out.
        lower(
            "tenant(\"t\", 1)\n\
             resource net.vpc \"${t}\" { cidr = \"${n}\" } where tenant(t, n)\n\
             resource net.vpc \"v-${t}\" {} where tenant(t, _n)\n\
             lonely(a) where tenant(a, _), not { tenant(a, z), z > 1 }\n",
        );
        let e = error("tenant(\"t\", 1)\nresource net.vpc \"${t}\" {} where tenant(t, n)\n");
        assert!(e.contains("variable `n` is used once"), "{e}");
        let e = error("tenant(\"t\", 1)\nlonely(a) where tenant(a, _), not { tenant(a, z) }\n");
        assert!(e.contains("variable `z` is used once"), "{e}");
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

    /// A bare header name is the literal name, a value in scope or not; a
    /// name from the clause is a string (R-76).
    #[test]
    fn a_bare_block_name_is_literal() {
        let got = lower(
            "let env = 1\nresource net.vpc env { size = 1 }\nresource net.vpc shared { size = 2 }\n",
        );
        assert_eq!(
            &got[1..],
            [
                "resource \"net.vpc\" \"env\" { size = 1 } :- ",
                "resource \"net.vpc\" \"shared\" { size = 2 } :- ",
            ]
        );
        let e = error("t(\"a\")\nresource net.vpc t {\n size = 1 } where t(t)\n");
        assert!(
            e.contains(
                "`t` is bound by the clause, but a bare header name is the resource's literal name"
            ) && e.contains("`\"${t}\"`"),
            "{e}"
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
            (
                "p(x) where want(net.vpc, x)\n",
                "write `x in net.vpc` (H-15)",
            ),
            (
                "p(v) where attr(net.vpc, \"main\", \"cidr\", v)\n",
                "write `v = net.vpc[\"main\"].cidr` (H-15)",
            ),
            (
                "arg(net.vpc, \"main\", \"cidr\", \"x\")\n",
                "write `set net.vpc[\"main\"].cidr = \"x\"` (H-15)",
            ),
            ("output(\"k\", 1)\n", "write `output k = 1` (H-15)"),
            (
                "deny(\"m\") where q(1)\n",
                "write `deny \"m\" where ..` (H-15)",
            ),
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
