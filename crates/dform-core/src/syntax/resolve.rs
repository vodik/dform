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
    Atom, AttrDecl, BindArg, Config, ExternFn, FieldAssign, FieldOp, Helper, Lit, Program, Rank,
    RuleStmt, Span, Stmt, Term, TypeExpr, Via, str_term, var,
};
use crate::diag::Diagnostic;
use crate::program::node::CopyKind;
use crate::program::node::{
    BinOp, CmpOp, Coll, ExprKind, GoalId, GoalKind, Has, HasMark, ListPart, Negated, ObjPart,
    RelArgs, RelRef,
};
use crate::program::scope::{self as scopes, DeclKind, DeclRef, Names, Reach};
use crate::program::{ExprId, ItemId, ScopeId};
use crate::spell;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

mod aggregate;
mod alias;
mod chain;
use chain::{Chain, Op};
mod columns;
mod diag;
use diag::arg_texts;
pub(crate) use diag::unknown_name;
mod each;
mod function;
mod give;
pub use each::each_var;
mod heads;
mod membership;
mod names;
pub(crate) use names::source_name;
use names::{bare_name, dotted_text, path_parts, word_text};
pub use names::{capitalise, use_parts};
mod pattern;
mod provider;
mod realize;
use crate::program::build::ReadOf;
use realize::{Plan, PlanRead, PlanStep};
mod singleton;
mod spread;
mod strings;
pub use strings::{Piece, pieces, unescape};
use strings::{has_hole, scan, string_value};
mod undeclared;
pub use provider::ENV_VAR;

/// One parsed file of a program.
pub struct Unit {
    /// `diag` source id.
    pub file: u32,
    pub root: SyntaxNode,
    /// The module the file is, by its path (`config`, `modules.net`); `None`
    /// for an entry file, the program's own top level.
    pub path: Option<String>,
    /// The file is a project module (R-114): its resources are of stacks'
    /// types, each a deployment the tool makes.
    pub project: bool,
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
    /// Its outputs, what a keyed read may name (`None`: its file did not
    /// parse, and the read is not checked).
    pub outputs: Option<Vec<String>>,
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
/// its `use`s name. The program carries the nodes it lowered from
/// (`Program::nodes`): what an evaluation lowers again once the schema has
/// read its literals (R-192, [`crate::types::read`]).
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
    let mut roots = l.declare_builtin_externs();
    for &e in entries {
        roots.extend(l.unit(e));
    }
    for (i, u) in units.iter().enumerate() {
        if u.path.is_some() && !entries.contains(&i) {
            roots.extend(l.unit(i));
        }
    }
    let settings = stack.and_then(|st| l.stack_settings(st, entries));
    // The program (R-211), built as the walk went, is what the stack
    // lowers to.
    let mut program = std::mem::take(&mut l.program);
    program.roots = roots;
    program.stack = settings;
    program.diags = std::mem::take(&mut l.diags);
    let result = crate::program::lower(&program);
    result.map(|p| Program {
        nodes: Some(std::rc::Rc::new(program)),
        ..p
    })
}

/// The program's scope tree (R-211, `program::Scopes`) of `units`, `entries`
/// its entry files: what each scope declares and the scopes around it,
/// built as the lowering builds it but with nothing lowered and no error
/// dropping it, for a reader of a program that may not lower (the
/// language server on a broken buffer, a project's stacks and the modules
/// nothing uses yet).
pub fn scopes(units: &[Unit], entries: &[usize], deployed: &[Deployed]) -> crate::program::Scopes {
    Lowerer::new(units, entries, deployed, false).program.scopes
}

/// Remember the order `root` writes objects' fields in
/// (`fmt::value::remember`): each object literal's keys, and a block's
/// entries' keys under each path they share (`metadata.name`,
/// `metadata.labels.app` write `name` before `labels`).
fn field_orders(root: &SyntaxNode) {
    for n in root.descendants() {
        match n.kind() {
            OBJECT => {
                let keys: Vec<String> = n
                    .children()
                    .filter(|c| c.kind() == OBJECT_FIELD)
                    .filter_map(|f| tokens(&f).next())
                    .map(|k| match k.kind() {
                        STRING => string_value(k.text()).unwrap_or_else(|_| k.text().into()),
                        _ => k.text().to_string(),
                    })
                    .collect();
                crate::fmt::value::remember(&keys);
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
                    let segs: Vec<String> = crate::address::path_keys(text.trim());
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
                    crate::fmt::value::remember(&ks);
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
    let mut rc = l.rc(&t, l.root(), &Rc::default());
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
struct Decls {
    /// The stacks the program's `use`s name.
    deployed: Vec<Deployed>,
    /// The project modules' source ids (R-114).
    project: BTreeSet<u32>,
    /// Resource types: every resource header's, every `type` block's, every
    /// `type_*` fact's, and the built-in provider schemas'.
    types: BTreeSet<String>,
    /// Every rule and fact statement `collect` found, with the scope it is
    /// lowered in: what `find_ref_columns` reads.
    rules: Vec<(ScopeId, SyntaxNode)>,
    /// The program relations' reference columns (R-185), by relation and
    /// arity, then index.
    refs: BTreeMap<(columns::RelKey, usize), BTreeMap<usize, columns::RefColumn>>,
    /// First segments of the types: no variable may take one.
    namespaces: BTreeSet<String>,
    /// The namespaces whose every type the compiler knows: the built-in
    /// schemas' but [`OPEN_NAMESPACES`]. Another namespace's type may be a
    /// provider schema's the compiler does not read (`--provider`, a
    /// plugin's).
    closed: BTreeSet<String>,
    /// Each `resource T n` statement as collected, before what `T` names
    /// is known: the scope it declares into, its file and the statement.
    pending: Vec<(ScopeId, u32, SyntaxNode)>,
    /// The `resource C n` statements whose type is a component (R-113):
    /// copies, by file and offset.
    copies: BTreeSet<(u32, u32)>,
    /// Each provider a `use P as A` renames, and its names (R-115):
    /// `ovh` -> `ca`, `eu`. `x in ovh.instance` ranges over `ca.instance`
    /// and `eu.instance` too.
    renamed: BTreeMap<String, BTreeSet<String>>,
    /// The name each provider `use` gives (`use ovh as ca`: `ca`): a
    /// namespace whose types a schema the compiler does not read may name.
    providers: BTreeSet<String>,
}

/// The built-in schemas' namespaces they do not close: a mock of part of a
/// real provider's types (`aws`, `google`; R-36), and `k8s`, whose types
/// are a cluster's own, CRDs included.
const OPEN_NAMESPACES: &[&str] = &["aws", "google", "k8s"];

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
            | SPREAD
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

/// Each provider `use` of the program, as written and by the name it
/// gives (`use P as A`: `P`, `A`).
fn provider_uses(units: &[Unit], deployed: &[Deployed]) -> Vec<(String, String)> {
    units
        .iter()
        .flat_map(|u| u.root.descendants().filter(|n| n.kind() == USE))
        .filter(|n| provider_use(n, units, deployed).is_some())
        .map(|n| use_parts(&n))
        .collect()
}

/// Each provider a `use P as A` of the program renames, and its names
/// (R-115).
fn renamed(uses: &[(String, String)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (of, name) in uses.iter().filter(|(of, name)| of != name) {
        out.entry(of.clone()).or_default().insert(name.clone());
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
/// component's path and the copy's name. A name from the clause
/// (`"agent-${i}"`, R-191) is its text as written, one segment: the scope
/// every copy it names is expanded under before each takes its own.
pub fn copy_parts(n: &SyntaxNode) -> (String, String) {
    let name = header_name(n)
        .map(|t| match t.kind() {
            STRING if has_hole(t.text()) => {
                let text = string_value(t.text()).unwrap_or_else(|_| t.text().to_string());
                crate::address::name_segment(&text).into_owned()
            }
            STRING => string_value(t.text()).unwrap_or_else(|_| t.text().to_string()),
            _ => t.text().to_string(),
        })
        .unwrap_or_default();
    (dotted_text(n, 1), name)
}

/// Whether `n`, a copy's `resource`, takes its name from its clause
/// (`resource node "agent-${i}" { .. } where i in 0..agents`, R-191):
/// it binds no name in its scope; each row of the clause is a copy.
pub fn named_by_clause(n: &SyntaxNode) -> bool {
    n.kind() == RESOURCE && header_name(n).is_some_and(|t| t.kind() == STRING && has_hole(t.text()))
}

/// The name token of a resource header: the word or string after its
/// dotted type.
fn header_name(n: &SyntaxNode) -> Option<SyntaxToken> {
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

/// The module a type names by its path, `types` of `types.environment`
/// or `modules.net` of `ref(modules.net.vpc)`: what the loader loads so
/// the type resolves without a `use` (R-208). `None` for a type of one
/// word.
pub fn type_module(n: &SyntaxNode) -> Option<String> {
    let path = dotted_text(n, 0);
    let (module, _) = path.rsplit_once('.')?;
    (!builtin_type(&path)).then(|| module.to_string())
}

/// A file's outputs, read from its tree without resolving it: the names
/// of its top-level `output` statements (a stack's, for a keyed read).
pub fn output_names(root: &SyntaxNode) -> Vec<String> {
    root.children()
        .filter(|n| n.kind() == OUTPUT_DECL)
        .map(|n| word_text(&n, 1))
        .collect()
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

/// A resolved path segment, an index its node.
#[derive(Clone, Debug)]
enum Seg {
    F(String),
    I(ExprId),
    /// `[k]` with `k` a string or a read (`["api"]`, `[c.name]`): the
    /// element of a keyed list whose key is `k` (R-35); a bare variable or
    /// an integer is the position.
    K(ExprId),
}

/// What a chain denotes, each part the node of what resolved it, the
/// reads it makes held by the node (and made into the resolver's
/// literals where it was resolved).
#[derive(Clone, Debug)]
enum Res {
    /// A value: a variable, a constant, an instance scope.
    Val(ExprId),
    /// A resource reference and a path into it.
    Ref {
        typ: ExprId,
        addr: ExprId,
        path: Vec<Seg>,
    },
    /// An instance output; `typ` when its declared type is a resource
    /// type (it holds that resource's address); `of`, the component and
    /// the scope of the test that the copy `inst` exists, made before the
    /// read when the copy is named by its key.
    Output {
        inst: ExprId,
        key: String,
        path: Vec<Seg>,
        typ: Option<String>,
        of: Option<(String, ExprId)>,
    },
    /// A live object: `world.T[e].path`.
    World {
        typ: String,
        addr: ExprId,
        path: String,
    },
    /// A value name (input or value rule), `k(V)`: what declares it and
    /// how the read reaches it.
    Value {
        decl: DeclRef,
        via: Reach,
        path: Vec<Seg>,
    },
    /// A type's name.
    Type(String),
    /// `p[a, b]`: the relation `p(a, b, V)`, or an extern with its one output.
    Lookup {
        pred: String,
        args: Vec<ExprId>,
        out: usize,
        path: Vec<Seg>,
    },
    /// A variable with fields or indexes.
    Var { var: ExprId, path: Vec<Seg> },
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
    scope: ScopeId,
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
    /// Lowered names an enclosing `for`/`when` binds: what the statement
    /// around binds, which building a clause leaves as it found it.
    outer: BTreeSet<String>,
    /// Lowered names the clause binds where none of its literals shows
    /// it: a `has` walk's value, a negation helper's own variables.
    inner: BTreeSet<String>,
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
    elems: BTreeMap<String, Elem>,
    /// Lowered variable -> the read a `set` target's `[_]` binds it by,
    /// for the target's first use of it (`each_binding`).
    each_reads: BTreeMap<String, ExprId>,
    /// Source variables a reference column types and no `in` does
    /// (R-204): the resource the row holds, its address its own, never a
    /// copy's (`Reach::Absolute`).
    row_refs: BTreeSet<String>,
    /// The tests made before the literal being lowered (a copy named by
    /// its key, a row's type among its column's), each with the copy's
    /// scope a read holding it names: those no read of the literal holds
    /// are goals before its own.
    tests: Vec<(Option<ExprId>, GoalId)>,
}

impl Rc {
    /// The lowered names bound outside the literals written so far: what
    /// the statement around binds and what the clause binds unseen.
    fn bound_around(&self) -> impl Iterator<Item = &String> {
        self.outer.iter().chain(&self.inner)
    }
}

/// What a rule's reads of the relation `name` a read reaches `via` are
/// kept by, once per rule: two relations of one name a body reaches two
/// ways (`k`, `super.k`) are two.
fn read_key(name: &str, via: &Reach) -> String {
    match via {
        Reach::Lexical(body) => format!("{name} of {body}"),
        via => via.relation(name),
    }
}

/// A term and its node.
type Termed = (Term, ExprId);

/// A block's entries as lowered: each, its value's node, and where its
/// reads and helpers end.
type Entries = Vec<(FieldAssign, ExprId, (usize, usize))>;

/// The resource list `in` ranges a variable over (R-69): its resource's
/// type and address and the list's path.
#[derive(Clone)]
struct Elem {
    typ: Termed,
    addr: Termed,
    list: String,
}

/// A rule as the resolver lowered it (`Lowerer::rule_parts`).
struct RuleParts {
    /// The clause gathered of its body.
    clause: Option<crate::program::ClauseId>,
    /// Its head, its arguments' patterns, and with its rank the last
    /// argument.
    head: Atom,
    rel: RelRef,
    args: crate::program::node::RelArgs,
    ranked: Atom,
    rank: Option<Rank>,
    /// Its clause's literals, then the reads the head hoisted, which made
    /// the statement's helpers from `made`.
    body: Vec<Lit>,
    made: usize,
    /// The variables the program wrote, by the name each lowers to.
    written: BTreeSet<String>,
}

/// A document's rows read (`Lowerer::doc_read`): `name(Path, At, ..)`
/// after `Path = LOCATION`, at `span`.
struct DocRead {
    location: ExprId,
    path: String,
    at: String,
    name: String,
    span: Span,
}

/// A relation atom's arguments as their nodes (`Lowerer::atom_args`).
enum AtomArgs {
    Positional(Vec<ExprId>),
    Record(Vec<(String, ExprId)>),
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
    /// In an input's `check`, the name of the value checked (the input,
    /// a field by its field's name): it is its own text, which the check's
    /// readers bind to the value (`refine::split_input`,
    /// `modules::refinement`); every other name reads as in any clause.
    /// `None` reading leniently elsewhere, where every name is its text.
    checked: Option<String>,
    /// The object inputs whose fields are being lowered, outermost first:
    /// each object's path (`db`, `db.pool`) and its fields' names. In a
    /// field's `check` a field of an enclosing object is in scope by its
    /// bare name, the innermost first (the lexical rule): `check hi >= lo`
    /// reads `db.lo`.
    objects: Vec<(String, Vec<String>)>,
    /// The program being built (R-211): an item per statement, pushed in
    /// order as the walk lowers each, and the helper numbers taken so far.
    program: crate::program::Program,
    /// The scope the statements being lowered are in: an entry file's,
    /// a module file's or a component's.
    item_scope: ScopeId,
    /// Each written literal and clause built as goals (R-211 steps 4-5,
    /// `program::build::Gather`), while a ported statement is lowered.
    gather: Option<Box<crate::program::build::Gather>>,
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
    outputs: BTreeSet<(ScopeId, String)>,
    /// The aggregate bindings of the statement being lowered.
    aggs: Vec<aggregate::Agg>,
    /// How deep in `not { }` bodies and comprehensions the lowering is.
    nested: usize,
    /// An object pattern's field reads, appended after the literal that
    /// binds it (`pattern`), and their goals.
    after: Vec<Lit>,
    after_goals: Vec<GoalId>,
    /// The columns of a relation with no `decl`, read from its first
    /// source (R-34).
    read_columns: BTreeMap<String, Vec<BindArg>>,
    /// What picks among the resources a bare name shares, in the value
    /// being lowered (R-74).
    want: Want,
    /// The module whose relations the `use` or `instance` block being
    /// lowered gives rows of (R-204): a row's reference columns are the
    /// module's.
    given: Option<ScopeId>,
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
            decls: Decls::default(),
            file: 0,
            offset: 0,
            diags: Vec::new(),
            lenient,
            checked: None,
            objects: Vec::new(),
            program: crate::program::Program::new(),
            item_scope: Default::default(),
            gather: None,
            helpers: Vec::new(),
            binding: false,
            text: false,
            any_type: false,
            core: false,
            calls: Calls::Function,
            aliases: alias::Aliases::default(),
            outputs: BTreeSet::new(),
            aggs: Vec::new(),
            nested: 0,
            after: Vec::new(),
            after_goals: Vec::new(),
            read_columns: BTreeMap::new(),
            want: Want::Nothing,
            given: None,
        };
        let root = l.program.scopes.root;
        l.item_scope = root;
        l.decls.deployed = deployed.to_vec();
        l.decls.project = units.iter().filter(|u| u.project).map(|u| u.file).collect();
        for (i, u) in units.iter().enumerate() {
            // A module's file is its own scope (R-65), with none around
            // it: a module never reads its user's names (R-205).
            let module = u.path.as_deref().filter(|_| !entries.contains(&i));
            let scope = l.program.scopes.file(u.file, module);
            l.collect(u.file, &u.root, l.program.scopes.declaring(scope), scope);
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
        l.unalias_outputs();
        let uses = provider_uses(units, deployed);
        l.decls.renamed = renamed(&uses);
        l.decls.providers = uses.into_iter().map(|(_, name)| name).collect();
        l.find_ref_columns();
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
        let scopes: Vec<ScopeId> = self.program.scopes.ids().collect();
        for &s in &scopes {
            let names: Vec<String> = self
                .names(s)
                .names(|k| matches!(k, DeclKind::Use { .. }))
                .cloned()
                .collect();
            for name in names {
                for i in 0..self.names(s).of(&name).len() {
                    let DeclKind::Use { written, .. } = &self.names(s).of(&name)[i].kind else {
                        continue;
                    };
                    let path = self.module_path_of(s, &written.clone());
                    if let DeclKind::Use { module, .. } =
                        &mut self.names_mut(s).of_mut(&name)[i].kind
                    {
                        *module = path;
                    }
                }
            }
        }
        self.classify_resources();
        // A copy of a name declared more than once (R-104) makes its
        // component readable by its last segment.
        for &s in &scopes {
            let mut copied = Vec::new();
            for (_, decls) in self.names(s).iter() {
                let bound: Vec<&scopes::Decl> = decls
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.kind,
                            DeclKind::Use { .. } | DeclKind::Copy { binds: true, .. }
                        )
                    })
                    .collect();
                if bound.len() < 2 {
                    continue;
                }
                for d in bound {
                    if let DeclKind::Copy { written, .. } = &d.kind {
                        copied.push((written.clone(), d.node.clone()));
                    }
                }
            }
            for (written, n) in copied {
                let Ok(path) = self.component_path(s, &written) else {
                    continue;
                };
                self.copy_by_last(s, &written, path, n);
            }
        }
        for &s in &scopes {
            let names: Vec<String> = self
                .names(s)
                .names(|k| matches!(k, DeclKind::Copy { .. }))
                .cloned()
                .collect();
            for name in names {
                let Some(d) = self
                    .names(s)
                    .of(&name)
                    .iter()
                    .find(|d| matches!(d.kind, DeclKind::Copy { .. }))
                else {
                    continue;
                };
                let (DeclKind::Copy { written: path, .. }, n) = (d.kind.clone(), d.node.clone())
                else {
                    continue;
                };
                let Ok(full) = self.component_path(s, &path) else {
                    continue;
                };
                // A used module of the name is read by it, and its
                // component's copies by the path (`k3s.k3s[t]`).
                self.copy_by_last(s, &path, full.clone(), n);
                let copy = self
                    .names_mut(s)
                    .of_mut(&name)
                    .iter_mut()
                    .find(|d| matches!(d.kind, DeclKind::Copy { .. }));
                if let Some(scopes::Decl {
                    kind: DeclKind::Copy { component, .. },
                    ..
                }) = copy
                {
                    *component = Some(full);
                }
            }
        }
    }

    /// The component at `path`, copied in `s` by the path `written`,
    /// readable there by its last segment (`postgres[t]`) unless a used
    /// module or stack of that name is read by it.
    fn copy_by_last(&mut self, s: ScopeId, written: &str, path: String, n: SyntaxNode) {
        let last = written.rsplit('.').next().unwrap_or(written);
        if self.use_in(s, last).is_some() || self.stack_in(s, last).is_some() {
            return;
        }
        if self.names(s).copied(last).is_none() {
            self.names_mut(s).declare(last, DeclKind::Copied(path), n);
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
                let sc = self.names(s);
                sc.component(head).is_some() || sc.module(head).is_some()
            });
            let typed = schema_types().contains(&typ) || self.decls.types.contains(&typ);
            // A project module's resource of a stack is a deployment
            // (R-114), a resource of the stack's type.
            let deployment = self.decls.project.contains(&file)
                && self.decls.deployed.iter().any(|d| d.path == full);
            let copy = !deployment
                && (bound || !typed)
                && (self.module_at(&full).is_some()
                    || self.decls.deployed.iter().any(|d| d.path == full)
                    || self.signature(decl, &typ).is_some());
            if copy {
                let (path, name) = copy_parts(&n);
                self.decls
                    .copies
                    .insert((file, n.text_range().start().into()));
                // A copy named by its clause binds no name (R-191): its
                // copies are read `C[t]`, as a provider type's are `T[t]`.
                let kind = DeclKind::Copy {
                    written: path,
                    component: None,
                    binds: !named_by_clause(&n),
                };
                self.names_mut(decl).declare(name, kind, n.clone());
                continue;
            }
            self.decls.types.insert(typ.clone());
            if let Some(name) = self.static_header(&n) {
                self.names_mut(decl)
                    .declare(name, DeclKind::Resource(typ), n.clone());
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
    fn module_path_of(&self, scope: ScopeId, written: &str) -> String {
        let (head, rest) = match written.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (written, None),
        };
        let base = self.chain_of(scope).into_iter().find_map(|s| {
            let sc = self.names(s);
            sc.component(head).or_else(|| sc.module(head)).cloned()
        });
        match (base, rest) {
            (Some(b), Some(r)) => format!("{b}.{r}"),
            (Some(b), None) => b,
            (None, _) => written.to_string(),
        }
    }

    /// The component a path written in `scope` names, or the error that
    /// says what it names instead.
    fn component_path(&self, scope: ScopeId, written: &str) -> Result<String, Box<Diagnostic>> {
        let full = self.module_path_of(scope, written);
        match self.module_at(&full) {
            Some((_, true)) => Ok(full),
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
                    .program
                    .scopes
                    .paths()
                    .filter(|(p, s)| {
                        self.program.scopes.is_component(*s)
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

    /// Record the declarations of a statement list in `decl`; a component
    /// block nests in `outer`.
    fn collect(&mut self, file: u32, parent: &SyntaxNode, decl: ScopeId, outer: ScopeId) {
        let arity = |n: &SyntaxNode| n.children().filter(|c| c.kind() == BIND_ARG).count();
        for n in parent.children() {
            match n.kind() {
                INPUT => {
                    let name = word_text(&n, 1);
                    self.names_mut(decl)
                        .declare(name, DeclKind::Input, n.clone());
                }
                // A let with parameters is a relation (R-187): its
                // parameters' columns, then its value's.
                LET if node(&n, PARAMS).is_some() => {
                    let name = word_text(&n, 1);
                    let arity = function::params(&n).map_or(0, |p| p.len()) + 1;
                    self.names_mut(decl)
                        .declare(name, DeclKind::LetFn { arity }, n.clone());
                }
                LET => {
                    let name = word_text(&n, 1);
                    self.names_mut(decl).declare(name, DeclKind::Let, n.clone());
                }
                // `input p from ..` or `input p`: rows of `p`, which its
                // `decl` declares (R-55).
                INPUT_RELATION => {
                    let name = word_text(&n, 1);
                    let given = terms(&n).next().is_none();
                    self.names_mut(decl).declare(
                        name,
                        DeclKind::RelationInput { given },
                        n.clone(),
                    );
                }
                // `output p` alone: a relation exported (R-55).
                OUTPUT_DECL
                    if node(&n, TYPE_EXPR).is_none()
                        && terms(&n).next().is_none()
                        && node(&n, ATTR_DECL).is_none() =>
                {
                    let k = word_text(&n, 1);
                    self.names_mut(decl)
                        .declare(k, DeclKind::RelationOutput, n.clone());
                }
                OUTPUT_DECL => {
                    let ty = node(&n, TYPE_EXPR).and_then(|t| self.resource_type(decl, &t));
                    let k = word_text(&n, 1);
                    let written = node(&n, TYPE_EXPR)
                        .map(|t| t.text().to_string().replace(char::is_whitespace, ""));
                    let kind = DeclKind::Output { typ: ty, written };
                    self.names_mut(decl).declare(k, kind, n.clone());
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
                    self.program.scopes.externs.insert(name, cols);
                }
                DECL => {
                    let name = dotted_text(&n, 1);
                    let kind = DeclKind::Decl { arity: arity(&n) };
                    self.names_mut(decl).declare(name, kind, n.clone());
                }
                TYPE_DECL => {
                    self.decls.types.insert(dotted_text(&n, 1));
                }
                TYPE_ALIAS if node(&n, SIGNATURE).is_some() => {
                    let name = word_text(&n, 1);
                    let kind = DeclKind::Signature { file };
                    self.names_mut(decl).declare(name, kind, n.clone());
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
                            self.names_mut(decl)
                                .declare(name, DeclKind::Stack(i), n.clone());
                        }
                        None if path.split('.').next() != Some("std") => {
                            let kind = DeclKind::Use {
                                written: path.clone(),
                                module: path,
                            };
                            self.names_mut(decl).declare(name, kind, n.clone());
                        }
                        None => {}
                    }
                }
                COMPONENT => {
                    let start: u32 = n.text_range().start().into();
                    let name = word_text(&n, 1);
                    let path = match self.module_path(file) {
                        Some(m) => format!("{m}.{name}"),
                        None => name.clone(),
                    };
                    let scope = self.program.scopes.component(file, start, outer, &path);
                    self.names_mut(decl)
                        .declare(name, DeclKind::Component(path), n.clone());
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.collect(file, &b, scope, scope);
                    }
                }
                RULE | FACT => {
                    self.decls.rules.push((outer, n.clone()));
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
                        let kind = DeclKind::Rule { arity: n_args };
                        self.names_mut(decl).declare(name.clone(), kind, n.clone());
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

    /// `T` in `output k: T`, written in `scope`, when it names a resource
    /// type: a dotted name that is no type alias (`types.environment` is
    /// one by its module's path, R-208).
    fn resource_type(&self, scope: ScopeId, t: &SyntaxNode) -> Option<String> {
        if node(t, TYPE_EXPR).is_some() {
            return None;
        }
        let name = dotted_text(t, 0);
        (name.contains('.') && !self.names_alias(scope, &name)).then_some(name)
    }

    // --- scopes -------------------------------------------------------------

    /// The scope a statement lowered in `scope` declares into: a file's top
    /// level declares into the program.
    fn decl_scope(&self, scope: ScopeId) -> ScopeId {
        self.program.scopes.declaring(scope)
    }

    /// What `scope` declares.
    fn names(&self, scope: ScopeId) -> &Names {
        &self.program.scopes.names[scope]
    }

    fn names_mut(&mut self, scope: ScopeId) -> &mut Names {
        &mut self.program.scopes.names[scope]
    }

    /// The program's scope.
    fn root(&self) -> ScopeId {
        self.program.scopes.root
    }

    /// The path of the module whose file `file` is (R-65); `None` for an
    /// entry file.
    fn module_path(&self, file: u32) -> Option<String> {
        let scope = self.program.scopes.of_file(file)?;
        match self.program.scopes.definition(scope) {
            Some((path, false)) => Some(path.clone()),
            _ => None,
        }
    }

    /// The top level of `file`, collected.
    fn file_scope(&self, file: u32) -> ScopeId {
        self.program
            .scopes
            .of_file(file)
            .expect("every file is collected")
    }

    /// The body of the component whose statement is at `start` in `file`,
    /// collected.
    fn block_scope(&self, file: u32, start: u32) -> ScopeId {
        let scopes = &self.program.scopes;
        scopes
            .of_block(file, start)
            .expect("every component is collected")
    }

    /// The body of the module or component at `path`, and whether it is a
    /// component's.
    fn module_at(&self, path: &str) -> Option<(ScopeId, bool)> {
        let s = self.program.scopes.by_path(path)?;
        Some((s, self.program.scopes.is_component(s)))
    }

    /// The names a rule at `rc` may read: its variables and what its
    /// scopes declare (inputs, `let`s, resources, copies, components,
    /// modules), for the name an unknown one is nearest.
    fn scope_names(&self, rc: &Rc) -> Vec<String> {
        let mut out: Vec<String> = rc.vars.keys().cloned().collect();
        for s in self.chain_of(rc.scope) {
            let s = self.names(s);
            out.extend(s.names(DeclKind::is_value).cloned());
            out.extend(s.names(|k| matches!(k, DeclKind::Resource(_))).cloned());
            out.extend(s.instances().map(|(n, _)| n.clone()));
            out.extend(s.names(|k| matches!(k, DeclKind::Component(_))).cloned());
            out.extend(s.uses().map(|(n, _)| n.clone()));
        }
        out
    }

    fn chain_of(&self, scope: ScopeId) -> Vec<ScopeId> {
        self.program.scopes.chain(scope)
    }

    /// The scopes from `scope` out to the body it is in ([`Scopes::own`]).
    fn own_scopes(&self, scope: ScopeId) -> Vec<ScopeId> {
        self.program.scopes.own(scope)
    }

    /// An instance's scope `name`, written at `span`, as a read in `scope`
    /// of what `declared` declares reaches it: the name itself when the
    /// copy is the body's own (the copies being instanced put the body's
    /// scope in front of it), else as a scope no copy of the body is taken
    /// from reaches it (`Reach::Absolute`).
    fn scoped(&mut self, scope: ScopeId, declared: ScopeId, name: ExprId, span: Span) -> ExprId {
        match self.own_scopes(scope).contains(&declared) {
            true => name,
            false => {
                let via = Reach::Absolute;
                self.node(span, ExprKind::Scoped { name, via })
            }
        }
    }

    /// The address of the resource `name` in scope, as a read in `scope`
    /// writes it, at `span`: the body's own is relative (expansion scopes
    /// it), its user's reached as written (`scoped`), an enclosing body's
    /// its instance's (`lexical`).
    fn resource_addr(&mut self, scope: ScopeId, name: &str, span: Span) -> ExprId {
        self.resource_addr_from(scope, scope, name, span)
    }

    /// [`Self::resource_addr`] of `name` looked up from `from` outward:
    /// `super.x` looks up from the enclosing scope (R-186).
    fn resource_addr_from(
        &mut self,
        scope: ScopeId,
        from: ScopeId,
        name: &str,
        span: Span,
    ) -> ExprId {
        let declared = self
            .chain_of(from)
            .into_iter()
            .find(|s| self.names(*s).resources(name).is_some())
            .unwrap_or(from);
        let segment = crate::address::name_segment(name);
        let seg = self.node(span, ExprKind::Lit(Value::Str(segment.into_owned())));
        match self.lexical(scope, declared) {
            Some(body) if !body.is_empty() => {
                let via = Reach::Lexical(body);
                self.node(span, ExprKind::Scoped { name: seg, via })
            }
            _ => self.scoped(scope, declared, seg, span),
        }
    }

    /// The definition whose body `declared` is, when a read in `scope`
    /// reaches it past its own body ([`Scopes::lexical`], R-186).
    fn lexical(&self, scope: ScopeId, declared: ScopeId) -> Option<String> {
        self.program.scopes.lexical(scope, declared)
    }

    /// The value `name` read in `scope`, looked up from `from` outward:
    /// the scope that declares it, and how the read reaches it, its own
    /// or an enclosing body's (`lexical`), whose relation is that body's
    /// item marked for expansion ([`Reach::relation`]).
    fn value_decl(&self, scope: ScopeId, from: ScopeId, name: &str) -> (DeclRef, Reach) {
        let declared = self
            .chain_of(from)
            .into_iter()
            .find(|s| self.names(*s).is_value(name));
        let via = match declared.and_then(|d| self.lexical(scope, d)) {
            Some(body) => Reach::Lexical(body),
            None => Reach::Own,
        };
        let decl = DeclRef {
            scope: declared.unwrap_or(from),
            name: name.to_string(),
            at: declared
                .and_then(|d| self.names(d).value_at(name))
                .unwrap_or(0),
        };
        (decl, via)
    }

    /// How a read in `scope` of the relation `p`, looked up from `from`
    /// outward, reaches it: an enclosing body's own relation lexically, as
    /// [`Self::value_decl`] reaches a value; anything else as its own.
    fn relation_reach(&self, scope: ScopeId, from: ScopeId, p: &str) -> Reach {
        if !crate::modules::is_private(p) {
            return Reach::Own;
        }
        let declared = self.chain_of(from).into_iter().find(|s| {
            let sc = self.names(*s);
            sc.is_relation(p) || sc.takes(p)
        });
        match declared.and_then(|d| self.lexical(scope, d)) {
            Some(body) => Reach::Lexical(body),
            None => Reach::Own,
        }
    }

    /// The scope `super` names in `scope` (R-186): the one around the
    /// component the read is in, one level per `super`, the program's for
    /// a component of the stack's file. A module's body has none: a
    /// module never reaches its user.
    /// `outer` counts the `super`s before this one: `super.super.x` past a
    /// component's module is the module's error too.
    fn super_scope(&mut self, scope: ScopeId, item: &str, outer: usize, span: Span) -> L<ScopeId> {
        let body = self.program.scopes.body(scope);
        let module = self
            .program
            .scopes
            .definition(body)
            .map(|(p, c)| (p.clone(), c));
        match module {
            Some((_, true)) => {}
            Some((path, false)) => {
                let help = match self.is_value(body, item) || self.resource(body, item).is_some() {
                    true => format!(
                        "read module {path}'s own as `{}{item}`",
                        "super.".repeat(outer)
                    ),
                    false => format!(
                        "a module never reaches its user: declare `input {item}: TYPE` in it and \
                         give it in the `use`, `use {path} {{ {item} = .. }}`"
                    ),
                };
                let d = Diagnostic::error(
                    span,
                    format!("`super` in module {path}: a module has no scope around it"),
                )
                .with_help(help);
                self.diags.push(d);
                return Err(Skip);
            }
            None => {
                let d = Diagnostic::error(
                    span,
                    "`super` at the stack's top level: it names the scope around a component",
                )
                .with_help(format!("read `{item}` bare"));
                self.diags.push(d);
                return Err(Skip);
            }
        }
        Ok(self.program.scopes.around(body))
    }

    /// The scope a chain's leading `super`s name and the chain after
    /// them (`super.repository.metadata.name`: the module's scope and
    /// `repository.metadata.name`).
    fn super_chain(&mut self, rc: &Rc, c: &Chain, span: Span) -> L<(ScopeId, Chain)> {
        let supers = c
            .ops
            .iter()
            .take_while(|o| matches!(o, Op::Field(f) if f == "super"))
            .count();
        let Some(Op::Field(item)) = c.ops.get(supers) else {
            return self.error(
                span,
                "`super` is the scope around this component: read an item of it, `super.NAME`",
            );
        };
        let mut from = rc.scope;
        for outer in 0..=supers {
            from = self.super_scope(from, item, outer, span)?;
        }
        let rest = Chain {
            head: item.clone(),
            head_kind: IDENT,
            call: None,
            range: c.range,
            ops: c.ops[supers + 1..].to_vec(),
        };
        Ok((from, rest))
    }

    /// `super.x.path` (R-186): what the scope around this component reads
    /// `x` as, a value or a resource, when the component's own `x` shadows
    /// it.
    fn super_read(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>, span: Span) -> L<Res> {
        let (from, rest) = self.super_chain(rc, c, span)?;
        if let Some(r) = self.item_read(rc, from, true, &rest, pre, span)? {
            return Ok(r);
        }
        let (x, around) = (&rest.head, self.scope_name(from));
        self.error(
            span,
            format!("`super.{x}`: {around} has no value or resource `{x}`"),
        )
    }

    /// The value or the resource `c.head` of the scope `from` (and, when
    /// `outward`, the scopes around it), read in `rc.scope` with `c`'s
    /// path: `super.x` and a module's read of itself by its name (R-186).
    /// `None` when it declares neither.
    fn item_read(
        &mut self,
        rc: &mut Rc,
        from: ScopeId,
        outward: bool,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let x = c.head.as_str();
        let here = self.names(from);
        if (outward && self.is_value(from, x)) || here.is_value(x) {
            return self.value(rc, from, c, pre, span).map(Some);
        }
        let types = match outward {
            true => self.resource(from, x),
            false => here.resources(x),
        };
        let Some(types) = types else {
            return Ok(None);
        };
        if types.len() > 1 {
            return self.ambiguous(x, &types, span).map(Some);
        }
        let path = self.segs(rc, &c.ops, pre)?;
        Ok(Some(Res::Ref {
            typ: self.str_node(span, &types[0]),
            addr: self.resource_addr_from(rc.scope, from, x, span),
            path,
        }))
    }

    /// The scope of the module `h` names when the read in `scope` is in
    /// that module's own file and no `use` there binds `h`: `backups` in
    /// backups.df reads the module itself, the instance the read is in.
    fn self_module(&self, scope: ScopeId, h: &str) -> Option<ScopeId> {
        let module = self.module_at(h).filter(|(_, c)| !c)?.0;
        for s in self.chain_of(scope) {
            if self.names(s).module(h).is_some() {
                return None;
            }
            if s == module {
                return Some(s);
            }
        }
        None
    }

    /// A scope as a message names it: `module backups`, `component
    /// backups.volume`, `the stack`.
    fn scope_name(&self, scope: ScopeId) -> String {
        self.program.scopes.describe(scope)
    }

    /// The instance `name` in scope: the scope that declares it and its
    /// component's path.
    fn instance_in(&self, scope: ScopeId, name: &str) -> Option<(ScopeId, String)> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).instance(name).map(|p| (s, p.clone())))
    }

    /// The stack `name` a `use` in scope binds.
    fn stack_in(&self, scope: ScopeId, name: &str) -> Option<Deployed> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.names(s)
                .stack(name)
                .map(|i| self.decls.deployed[i].clone())
        })
    }

    /// The stack a `use` in scope binds that `head.f1.f2..` names by its
    /// module path (`stacks.platform`), and how many of `ops` the path
    /// takes.
    fn stack_by_path(&self, scope: ScopeId, head: &str, ops: &[Op]) -> Option<(Deployed, usize)> {
        let mut at = head.to_string();
        for (i, op) in ops.iter().enumerate() {
            let Op::Field(f) = op else { return None };
            at = format!("{at}.{f}");
            let found = self.chain_of(scope).into_iter().find_map(|s| {
                self.names(s)
                    .stacks()
                    .map(|i| &self.decls.deployed[i])
                    .find(|d| d.path == at)
                    .cloned()
            });
            if let Some(d) = found {
                return Some((d, i + 1));
            }
        }
        None
    }

    /// The module `name` a `use` in scope binds: its path.
    fn use_in(&self, scope: ScopeId, name: &str) -> Option<String> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).module(name).cloned())
    }

    /// The name a `use` in scope binds the module `path` to, when `scope`
    /// is in that module's file and `path` is bound by no `use` of its
    /// own name (`use backups as b`: `b`).
    fn alias_in(&self, scope: ScopeId, path: &str) -> Option<String> {
        let module = self.module_at(path)?.0;
        let chain = self.chain_of(scope);
        if !chain.contains(&module) || self.use_in(scope, path).is_some() {
            return None;
        }
        chain.into_iter().find_map(|s| {
            let mut uses = self.names(s).uses();
            uses.find(|(_, p)| *p == path).map(|(n, _)| n.clone())
        })
    }

    /// The component `name` reads as in scope: its path.
    fn component_in(&self, scope: ScopeId, name: &str) -> Option<String> {
        self.chain_of(scope).into_iter().find_map(|s| {
            let sc = self.names(s);
            sc.component(name).or_else(|| sc.copied(name)).cloned()
        })
    }

    /// Whether `name` is an instance in scope whose component names
    /// nothing: its `instance` statement is the error.
    fn unbound_instance(&self, scope: ScopeId, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.names(s).unbound(name))
    }

    /// A `let`'s rows and the scope that declares it.
    fn find_let(&self, scope: ScopeId, name: &str) -> Option<(ScopeId, Vec<SyntaxNode>)> {
        self.chain_of(scope).into_iter().find_map(|s| {
            let rows: Vec<SyntaxNode> = self
                .names(s)
                .lets(name)
                .filter_map(|n| terms(n).next())
                .collect();
            (!rows.is_empty()).then_some((s, rows))
        })
    }

    /// The input `name` in scope when its type is a resource type, `ref(T)`
    /// or `T` (R-101): `T`.
    fn input_ref(&self, scope: ScopeId, name: &str) -> Option<String> {
        let n = self
            .chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).input(name).cloned())?;
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
    fn value_type(&self, scope: ScopeId, name: &str) -> Result<Option<VType>, String> {
        self.value_type_depth(scope, name, 0)
    }

    fn value_type_depth(
        &self,
        scope: ScopeId,
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
    fn term_vtype(&self, scope: ScopeId, t: &SyntaxNode, depth: usize) -> Option<VType> {
        match declared_ref(t).filter(|typ| !self.names_alias(scope, typ)) {
            Some(typ) => Some(VType::Ref(typ)),
            None => self.written_vtype(scope, t, depth),
        }
    }

    /// The reference a `let` row's term names as written, whatever the
    /// row declares.
    fn written_vtype(&self, scope: ScopeId, t: &SyntaxNode, depth: usize) -> Option<VType> {
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
    fn shared_name(&mut self, scope: ScopeId, c: &Chain, span: Span) -> L<bool> {
        let h = c.head.as_str();
        let Some(types) = self.resource(scope, h) else {
            return Ok(false);
        };
        let field = match c.ops.first() {
            Some(Op::Field(f)) => Some(f.as_str()),
            _ => None,
        };
        let indexed = matches!(c.ops.first(), Some(Op::Index(..) | Op::Keyed(..)));
        let items = |sc: &Names| -> Vec<String> {
            sc.names(DeclKind::is_value)
                .chain(sc.names(|k| matches!(k, DeclKind::Output { .. })))
                .chain(sc.names(|k| matches!(k, DeclKind::Resource(_))))
                .cloned()
                .collect()
        };
        let (what, other) = if let Some(s) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.names(*s).is_value(h))
        {
            let kind = if self.names(s).input(h).is_some() {
                "the input"
            } else {
                "the let"
            };
            (kind.to_string(), None)
        } else if let Some(path) = self.use_in(scope, h) {
            let own = self
                .module_at(&path)
                .map(|(m, _)| items(self.names(m)))
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
                .module_at(&path)
                .map(|(m, _)| {
                    let output = |k: &DeclKind| matches!(k, DeclKind::Output { .. });
                    self.names(m).names(output).cloned().collect()
                })
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
    fn own_wins(&self, scope: ScopeId, name: &str) -> Option<bool> {
        let own = self.own_scopes(scope);
        let names = |s: &Names| {
            s.is_value(name)
                || s.module(name).is_some()
                || s.instance(name).is_some()
                || s.stack(name).is_some()
        };
        let resource = |s: &Names| s.resources(name).is_some();
        let (res_own, other_own) = own.iter().fold((false, false), |(r, o), s| {
            let sc = self.names(*s);
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
    fn names_other(&self, scope: ScopeId, name: &str) -> bool {
        self.is_value(scope, name)
            || self.use_in(scope, name).is_some()
            || self.instance_in(scope, name).is_some()
    }

    fn is_value(&self, scope: ScopeId, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.names(s).is_value(name))
    }

    /// The types a resource name has in scope (the innermost scope that
    /// declares it).
    fn resource(&self, scope: ScopeId, name: &str) -> Option<Vec<String>> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).resources(name))
    }

    fn resource_of_type(&self, scope: ScopeId, typ: &str, name: &str) -> bool {
        self.chain_of(scope).into_iter().any(|s| {
            self.names(s)
                .resources(name)
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

    /// The items of the file `i`: its statements', or, a module's file,
    /// the module it is (R-65); then its doc comments'.
    fn unit(&mut self, i: usize) -> Vec<ItemId> {
        let unit = &self.units[i];
        let (file, root) = (unit.file, unit.root.clone());
        let saved = (self.file, self.item_scope);
        self.file = file;
        let scope = self.file_scope(file);
        let module = self.module_path(file);
        self.item_scope = scope;
        let mut items = Vec::new();
        for n in root.children() {
            match n.kind() {
                // `edition` is a syntax error (R-68).
                ERROR | EDITION => {}
                _ => items.extend(self.stmt(&n, scope, &Rc::default())),
            }
        }
        // A module's file lowers to the module it is (R-65), named by its
        // path.
        if let Some(path) = module {
            let r = root.text_range();
            let span = Span {
                end: u32::from(r.start()),
                ..self.span_of(r)
            };
            items = vec![self.program.module(path, scope, items, span)];
        }
        // Each doc comment is a `doc(Kind, Name, Key, Value)` fact per pair
        // (docs/grammar.md "Doc comments").
        if !self.text && !self.any_type {
            for d in super::doc::collect(&root) {
                if d.pairs.is_empty() {
                    continue;
                }
                let span = self.span_of(d.range);
                let (kind, name, pairs) = (d.kind, d.name, d.pairs);
                let kind = crate::program::ItemKind::Doc { kind, name, pairs };
                items.push(self.program.item(span, self.item_scope, kind));
            }
        }
        (self.file, self.item_scope) = saved;
        items
    }

    fn stmts(&mut self, block: Option<SyntaxNode>, scope: ScopeId, outer: &Rc) -> Vec<ItemId> {
        let Some(block) = block else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for n in block.children().filter(|n| n.kind() != ERROR) {
            out.extend(self.stmt(&n, scope, outer));
        }
        out
    }

    /// A statement's item: what it lowers to, with the helper rules it
    /// generated after it. `outer` carries the variables an enclosing
    /// `for`/`when` binds.
    fn stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Option<ItemId> {
        if n.kind() == COMPONENT {
            return Some(self.component(n, scope, outer));
        }
        let gathered = self.gather.as_mut().map(|g| g.begin());
        let item = self.stmt_item(n, scope, outer);
        if let (Some(g), Some(outer)) = (&mut self.gather, gathered) {
            g.resume(outer);
        }
        item
    }

    /// [`Self::stmt`], the statement's variables and clause gathered on
    /// their own.
    fn stmt_item(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Option<ItemId> {
        if let Some(value) = self.literal_let(n) {
            return self.let_literal(n, scope, &value);
        }
        match n.kind() {
            LET if node(n, PARAMS).is_none() => self.ported(n, |l| l.let_stmt(n, scope, outer)),
            LET => self.ported(n, |l| l.function_stmt(n, scope, outer)),
            RULE | FACT => self.ported(n, |l| l.rule(n, scope, outer)),
            CHECK => self.ported(n, |l| l.check(n, scope, outer)),
            OUTPUT_DECL => self.ported(n, |l| l.output(n, scope, outer)),
            INPUT => self.ported(n, |l| l.input(n, scope, outer)),
            DECL => Some(self.decl(n, scope)),
            EXTERN => self.extern_item(n).ok(),
            TYPE_DECL => self.ported(n, |l| l.type_block(n, scope)),
            USE if provider_use(n, self.units, &self.decls.deployed).is_some() => {
                self.ported(n, |l| {
                    l.redeclared(n, &use_parts(n).1)?;
                    l.provider(n, scope, outer)
                })
            }
            SET => self.ported(n, |l| l.set(n, scope, outer)),
            INPUT_RELATION => self.ported(n, |l| l.relation_input(n, scope, outer)),
            RESOURCE if self.decls.project.contains(&self.file) => {
                self.ported(n, |l| l.deployment(n, scope, outer))
            }
            RESOURCE if !self.is_copy(n) => self.ported(n, |l| l.block_stmt(n, scope, outer)),
            RESOURCE => self.ported(n, |l| l.instance(n, scope, outer)),
            USE => self.ported_maybe(n, |l| l.use_stmt(n, scope, outer)),
            // An alias lowers to nothing: each use is its type.
            TYPE_ALIAS => None,
            k => {
                let _: L<()> = self.error(self.span(n), format!("unexpected {k:?}"));
                None
            }
        }
    }

    /// A ported statement's item (R-211 step 5), built while its body's
    /// goals are gathered, with the helper rules and aggregates of its
    /// own; checked for `_` as what it lowers to.
    fn ported(
        &mut self,
        n: &SyntaxNode,
        build: impl FnOnce(&mut Self) -> L<ItemId>,
    ) -> Option<ItemId> {
        self.ported_maybe(n, |l| build(l).map(Some))
    }

    /// [`Self::ported`] of a statement that may lower to nothing (`use std.x`,
    /// `use` of a stack's deployments).
    fn ported_maybe(
        &mut self,
        n: &SyntaxNode,
        build: impl FnOnce(&mut Self) -> L<Option<ItemId>>,
    ) -> Option<ItemId> {
        let saved = std::mem::take(&mut self.helpers);
        let aggs = std::mem::take(&mut self.aggs);
        let own = self.gather.is_none();
        if own {
            self.gather = Some(Box::default());
        }
        let item = build(self);
        if own {
            self.gather = None;
        }
        self.aggs = aggs;
        self.helpers = saved;
        let item = item.ok()??;
        if !(self.core || self.lenient || self.text || self.any_type) {
            let stmts = crate::program::lower_item(&self.program, item);
            self.placeholders(&stmts, self.span(n)).ok()?;
        }
        Some(item)
    }

    /// `component NAME [: S] { .. }`: its statements' items in a scope of
    /// its own.
    fn component(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> ItemId {
        let span = self.span(n);
        let start: u32 = n.text_range().start().into();
        let inner = self.block_scope(self.file, start);
        let name = word_text(n, 1);
        let path = self
            .names(self.decl_scope(scope))
            .component(&name)
            .cloned()
            .unwrap_or(name);
        if let Some(t) = node(n, TYPE_EXPR) {
            self.check_signature(n, &t, scope);
        }
        let saved = std::mem::replace(&mut self.item_scope, inner);
        let body = self.stmts(node(n, STMT_BLOCK), inner, outer);
        let item = self.program.module(path, inner, body, span);
        self.item_scope = saved;
        item
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
        let bad = |l: &mut Self, at: Span, msg: String, fix: String| {
            l.diags.push(Diagnostic::error(at, msg).with_help(fix));
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
                            format!("give `{}` a value, or leave its line out", f.key),
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
                            format!("give `{k}` a value, or leave it out for its default"),
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
                            format!("write its value, `output {} = ..`", o.name),
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
                    let p = h.pred.rsplit("::").next().unwrap_or(&h.pred);
                    bad(
                        self,
                        h.span,
                        format!("`_` is `{p}`'s column {c}, which then has no value"),
                        format!(
                            "name the column and give it a value after `where`: `{p}(x) \
                             where .., x in ..`"
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
                    "`_` matches anything and holds nothing: name what it stands for (`x`, \
                     `env.p`)"
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
    fn rc(&self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Rc {
        let mut rc = Rc {
            scope,
            vars: outer.vars.clone(),
            types: outer.types.clone(),
            instances: outer.instances.clone(),
            row_refs: outer.row_refs.clone(),
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
        for c in n
            .descendants()
            .filter(|c| matches!(c.kind(), LIT_IN | LIT_NOT_IN))
        {
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
            // `r not in T`, `r` a reference column's (R-42): the column
            // binds its type, which the test says is not `T` (R-219).
            let negated = c.kind() == LIT_NOT_IN || c.parent().is_some_and(|p| p.kind() == LIT_NOT);
            if negated && Self::ref_bound(&c, &lhs.head) {
                let typed = terms(&c)
                    .nth(1)
                    .and_then(|t| Chain::of(&t))
                    .is_some_and(|r| {
                        let r = self.type_each(&rc, r);
                        self.chain_type(&rc, &r).is_some() || self.namespace_of(&rc, &r).is_some()
                    });
                if typed {
                    let tv = fresh(&mut rc, "Type");
                    rc.types.insert(lhs.head, var(&tv));
                    continue;
                }
            }
            if c.kind() == LIT_NOT_IN {
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
        self.ref_column_vars(n, scope, &mut rc);
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
            || self.module_at(&c.head).is_some()
            || rc.candidates.contains(&c.head);
        // `T.n.path` is a read of a resource, not a type.
        let fields = c.fields();
        let resource = (1..fields.len())
            .any(|i| self.resource_of_type(rc.scope, &fields[..i].join("."), &fields[i]));
        // A namespace no declaration or builtin schema names is a provider
        // schema's the compiler does not read (`--provider`, a plugin's).
        let namespace = self.decls.namespaces.contains(&c.head)
            || self.any_type
            || !self.program.scopes.declares_relation(&c.head);
        (!local && !resource && namespace && !c.ops.is_empty()).then_some(name)
    }

    /// `key NAME` in the module `path`, a file the program loads, not the
    /// one the tool runs (R-208): the error at its line, its help the
    /// input to declare (the key's own type text) and the `use` to give it
    /// in, labelled where the module is used.
    fn key_in_module<T>(&mut self, n: &SyntaxNode, name: &str, path: &str, span: Span) -> L<T> {
        let ty = node(n, TYPE_EXPR).map_or_else(|| "TYPE".to_string(), |t| t.text().to_string());
        let site = self.program.scopes.names.values().find_map(|sc| {
            let used = sc.uses().any(|(_, m)| m == path);
            sc.iter().flat_map(|(_, ds)| ds).find_map(|d| {
                let is_use = matches!(d.kind, DeclKind::Use { .. });
                (is_use && used && use_parts(&d.node).0 == path).then(|| d.node.clone())
            })
        });
        let used = site.as_ref().map_or_else(
            || path.rsplit('.').next().unwrap_or(path).to_string(),
            |u| use_parts(u).1,
        );
        let shown = match used == path.rsplit('.').next().unwrap_or(path) {
            true => path.to_string(),
            false => format!("{path} as {used}"),
        };
        let mut d = Diagnostic::error(
            span,
            format!(
                "`key {name}` in a file that is not an entrypoint: a key is a deployment's \
                 identity; a module takes `input {name}` instead"
            ),
        )
        .with_help(format!(
            "declare `input {name}: {ty}` here, and give it in the use: `use {shown} {{ {name} }}`"
        ));
        if let Some(u) = &site
            && let Some(file) = self.file_of_node(u)
        {
            d = d.with_label(
                Span {
                    file,
                    start: u.text_range().start().into(),
                    end: u.text_range().end().into(),
                    origin: 0,
                },
                format!("{path} is used here, not deployed"),
            );
        }
        self.diags.push(d);
        Err(Skip)
    }

    /// The `diag` source id of the file a node of the program is in.
    fn file_of_node(&self, n: &SyntaxNode) -> Option<u32> {
        let root = n.ancestors().last()?;
        self.units.iter().find(|u| u.root == root).map(|u| u.file)
    }

    /// `input k: T [= d] [check B] [where G]`, `key k: T`, `input k { f:
    /// T .. }` built as an `Input` item (R-211 step 5): its default read
    /// as its type, its refinement's and its clause's goals, its fields
    /// items of their own; `lower` writes the declaration.
    fn input(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        // `input k { f: T [= d] [check B] .. }` (R-54).
        let fields = self.input_fields(n, scope, outer, &name)?;
        let ty = match node(n, TYPE_EXPR) {
            Some(t) => self.input_type(&t),
            None => self.fields_type(&fields),
        };
        let key = is_key(n);
        if key && let Some(path) = self.module_path(self.file) {
            return self.key_in_module(n, &name, &path, span);
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
        let default_node = match terms(n).next() {
            // A literal default is read as the declared type (R-31).
            Some(t) => match self.typed_constant(&mut rc, &t, &ty)? {
                Ok(d) => Some(d),
                Err(why) => {
                    return self.error(self.span(&t), format!("input {name} {why}"));
                }
            },
            None => None,
        };
        self.refinement(n, scope, Some(&name))?;
        let refined = self.clause_at(node(n, REFINEMENT).and_then(|r| node(&r, BODY)));
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
                format!(
                    "the clause of input {name} reads {name}: a clause picks where it is declared"
                ),
            )
            .with_help(format!(
                "a refinement is spelled `check` (R-1): `{} check {body}`",
                head[..at].trim()
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        let guard = self.clauses(&mut rc, n)?;
        // An input is declared where its clause holds, which every read of
        // it repeats: a clause binds no aggregate there, as a resource's
        // does not; a `let` binds it, read here.
        self.aggregate_in(&guard, span)?;
        let guarded = self.clause_at(node(n, CLAUSE));
        if key && !guard.is_empty() {
            return self.error(
                span,
                format!(
                    "key {name} has a clause: a key names the deployment, so every \
                             deployment has it"
                ),
            );
        }
        let rows = self.inputs_named(n, &name, span)?;
        let i = crate::program::build::InputLowered {
            name,
            key,
            ty,
            default: default_node,
            refinement: refined,
            guard: guarded,
            fields,
            rows,
            span,
            scope: self.item_scope,
        };
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &|_| true);
        let item = b.input_item(i);
        gather.done(b);
        Ok(item)
    }

    /// The object type of an input's fields.
    fn fields_type(&self, fields: &[ItemId]) -> TypeExpr {
        let field = |&f: &ItemId| match &self.program.items[f].kind {
            crate::program::ItemKind::Input { name, ty, .. } => (name.clone(), ty.clone()),
            _ => unreachable!("a field is an input"),
        };
        TypeExpr::Object(fields.iter().map(field).collect())
    }

    /// The clause of the body `at` just lowered, gathered (empty when no
    /// literal is written in it); none when there is no body, or nothing
    /// gathers.
    fn clause_at(&mut self, at: Option<SyntaxNode>) -> Option<crate::program::ClauseId> {
        let span = self.span(&at?);
        let gather = self.gather.as_mut()?;
        Some(gather.body_clause(&mut self.program, span))
    }

    /// The fields of an object input's block form (R-54), each a
    /// declaration named by its field: `f: T [= d] [check B]`, a nested
    /// object `f: { .. }`. A field's default is read as its type (R-31).
    /// `path` is the object's (`db`, `db.pool`); its fields are in scope by
    /// their bare names in each field's check, as the enclosing objects'.
    fn input_fields(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        path: &str,
    ) -> L<Vec<ItemId>> {
        let names = n
            .children()
            .filter(|c| c.kind() == ATTR_DECL)
            .filter_map(|a| node(&a, BLOCK_PATH))
            .map(|p| p.text().to_string())
            .collect();
        self.objects.push((path.to_string(), names));
        let r = self.field_decls(n, scope, outer, path);
        self.objects.pop();
        r
    }

    /// The fields of `input_fields`, with the object's in scope.
    fn field_decls(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        object: &str,
    ) -> L<Vec<ItemId>> {
        let mut out: Vec<(String, Span, ItemId)> = Vec::new();
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
                if let Some((_, first, _)) = out.iter().find(|(f, ..)| *f == name) {
                    let d = Diagnostic::error(span, format!("field {name} is declared twice"))
                        .with_label(*first, "first here");
                    self.diags.push(d);
                    return Err(Skip);
                }
                let fields = self.input_fields(&a, scope, outer, &format!("{object}.{name}"))?;
                let ty = match node(&a, TYPE_EXPR) {
                    Some(t) => self.input_type(&t),
                    None => self.fields_type(&fields),
                };
                let mut rc = self.rc(&a, scope, outer);
                let default = match terms(&a).next() {
                    Some(t) => match self.typed_constant(&mut rc, &t, &ty)? {
                        Ok(d) => Some(d),
                        Err(why) => {
                            return self.error(self.span(&t), format!("field {name} {why}"));
                        }
                    },
                    None => None,
                };
                self.refinement(&a, scope, Some(&name))?;
                let refinement = self.clause_at(node(&a, REFINEMENT).and_then(|r| node(&r, BODY)));
                let i = crate::program::build::InputLowered {
                    name: name.clone(),
                    key: false,
                    ty,
                    default,
                    refinement,
                    guard: None,
                    fields,
                    rows: None,
                    span,
                    scope: self.item_scope,
                };
                let gather = self
                    .gather
                    .as_mut()
                    .expect("a ported statement is gathered");
                let mut b = gather.builder(&mut self.program, span, &|_| true);
                let item = b.input_item(i);
                gather.done(b);
                Ok((name, span, item))
            })();
            match r {
                Ok(f) => out.push(f),
                Err(Skip) => failed = true,
            }
        }
        match failed {
            true => Err(Skip),
            false => Ok(out.into_iter().map(|(.., f)| f).collect()),
        }
    }

    // --- declarations that lower to themselves ----------------------------

    /// `extern f(+a: T, -b)`: a relation a provider answers, its columns
    /// as declared (R-211 step 5: an item carrying them).
    fn extern_item(&mut self, n: &SyntaxNode) -> L<ItemId> {
        let span = self.span(n);
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
            let d = Diagnostic::error(at, format!("extern {name}: `persist` is gone")).with_help(
                "keep an answer where it is read: `memo.first(KEY, CANDIDATE, VALUE)` keeps the \
                 first candidate given for KEY",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let kind = crate::program::ItemKind::Extern { name, args };
        Ok(self.program.item(span, self.item_scope, kind))
    }

    /// `type T { p: T flag* }`: a type block, its attributes as read
    /// (R-211 step 5: an item carrying them).
    fn type_block(&mut self, n: &SyntaxNode, scope: ScopeId) -> L<ItemId> {
        let name = dotted_text(n, 1);
        let attrs = self.attr_decls(n, scope)?;
        let kind = crate::program::ItemKind::TypeBlock { name, attrs };
        let item = self.program.item(self.span(n), self.item_scope, kind);
        // The helpers its refinements made (a `not`'s rule) follow it.
        if !self.helpers.is_empty() {
            let made = std::mem::take(&mut self.helpers);
            self.program
                .terms_made
                .insert(crate::program::NodeId::Item(item), made);
        }
        Ok(item)
    }

    /// `decl p(a, b)` declares the relation `p/2` by its columns (H-11):
    /// one no rule of the program defines is fed from outside (a provider,
    /// a given fact); `decl p(a, b) mixed` lets it have both facts and
    /// rules. The column names are the named-argument form's.
    fn decl(&mut self, n: &SyntaxNode, scope: ScopeId) -> ItemId {
        let pred = dotted_text(n, 1);
        let binds: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == BIND_ARG).collect();
        let columns = binds
            .iter()
            .map(|b| {
                (
                    word_text(b, 0),
                    node(b, TYPE_EXPR).map(|t| self.type_expr(&t)),
                )
            })
            .collect();
        let mixed = tokens(n).last().is_some_and(|t| t.text() == "mixed");
        // A relation the scope's own statements do not define: one in
        // another module of the same name is another relation (R-65).
        let fed = !self.names(self.decl_scope(scope)).defines(&pred);
        let span = self.span(n);
        let rel = crate::program::node::RelRef::own(pred, span);
        let kind = crate::program::ItemKind::Decl {
            rel,
            columns,
            mixed,
            fed,
        };
        self.program.item(span, self.item_scope, kind)
    }

    /// `input p from TERM [where B]` (R-55): rows of the relation `p`, its
    /// columns `decl p(..)`'s. Several lines are one relation, their rows
    /// together, and facts the program states join them. `input p` alone,
    /// in a module or a component, is a relation its user gives the rows
    /// of, in the `use` or `instance` block.
    fn relation_input(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let pred = word_text(n, 1);
        let module = self.module_path(self.file).is_some()
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
            let r = crate::program::build::RelationInputLowered {
                span,
                scope: self.item_scope,
                rel: pred,
                arity,
                mixed: false,
                from: None,
            };
            let mut b = crate::program::Builder::new(&mut self.program, span, &|_| true);
            return Ok(b.relation_input_item(r));
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
        let (cols, arity) = match self.relation_decl(scope, &pred) {
            // No `decl`: the columns are the first source's (R-34).
            None => {
                let cols = match self.read_columns.get(&pred) {
                    Some(c) => c.clone(),
                    None => {
                        let c = self.first_source_columns(&pred, n, &source, span)?;
                        self.read_columns.insert(pred.clone(), c.clone());
                        c
                    }
                };
                let arity = cols.len();
                (cols, arity)
            }
            Some(decl) => {
                let cols = self.table_columns(&pred, &decl)?;
                (
                    cols,
                    decl.children().filter(|c| c.kind() == BIND_ARG).count(),
                )
            }
        };
        self.table_item(n, scope, outer, (pred, arity, true), cols)
    }

    /// `p from DOC [where B]`'s rows (R-39), its columns `cols`, as a
    /// `RelationInput` item: a stack's own relation (`mixed`), or the rows
    /// a copy's block gives its module's relation.
    fn table_item(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        (pred, arity, mixed): (String, usize, bool),
        cols: Vec<BindArg>,
    ) -> L<ItemId> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let rows = self.table(&mut rc, &pred, cols, n, body, span)?;
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let r = crate::program::build::RelationInputLowered {
            span,
            scope: self.item_scope,
            rel: pred,
            arity,
            mixed,
            from: Some(crate::program::build::RelationRows {
                clause,
                externs: rows.externs.clone(),
                head: &rows.head,
                body: &rows.body,
                rows: rows.rows,
                made: &self.helpers[rows.made..],
                results: &results,
            }),
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.relation_input_item(r);
        gather.done(b);
        Ok(item)
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
    fn relation_decl(&self, scope: ScopeId, pred: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).decl(pred).cloned())
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

    fn attr_decls(&mut self, n: &SyntaxNode, scope: ScopeId) -> L<Vec<AttrDecl>> {
        let mut out = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let path = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            let ty = node(&a, TYPE_EXPR).map(|t| self.type_expr(&t));
            let flags = tokens(&a)
                .filter(|t| t.kind() == IDENT)
                .map(|t| t.text().to_string())
                .collect();
            let refinement = self.refinement(&a, scope, None)?;
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

    /// A refinement's `check` body. An input's (`checked` its name, a
    /// field's name) is a clause like any other: the value checked is its
    /// name's text, every other name in scope reads as it would anywhere
    /// (a sibling input, an object's field by its path, a `let`). A type
    /// block's names are their own text (its attributes).
    fn refinement(&mut self, n: &SyntaxNode, scope: ScopeId, checked: Option<&str>) -> L<Vec<Lit>> {
        let Some(b) = node(n, REFINEMENT).and_then(|w| node(&w, BODY)) else {
            return Ok(Vec::new());
        };
        let saved = (self.lenient, self.checked.take());
        self.lenient = true;
        self.checked = checked.map(str::to_string);
        let mut rc = Rc {
            scope,
            ..Rc::default()
        };
        let r = self.body(&mut rc, &b);
        (self.lenient, self.checked) = saved;
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
                let module = self.module_at(ns).is_some()
                    || self
                        .program
                        .scopes
                        .names
                        .values()
                        .any(|s| s.module(ns).is_some() || s.component(ns).is_some());
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
                    // R-180: a range of addresses is a range of ips.
                    if name == "iprange" && aliases && self.alias(n, &name).is_none() {
                        let d = Diagnostic::error(self.span(n), "unknown type iprange").with_help(
                            "a range of addresses is `range(ip)`: `let r: range(ip) = \
                                 \"10.0.0.10..=10.0.0.99\"`",
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
                    // `range(T)` for an ordered `T` (R-180).
                    if name == "range"
                        && !matches!(args.as_slice(), [TypeExpr::Name(t)]
                            if crate::range::ORDERED.contains(&t.as_str()))
                    {
                        let d = Diagnostic::error(
                            self.span(n),
                            format!("`{}` is no range type", n.text()),
                        )
                        .with_help(format!(
                            "a range is of one ordered type: `range(T)`, `T` one of {}",
                            crate::range::ORDERED.join(", ")
                        ));
                        self.diags.push(d);
                    }
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
                STRING => out.push_str(&crate::address::path_key(&self.string(&t)?)),
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
        let id = self.constant_node(rc, n)?;
        Ok(self.term_of(id))
    }

    /// The constant `t` read as the type `ty` (R-31): its node, or why it
    /// is not one.
    fn typed_constant(
        &mut self,
        rc: &mut Rc,
        t: &SyntaxNode,
        ty: &TypeExpr,
    ) -> L<Result<ExprId, String>> {
        let id = self.constant_node(rc, t)?;
        let was = self.term_of(id);
        Ok(
            crate::types::literal(&crate::types::of_expr(ty), was.clone())
                .map(|now| self.retyped(rc, id, &was, now)),
        )
    }

    /// [`Self::constant`] as its node.
    fn constant_node(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<ExprId> {
        let mut pre = Vec::new();
        let id = self.expr(rc, n, Pos::Whole, &mut pre)?;
        let t = self.lowered(id, &mut pre);
        if !pre.is_empty() {
            return self.error(
                self.span(n),
                "this value is read before the program runs: it must be a constant",
            );
        }
        self.check_bound(rc, &[], &[&t])?;
        Ok(id)
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
    ) -> L<TableRows> {
        let vars: Vec<Term> = cols
            .iter()
            .map(|c| var(&fresh(rc, &capitalise(&c.name))))
            .collect();
        let made = self.helpers.len();
        let (externs, rows) = self.doc_source(rc, n, pred, cols, vars.clone(), &mut body)?;
        self.check_bound(rc, &body, &[])?;
        Ok(TableRows {
            externs,
            head: atom_at(pred, vars, span),
            body,
            rows,
            made,
        })
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
                         a secret comes from a secret input, a data source's secret column or a std function such as `random.password`",
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
        let scope = self.file_scope(self.units[first].file);
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
    ) -> L<(Vec<Stmt>, GoalId)> {
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
        let location = self.expr(rc, &src, Pos::Content, body)?;
        let path = fresh(rc, "Doc");
        let at = fresh(rc, "At");
        let ext = crate::tables::extern_name(crate::tables::VALUE, &name);
        let span = self.span(&src);
        let read = DocRead {
            location,
            path,
            at,
            name: ext.clone(),
            span,
        };
        let rows = self.rows_goal(rc, &read, &outs, body);
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
        let decl = Stmt::ExternFn(ExternFn {
            name: ext,
            args: ins,
            span,
        });
        Ok((vec![decl], rows))
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
    fn loader_call(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> Option<L<ExprId>> {
        if let Some(e) = self.old_loader(n) {
            return Some(e.map(|never| match never {}));
        }
        let (format, _) = self.loader_of(n)?;
        let table = crate::tables::DOCUMENT.to_string();
        let v = fresh(rc, &capitalise(&format));
        let cols = vec![BindArg {
            input: false,
            name: "value".into(),
            ty: Some(TypeExpr::Name("any".into())),
        }];
        let (stmts, read) = match self.doc_read(rc, n, &table, cols, pre) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        for st in stmts {
            let known = |h: &Stmt| matches!((h, &st), (Stmt::ExternFn(a), Stmt::ExternFn(b)) if a.name == b.name);
            if !self.helpers.iter().any(known) {
                self.helpers.push(st);
            }
        }
        // `table.FORMAT.document(Path, At, V)` after `Path = LOCATION`.
        Some(Ok(self.building(rc, read.span, |b| {
            let path = b.bound_into(read.location, &read.path);
            let at = b.variable(&read.at);
            b.call_into((&read.name, Reach::Own), vec![path, at], &v)
        })))
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
    fn old_loader(&mut self, n: &SyntaxNode) -> Option<L<std::convert::Infallible>> {
        if n.kind() != CALL {
            return None;
        }
        let name = self.callee(n)?;
        if !crate::tables::FORMATS.contains(&name.as_str())
            || self.program.scopes.declares_relation(&name)
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
        // `secrets(..)` never was a loader: the given secrets' file is
        // read as every document is (R-108).
        let (headline, help) = match name.as_str() {
            "secrets" => (
                "a file of given secrets is read by `secrets.decode(io.read(..))`".into(),
                format!(
                    "`{new}`: the file `dform secrets set` writes, opened with the deployment's key"
                ),
            ),
            _ => (
                format!("`{name}(..)` is gone (R-155): a location is read by `io.read`"),
                format!(
                    "`{new}`: `io.read` reads the text, a format's `decode` the document in it, \
                     its rows at their lines"
                ),
            ),
        };
        let d = Diagnostic::error(self.span(n), headline).with_help(help);
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
    ) -> L<(Vec<Stmt>, GoalId)> {
        let (stmts, read) = self.doc_read(rc, src, table, cols, body)?;
        let rows = self.rows_goal(rc, &read, &outs, body);
        Ok((stmts, rows))
    }

    /// The rows of a document `read` reads into `outs`, `name(Path, At,
    /// outs..)` after `Path = LOCATION`, into `body`: their goal.
    fn rows_goal(&mut self, rc: &Rc, read: &DocRead, outs: &[Term], body: &mut Vec<Lit>) -> GoalId {
        let t = self.lowered(read.location, body);
        body.push(Lit::Eq(var(&read.path), t));
        let mut args = vec![var(&read.path), var(&read.at)];
        args.extend(outs.iter().cloned());
        body.push(Lit::Pos(atom_at(&read.name, args, read.span)));
        let rel = RelRef::own(read.name.clone(), read.span);
        self.building(rc, read.span, |b| {
            let path = b.bound_into(read.location, &read.path);
            let mut args = vec![path, b.variable(&read.at)];
            for o in outs {
                let Term::Var(v) = o else {
                    unreachable!("a document's column is read into a variable")
                };
                args.push(b.variable(v));
            }
            let args = RelArgs::Positional(args.into_iter().map(|a| b.pattern_of(a)).collect());
            b.goal_node(read.span, GoalKind::Rel { rel, args })
        })
    }

    /// The read of a document's rows [`Self::table_body`] makes: the
    /// extern declared for it, its location's node (the reads no node of
    /// it holds into `pre`), the variables it is read into and the
    /// extern's name.
    fn doc_read(
        &mut self,
        rc: &mut Rc,
        src: &SyntaxNode,
        table: &str,
        cols: Vec<BindArg>,
        pre: &mut Vec<Lit>,
    ) -> L<(Vec<Stmt>, DocRead)> {
        let span = self.span(src);
        let bad = |l: &mut Self, what: &str| -> L<(Vec<Stmt>, DocRead)> {
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
            return e.and_then(|never| match never {});
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
            .with_help(
                "`io.read(\"git+https://HOST/OWNER/REPO/PATH?ref=TAG\")` (`git+ssh://` over \
                 ssh, `git+file:REPO/PATH?ref=TAG` for a repository in the project), read at \
                 the commit the ref names, which the plan file records",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let location = self.expr(rc, arg, Pos::Content, pre)?;
        let path = fresh(rc, "Path");
        let at = fresh(rc, "At");
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
        let decl = Stmt::ExternFn(ExternFn {
            name: name.clone(),
            args: ins,
            span,
        });
        let read = DocRead {
            location,
            path,
            at,
            name,
            span,
        };
        Ok((vec![decl], read))
    }

    /// `output k [: T] = t [where B]` (H-7): the declaration, when typed, and
    /// its value; a value that reads, or one with a condition, is the rule
    /// `output(k, t') :- B, reads`.
    fn output(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
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
                .flat_map(|s| self.names(s).arities(&name))
                .collect();
            match arities.iter().collect::<Vec<_>>().as_slice() {
                [n] => {
                    // A column of references holds the copy's resources,
                    // and never leaves a deployment (R-204).
                    let ref_columns = self.published_refs(scope, &name, **n, span)?;
                    let rel = crate::program::node::RelRef::own(name, span);
                    let kind = crate::program::ItemKind::OutputRelation { rel, ref_columns };
                    return Ok(self.program.item(span, self.item_scope, kind));
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
        // The declaration, once per scope (an output may have several rows);
        // with no type written, any.
        let ty = match node(n, TYPE_EXPR) {
            Some(t) => match self.resource_type(scope, &t) {
                Some(_) => TypeExpr::Name("addr".to_string()),
                None => self.type_expr(&t),
            },
            None => TypeExpr::Name("any".to_string()),
        };
        let addr_typed = matches!(&ty, TypeExpr::Name(n) if n == "addr");
        let declared = self.outputs.insert((scope, name.clone())).then_some(ty);
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
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        // An output typed by a resource type holds its address (`scoped` by
        // the module); any other output is a value, a resource in it the
        // reference (R-43).
        let (value, id) = match Chain::of(&t) {
            Some(c)
                if addr_typed
                    && c.is_bare()
                    && !rc.vars.contains_key(&c.head)
                    && self.resource(scope, &c.head).is_some() =>
            {
                (str_term(&c.head), self.str_node(self.span(&t), &c.head))
            }
            _ if addr_typed => self.arg_node(&mut rc, &t, Pos::Whole, &mut pre)?,
            _ => self.arg_node(&mut rc, &t, Pos::Value, &mut pre)?,
        };
        let head = atom_at("output", vec![str_term(&name), value], span);
        match pre.is_empty() && clause.is_none() {
            true => self.check_bound(&rc, &[], &[&head.args[1]])?,
            false => self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?,
        }
        let parts = (name, declared, clause, id, made);
        self.output_item(n, &rc, parts, head, pre)
    }

    /// An output row's item, of what it lowered to: its name, its type
    /// where it declares the output, its clause, its value's node, made
    /// the statement's helpers from `made`.
    #[allow(clippy::type_complexity)]
    fn output_item(
        &mut self,
        n: &SyntaxNode,
        rc: &Rc,
        (name, declared, clause, value, made): (
            String,
            Option<TypeExpr>,
            Option<crate::program::ClauseId>,
            ExprId,
            usize,
        ),
        head: Atom,
        pre: Vec<Lit>,
    ) -> L<ItemId> {
        let span = self.span(n);
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let o = crate::program::build::OutputLowered {
            name,
            ty: declared,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &pre,
            value,
            made: &self.helpers[made..],
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.output_item(o);
        gather.done(b);
        Ok(item)
    }

    /// `output k { f [: T] = t, g: { .. } } [where B]` (R-55): an object
    /// output by its fields, one value, typed by its fields' types (`any`
    /// where a field gives none).
    fn output_object(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        let (value, id, ty) = self.output_fields(&mut rc, n, &mut pre)?;
        let declared = self.outputs.insert((scope, name.clone())).then_some(ty);
        let head = atom_at("output", vec![str_term(&name), value], span);
        self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?;
        let parts = (name, declared, clause, id, made);
        self.output_item(n, &rc, parts, head, pre)
    }

    /// An object output's fields: the object term and its type.
    fn output_fields(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<(Term, ExprId, TypeExpr)> {
        let mut value = BTreeMap::new();
        let mut parts = Vec::new();
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
            let (v, id, t) = if node(&a, ATTR_DECL).is_some() {
                self.output_fields(rc, &a, pre)?
            } else {
                let Some(t) = terms(&a).next() else {
                    return self.error(span, format!("field {field} has no value: `{field} = t`"));
                };
                let (v, id) = self.arg_node(rc, &t, Pos::Value, pre)?;
                let ty = node(&a, TYPE_EXPR)
                    .map(|t| self.type_expr(&t))
                    .unwrap_or_else(|| TypeExpr::Name("any".into()));
                (v, id, ty)
            };
            types.push((field.clone(), t));
            parts.push(ObjPart::Field {
                key: field.clone(),
                key_span: span,
                value: id,
                pun: false,
            });
            value.insert(field, v);
        }
        let id = self.node(self.span(n), ExprKind::Object { parts });
        Ok((Term::Obj(value), id, TypeExpr::Object(types)))
    }

    /// The clause of a block statement: the `where` body after its block.
    fn clauses(&mut self, rc: &mut Rc, stmt: &SyntaxNode) -> L<Vec<Lit>> {
        let mut lits = Vec::new();
        for c in stmt.children().filter(|c| c.kind() == CLAUSE) {
            lits.extend(node(&c, BODY).ok_or(Skip)?.children());
        }
        self.lits(rc, &lits)
    }

    /// The assignments of a block, their reads appended to `reads`; with
    /// each, where its reads and the statement's helpers it made end.
    fn fields(&mut self, rc: &mut Rc, block: &SyntaxNode, reads: &mut Vec<Lit>) -> L<Entries> {
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
                let (value, id) = self.entry_value(rc, &a, Pos::Value, reads)?;
                let f = FieldAssign {
                    key,
                    op,
                    value,
                    rank: self.rank_tok(&a)?,
                    span: self.span(&a),
                };
                Ok((f, id, (reads.len(), self.helpers.len())))
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
    fn signature(&self, scope: ScopeId, name: &str) -> Option<(u32, SyntaxNode)> {
        match name.rsplit_once('.') {
            Some((m, s)) => {
                let path = self.module_path_of(scope, m);
                let at = self.module_at(&path)?.0;
                let sig = self.names(at).signature(s);
                sig.map(|(f, n)| (f, n.clone()))
            }
            None => self
                .chain_of(scope)
                .into_iter()
                .find_map(|s| self.names(s).signature(name).map(|(f, n)| (f, n.clone()))),
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
    fn check_signature(&mut self, n: &SyntaxNode, t: &SyntaxNode, scope: ScopeId) {
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
    /// each under a clause, of one type. Which declaration it is, and at
    /// the first where each is (its `__declared` row and the denies of two
    /// that both hold, `lower` writes).
    fn inputs_named(
        &mut self,
        n: &SyntaxNode,
        name: &str,
        span: Span,
    ) -> L<Option<(usize, Vec<Span>)>> {
        let same: Vec<SyntaxNode> = n
            .parent()
            .into_iter()
            .flat_map(|p| p.children())
            .filter(|c| c.kind() == INPUT && word_text(c, 1) == name)
            .collect();
        if same.len() < 2 {
            return Ok(None);
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
        let i = same.iter().position(|c| c == n).unwrap_or_default();
        let sites = match i {
            0 => same.iter().map(|c| self.span(c)).collect(),
            _ => Vec::new(),
        };
        Ok(Some((i, sites)))
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
            .filter(|c| c.kind() == USE || (self.is_copy(c) && !named_by_clause(c)))
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
    fn use_stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<Option<ItemId>> {
        let span = self.span(n);
        let (written, name) = use_parts(n);
        self.redeclared(n, &name)?;
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
            return Ok(None);
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
            return Ok(None);
        }
        let path = self.module_path_of(scope, &written);
        match self.module_at(&path) {
            None => return self.error(span, format!("no module `{written}`")),
            Some((_, true)) => {
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
        let copy = (CopyKind::Use, path, name, None);
        self.copy(n, scope, outer, copy).map(Some)
    }

    /// `resource stacks.S NAME { k = v } [where B]` in a project module
    /// (R-114): a deployment of the stack S, a resource of its type whose
    /// attributes are its key's values. A project module makes nothing
    /// else. Built as the `Resource` item it is.
    fn deployment(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let written = dotted_text(n, 1);
        if self.decls.deployed.iter().any(|d| d.path == written) {
            return self.block_stmt(n, scope, outer);
        }
        let stacks: Vec<String> = self
            .decls
            .deployed
            .iter()
            .map(|d| format!("`{}`", d.path))
            .collect();
        let d = Diagnostic::error(
            self.span(n),
            format!(
                "{written} is no stack: a project module's resources are deployments of stacks"
            ),
        )
        .with_help(format!(
            "make a deployment of a stack by its path, `resource {} NAME {{ KEY = VALUE }}`; \
             what else is made is a stack's",
            self.decls
                .deployed
                .first()
                .map_or("stacks.NAME", |d| d.path.as_str())
        ));
        let d = match stacks.is_empty() {
            true => d,
            false => d.with_note(format!("the stacks it deploys: {}", stacks.join(", "))),
        };
        self.diags.push(d);
        Err(Skip)
    }

    /// `resource PATH NAME { k = v } [where B]` of a component (R-113,
    /// R-65): one copy of the component, named NAME.
    fn instance(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
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
        if !named_by_clause(n) {
            self.redeclared(n, &name)?;
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
        let via = self.via(scope, &written, &module);
        self.copy(n, scope, outer, (CopyKind::Component, module, name, via))
    }

    /// The module instance the component `component`, written `written`
    /// in `scope`, is named through (R-186): `a.volume` under `use backups
    /// as a`, the component an item of that module. Its body reads that
    /// instance's items bare.
    fn via(&self, scope: ScopeId, written: &str, component: &str) -> Option<Via> {
        let (head, _) = written.split_once('.')?;
        let (parent, _) = component.rsplit_once('.')?;
        let declared = self.chain_of(scope).into_iter().find(|s| {
            let sc = self.names(*s);
            sc.component(head).is_some() || sc.module(head).is_some()
        })?;
        if self.names(declared).module(head)? != parent {
            return None;
        }
        Some(Via {
            module: parent.to_string(),
            name: head.to_string(),
            absolute: !self.own_scopes(scope).contains(&declared),
        })
    }

    /// One copy of the component `module`, named `name`, its inputs the
    /// block of `n` (an `instance` or a `use`) and its clause `n`'s. Built
    /// as a `Copy` item (R-211 step 5): a name from the clause, its
    /// inputs' values and the reads they hoisted, its rows items of their
    /// own, its clause B's goals; `lower` writes it.
    fn copy(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
        (kind, module, name, via): (CopyKind, String, String, Option<Via>),
    ) -> L<ItemId> {
        let span = self.span(n);
        self.check_gives(n, scope, &module)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.clauses(&mut rc, n)?;
        let gathered = self.gather.as_mut().and_then(|g| g.take_clause());
        let named_at = (body.len(), self.helpers.len());
        // A name from the clause (R-191): each row a copy, named as a
        // provider type's resource is.
        let (named, named_id) = match header_name(n).filter(|_| named_by_clause(n)) {
            Some(t) => {
                let (t, id) = self.name_from_clause(&mut rc, &t, &mut body)?;
                (Some(t), Some(id))
            }
            None => (None, None),
        };
        let made = self.helpers.len();
        let mut reads = Vec::new();
        let fields = match node(n, BLOCK) {
            Some(block) => self.fields(&mut rc, &block, &mut reads)?,
            None => Vec::new(),
        };
        let base = body.len();
        body.extend(reads);
        let mut inputs = Vec::new();
        let mut from = (base, made);
        for (f, id, (r, h)) in fields {
            if matches!(f.op, FieldOp::Add) {
                return self.error(f.span, "an input of a copy is set with `=`, not `+=`");
            }
            if f.rank.is_some() {
                return self.error(f.span, "an input of a copy takes no rank");
            }
            let to = (base + r, h);
            inputs.push((f.key, f.value, f.span, id, from.1..to.1));
            from = to;
        }
        let values: Vec<&Term> = inputs
            .iter()
            .map(|(_, v, _, _, _)| v)
            .chain(named.as_ref())
            .collect();
        self.check_bound(&rc, &body, &values)?;
        let rows = match node(n, BLOCK) {
            Some(block) => self.block_rows(&block, &module, scope, outer)?,
            None => Default::default(),
        };
        // A copy binds no aggregate: its rows do, each folding its own.
        self.aggregate_in(&body, span)?;
        let c = crate::program::build::CopyLowered {
            span,
            scope: self.item_scope,
            kind,
            module,
            name,
            named: named_id.map(|id| (id, &self.helpers[named_at.1..made])),
            inputs: inputs
                .iter()
                .map(|(key, _, at, id, h)| crate::program::build::InputGiven {
                    key: key.clone(),
                    span: *at,
                    value: *id,
                    made: &self.helpers[h.clone()],
                })
                .collect(),
            rows,
            clause: gathered,
            via,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.copy_item(c);
        gather.done(b);
        Ok(item)
    }

    /// The rows a `use` or `instance` block gives the relations its
    /// module takes, `input p` (R-55): `p(t, ..) [where B]`, a rule in
    /// this scope, and `p from FORMAT(..) [where B]`, a table, its columns
    /// the module's `decl p`. Each head is the module's own `p`, made the
    /// copy's by `modules`. Their items.
    fn block_rows(
        &mut self,
        block: &SyntaxNode,
        module: &str,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<Vec<ItemId>> {
        let Some(inner) = self.module_at(module).map(|m| m.0) else {
            return Ok(Default::default());
        };
        let given = |k: &DeclKind| matches!(k, DeclKind::RelationInput { given: true });
        let takes: BTreeSet<String> = self.names(inner).names(given).cloned().collect();
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
                    let Some(decl) = self.names(inner).decl(&pred).cloned() else {
                        return self.error(
                            span,
                            format!(
                                "{pred} from ..: {module} declares no columns for {pred}, \
                                 `decl {pred}(a: T, ..)`"
                            ),
                        );
                    };
                    let cols = self.table_columns(&pred, &decl)?;
                    let arity = cols.len();
                    self.table_item(&n, scope, outer, (pred, arity, false), cols)
                })(),
                // A row's references are the user's resources, the
                // module's column typed as its `decl` says.
                _ => {
                    self.given = Some(inner);
                    let r = self.rule(&n, scope, outer);
                    self.given = None;
                    r
                }
            };
            match r {
                Ok(item) => out.push(item),
                Err(Skip) => failed = true,
            }
        }
        if failed {
            return Err(Skip);
        }
        Ok(out)
    }

    /// A header name with holes, `"agent-${i}"`, of a resource of any
    /// type, a provider's, a stack's or a component's (R-112, R-191): a
    /// term per row of the clause, bound last by `format`, one segment of
    /// the address (quoted when it holds a dot). Its variable.
    fn name_from_clause(
        &mut self,
        rc: &mut Rc,
        header: &SyntaxToken,
        body: &mut Vec<Lit>,
    ) -> L<(Term, ExprId)> {
        let mut pre = Vec::new();
        let s = self.string_expr(rc, header, &mut pre)?;
        let t = self.lowered(s, &mut pre);
        body.extend(pre);
        let v = fresh(rc, "Addr");
        body.push(Lit::Eq(
            var(&v),
            func(crate::address::NAME_SEGMENT, vec![t]),
        ));
        let span = self.span_of(header.text_range());
        let id = self.building(rc, span, |b| {
            let segment = b.form(crate::address::NAME_SEGMENT, vec![s]);
            b.bound_into(segment, &v)
        });
        Ok((var(&v), id))
    }

    /// `resource T n { f = t ... } where B`, `T` a provider's type or one
    /// the program declares. Built as a `Resource` item (R-211 step 5):
    /// its header, each entry's value and the reads it hoisted, the clause
    /// B's goals; `lower` writes the statement.
    fn block_stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        use crate::program::build::{BodyLowered, EntryLowered, HeaderLowered};
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
        let clause = self.gather.as_mut().and_then(|g| g.take_clause());
        let made = self.helpers.len();
        let mut reads = Vec::new();
        let (fields, ends, value) = match &block {
            Some(block) => {
                let fields =
                    self.wanting(Want::Schema, |l| l.fields(&mut rc, block, &mut reads))?;
                let (fields, ends): (Vec<_>, Vec<_>) = fields
                    .into_iter()
                    .map(|(f, id, end)| ((f, id), end))
                    .unzip();
                (fields, ends, None)
            }
            None => {
                let (_, value, fields) = self.value_body(&mut rc, n, &mut reads)?;
                let end = (reads.len(), self.helpers.len());
                let fields = fields.into_iter().map(|f| (f, value)).collect();
                (fields, vec![end], Some(value))
            }
        };
        let (fields, ids): (Vec<FieldAssign>, Vec<ExprId>) = fields.into_iter().unzip();
        let reads_at = body.len()..body.len() + reads.len();
        body.extend(reads);
        let named_at = (body.len(), self.helpers.len());
        // The header: a string with holes is bound last, by `format`; a
        // bare name is the literal name, always (R-76). The name is one
        // segment of the address, quoted when it holds a dot (R-112).
        let interp = header.kind() == STRING && has_hole(header.text());
        let mut named = None;
        let name = if interp {
            let (t, id) = self.name_from_clause(&mut rc, &header, &mut body)?;
            named = Some(id);
            t
        } else if header.kind() == STRING {
            str_term(&crate::address::name_segment(&self.string(&header)?))
        } else {
            let text = header.text();
            let bound = bound_vars(&body);
            match rc.vars.get(text) {
                Some(v) if bound.contains(v) || rc.bound_around().any(|b| b == v) => {
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
        self.aggregate_in(&body, span)?;
        let typ = dotted_text(n, 1);
        let literal =
            (header.kind() == STRING && !interp).then(|| self.string(&header).unwrap_or_default());
        // Each entry's reads and helpers, from where the one before ended.
        let starts = std::iter::once((reads_at.start, made))
            .chain(ends.iter().map(|&(r, h)| (reads_at.start + r, h)));
        let spans: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = starts
            .zip(&ends)
            .map(|((r0, h0), &(r1, h1))| (r0..reads_at.start + r1, h0..h1))
            .collect();
        let body_lowered = match value {
            Some(v) => BodyLowered::Value {
                value: v,
                made: &self.helpers[spans[0].1.clone()],
            },
            None => BodyLowered::Block(
                fields
                    .iter()
                    .zip(&ids)
                    .zip(&spans)
                    .map(|((f, id), (_, h))| EntryLowered {
                        path: &f.key,
                        op: f.op,
                        value: *id,
                        rank: f.rank,
                        span: f.span,
                        made: &self.helpers[h.clone()],
                    })
                    .collect(),
            ),
        };
        let header_lowered = if let Some(value) = named {
            HeaderLowered::Interp {
                value,
                made: &self.helpers[named_at.1..],
            }
        } else if let Some(text) = literal {
            HeaderLowered::Literal(text)
        } else {
            HeaderLowered::Bare(header.text())
        };
        let r = crate::program::build::ResourceLowered {
            span,
            scope: self.item_scope,
            typ: crate::program::node::TypeRef { name: typ, span },
            name: header_lowered,
            rank,
            clause,
            body: body_lowered,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.resource_item(r);
        gather.done(b);
        Ok(item)
    }

    /// `resource T N = VALUE` (R-126): the body is a value of the type, a
    /// document. An object written out is the block of its entries, one
    /// per key, checked as a block's are; any other value is one
    /// contribution at the root, an entry per key of the object it is when
    /// the rule runs (`transform::resource_to_stmts`). The value, and its
    /// entries.
    fn value_body(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        reads: &mut Vec<Lit>,
    ) -> L<(Term, ExprId, Vec<FieldAssign>)> {
        let t = terms(n).next().ok_or(Skip)?;
        let span = self.span(&t);
        let (value, id) = self.wanting(Want::Schema, |l| l.arg_node(rc, &t, Pos::Value, reads))?;
        match crate::program::lower::value_entries(value.clone(), span) {
            Some(entries) => Ok((value, id, entries)),
            None => self.error(
                span,
                format!(
                    "`resource T NAME = VALUE` takes an object, a value of the type: not `{}`",
                    t.text().to_string().trim()
                ),
            ),
        }
    }

    /// `set from DOC [@rank] [where B]` (R-38): every leaf of the document
    /// a contribution to the input at its path, `arg(input, "", P, V, Rank)
    /// :- B, reads, table.FORMAT.set(Path, At, P, V)`, which
    /// `tables::expand_set_from` makes one rule per input path, and a deny
    /// for a leaf at a path that is no input's.
    fn set_from(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let rank = self.rank_tok(n)?;
        let (path, value) = (fresh(&mut rc, "Path"), fresh(&mut rc, "Value"));
        let cols = [("path", "string"), ("value", "any")]
            .map(|(name, ty)| BindArg {
                input: false,
                name: name.into(),
                ty: Some(TypeExpr::Name(ty.into())),
            })
            .to_vec();
        let made = self.helpers.len();
        let (externs, rows) = self.doc_source(
            &mut rc,
            n,
            crate::tables::SET_DOC,
            cols,
            vec![var(&path), var(&value)],
            &mut body,
        )?;
        self.check_bound(&rc, &body, &[])?;
        let head = atom_at(
            "arg",
            vec![
                str_term(crate::modules::INPUT),
                str_term(""),
                var(&path),
                var(&value),
                str_term(rank.unwrap_or(Rank::Normal).name()),
            ],
            span,
        );
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let s = crate::program::build::SetFromLowered {
            span,
            scope: self.item_scope,
            clause,
            rank,
            path: &path,
            value: &value,
            externs,
            head: &head,
            body: &body,
            rows,
            made: &self.helpers[made..],
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.set_from_item(s);
        gather.done(b);
        Ok(item)
    }

    fn opt_body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        match node(n, BODY) {
            Some(b) => self.body(rc, &b),
            None => Ok(Vec::new()),
        }
    }

    /// `p(a, b) [@rank] [where B]` (a fact without `where`) built as a
    /// `rule` item (R-211 step 5): its head the relation and its
    /// arguments, the reads they hoisted the head's, its clause B's goals;
    /// `lower` writes it.
    fn rule(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let r = self.rule_parts(n, scope, outer)?;
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let l = crate::program::build::RuleLowered {
            span: self.span(n),
            scope: self.item_scope,
            head: &r.head,
            rel: r.rel,
            args: r.args,
            rank: r.rank,
            clause: r.clause,
            body: &r.body,
            made: &self.helpers[r.made..],
            results: &results,
            ranked: &r.ranked,
        };
        let written: BTreeSet<&str> = r.written.iter().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, r.head.span, &written);
        let item = b.rule_item(l);
        gather.done(b);
        Ok(item)
    }

    /// A rule lowered: its head, its body and what binds it.
    fn rule_parts(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<RuleParts> {
        let span = self.span(n);
        let head_node = n.children().find(|c| c.kind() == CALL).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let clause = self.body_clause(n);
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
                    "`{}` is another copy's relation, read after `where`; its rows are its own",
                    c.fields().join(".")
                ),
            );
        }
        let made = self.helpers.len();
        let (head, args, via) = self.calls(calls, |l| {
            l.atom_args(&mut rc, &head_node, Pos::Whole, &mut body)
        })?;
        let rel = RelRef::reached(head.pred.clone(), via, head.span);
        let args = self.rel_args(&rc, head.span, args);
        let rank = self.rank_tok(n)?;
        let mut ranked = head.clone();
        if let Some(rank) = rank {
            if head.pred != "arg" || head.args.len() != 4 || head.record.is_some() {
                return self.error(
                    span,
                    format!(
                        "a rank orders the writes to one attribute, `set r.p = v @override`: \
                         `{}` is a relation, whose rows are not ranked",
                        head.pred.rsplit("::").next().unwrap_or(&head.pred)
                    ),
                );
            }
            ranked.args.push(str_term(rank.name()));
        }
        self.core_head(&head_node, &ranked, has_body)?;
        self.check_bound(&rc, &body, &atom_terms(&ranked))?;
        Ok(RuleParts {
            clause,
            head,
            rel,
            args,
            ranked,
            rank,
            body,
            made,
            written: rc.vars.values().cloned().collect(),
        })
    }

    /// `let k = t [@rank] [where B]` (H-6, R-3): a contribution to the cell
    /// `k`, written `let(k, t, rank)` for `modules` to scope (the cell is
    /// `(let, SCOPE, k)`, read by name as `k(V)`). When `t` is a reference,
    /// `k`'s value is that reference and a dot on `k` reads through it.
    /// Built as a `let` item (R-211 step 5): its clause B's goals, its
    /// value the term's node; `lower` writes it.
    fn let_stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
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
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        let t = terms(n).next().ok_or(Skip)?;
        let aggregate = t.kind() == CALL && self.aggregate_name(&t).is_some();
        let mut fold = None;
        let (value, id) = if aggregate {
            // `let n = count(x) where B`: one group (R-59).
            let (call, agg) = self.aggregate_call(&mut rc, &t, &mut body)?;
            let v = fresh(&mut rc, &capitalise(&name));
            self.aggs.push(aggregate::Agg {
                var: v.clone(),
                text: t.text().to_string(),
                span: self.span(&t),
            });
            body.push(Lit::Eq(var(&v), call));
            let at = self.span(&t);
            fold = Some(self.fold_goal(&rc, at, &v, agg));
            (var(&v), Some(self.var_node(&rc, at, &v)))
        } else {
            let (value, id) = self.wanting(want, |l| l.let_value(&mut rc, &t, &mut body))?;
            (value, Some(id))
        };
        let (value, id) = match (&ty, &declared) {
            (Some(ty), Some(d)) => {
                let typed = self.let_typed(scope, &name, (ty, d), &t, value.clone())?;
                let id = id.map(|id| self.retyped(&rc, id, &value, typed.clone()));
                (typed, id)
            }
            _ => (value, id),
        };
        let rank = self.rank_tok(n)?;
        let head = Atom {
            pred: crate::modules::LET.to_string(),
            args: vec![
                str_term(&name),
                value.clone(),
                str_term(rank.unwrap_or(Rank::Normal).name()),
            ],
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        // The cell's type is its reader's column, `decl NAME(NAME: T)`, as
        // R-34 types a relation: once, at the first typed row. A resource
        // type is the reference's own (`value_type`).
        let declares = matches!(&ty, Some(t) if !matches!(t, crate::types::Ty::Ref(_)))
            && typed_rows.first().and_then(|t| t.parent()).as_ref() == Some(n);
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let l = crate::program::build::LetLowered {
            name,
            ty: declared,
            rank,
            declares,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &body,
            value: id,
            fold,
            made: &self.helpers[made..],
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let at = self.span(&t);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, at, &written);
        let item = b.let_item(l);
        gather.done(b);
        let item = item.ok_or(Skip)?;
        Ok(item)
    }

    /// The clause of statement `n`'s body just lowered, gathered (empty
    /// when no literal is written in it); none without a body, or when
    /// nothing gathers.
    fn body_clause(&mut self, n: &SyntaxNode) -> Option<crate::program::ClauseId> {
        self.clause_at(node(n, BODY))
    }

    /// The literal of `let NAME = LITERAL [@rank]` (R-211's first port): no
    /// type, no clause, a literal not interpolated.
    fn literal_let(&self, n: &SyntaxNode) -> Option<SyntaxNode> {
        if n.kind() != LET
            || [TYPE_EXPR, BODY, PARAMS]
                .iter()
                .any(|&k| node(n, k).is_some())
        {
            return None;
        }
        let t = terms(n).next().filter(|t| t.kind() == LITERAL)?;
        let tok = tokens(&t).next()?;
        let interpolated = tok.kind() == STRING && !self.text && tok.text().contains("${");
        (!interpolated).then_some(t)
    }

    /// `let NAME = LITERAL [@rank]` built as a `let` item of the literal's
    /// node, read from the tree; `lower` writes it `let("NAME", v,
    /// "rank")`.
    fn let_literal(&mut self, n: &SyntaxNode, scope: ScopeId, t: &SyntaxNode) -> Option<ItemId> {
        self.let_item(n, scope, t).ok()
    }

    fn let_item(&mut self, n: &SyntaxNode, scope: ScopeId, t: &SyntaxNode) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Err(e) = self.value_type(scope, &name) {
            return self.error(span, e);
        }
        let value = self.literal_expr(t)?;
        let rank = self.rank_tok(n)?;
        Ok(self.program.items.insert(crate::program::Item {
            span,
            scope: self.item_scope,
            kind: crate::program::ItemKind::Let {
                name,
                ty: None,
                value,
                clause: None,
                rank,
                declares: false,
            },
        }))
    }

    /// A typed `let`'s value `value` (lowered from `t`) checked against
    /// its type `ty` (declared `d`), a literal read as one (R-31, R-74),
    /// an object type's fields each as theirs (R-192).
    fn let_typed(
        &mut self,
        scope: ScopeId,
        name: &str,
        (ty, d): (&crate::types::Ty, &crate::ast::TypeExpr),
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
                    crate::address::REF,
                    vec![str_term(&typ), value.clone(), str_term("")],
                );
                match crate::types::mismatch(ty, &r) {
                    Some(why) => self.error(span, format!("let {name} {why}")),
                    None => Ok(value),
                }
            }
            None => crate::types::declared(d, value).or_else(|(path, why)| {
                let name = crate::types::dotted(name, &path);
                self.error(span, format!("let {name} {why}"))
            }),
        }
    }

    /// A `let`'s value: a reference is its key (a resource's address, a
    /// live object's name); anything else the term.
    fn let_value(&mut self, rc: &mut Rc, t: &SyntaxNode, body: &mut Vec<Lit>) -> L<(Term, ExprId)> {
        if let Some(c) = Chain::of(t) {
            // A resource by its bare name.
            if c.is_bare()
                && !rc.vars.contains_key(&c.head)
                && !self.is_value(rc.scope, &c.head)
                && self.resource(rc.scope, &c.head).is_some()
            {
                let span = self.span(t);
                let (_, addr) = self.reference(rc, &c, body, span)?;
                return Ok((self.term_of(addr), addr));
            }
            let mut pre = Vec::new();
            let mut rc2 = rc.clone();
            if let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) {
                let key = match &res {
                    Res::Ref { addr, path, .. } if path.is_empty() => Some(*addr),
                    Res::World { addr, path, .. } if path.is_empty() => Some(*addr),
                    _ => None,
                };
                if let Some(k) = key {
                    *rc = rc2;
                    body.extend(pre);
                    return Ok((self.term_of(k), k));
                }
            }
        }
        self.arg_node(rc, t, Pos::Value, body)
    }

    /// `set chain (=|+=) t [@rank] [where B]` (H-5): a contribution to a
    /// resource's attribute (`arg(T, A, p, t)`) or an input (the stack's
    /// own, a field of an object one, a used module's, a copy's), normal
    /// unless ranked (R-38). `set { chain = t .. } [@rank] [where B]` is
    /// several under one clause and rank; `set from DOC [@rank] [where B]`
    /// a document's leaves, each to the input at its path. Built as a
    /// `Set` item (R-211 step 5): each line's target, its value and the
    /// reads they hoisted, the clause B's goals; `lower` writes each
    /// line's `arg`.
    fn set(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        if tokens(n).nth(1).is_some_and(|t| t.text() == "from")
            && !tokens(n).any(|t| matches!(t.kind(), EQ | PLUS_EQ))
        {
            return self.set_from(n, scope, outer);
        }
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        let body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let clause = self.body_clause(n);
        let rank = self.rank_tok(n)?;
        let block = node(n, BLOCK);
        let lines: Vec<SyntaxNode> = match &block {
            Some(b) => b.children().filter(|c| c.kind() == ASSIGN).collect(),
            None => vec![n.clone()],
        };
        let mut writes = Vec::new();
        let mut failed = false;
        for a in &lines {
            let r = (|| {
                let lhs = a.children().find(|c| c.kind() == CHAIN).ok_or(Skip)?;
                let rhs = terms(a).find(|t| *t != lhs).ok_or(Skip)?;
                let add = tokens(a).any(|t| t.kind() == PLUS_EQ);
                let rank = match block {
                    Some(_) => self.rank_tok(a)?.or(rank),
                    None => rank,
                };
                let span = self.span(a);
                let mut rc = rc.clone();
                let line = (&lhs, &rhs, add, rank, span);
                self.contribution(&mut rc, line, body.clone(), has_body)
            })();
            match r {
                Ok(w) => writes.push(w),
                Err(Skip) if block.is_some() => failed = true,
                Err(Skip) => return Err(Skip),
            }
        }
        if failed {
            return Err(Skip);
        }
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let written: BTreeSet<&str> = writes
            .iter()
            .flat_map(|w| w.written.iter().map(String::as_str))
            .collect();
        let written = |v: &str| written.contains(v);
        let s = crate::program::build::SetLowered {
            span,
            scope: self.item_scope,
            clause,
            writes: writes.iter().map(|w| w.lowered(&self.helpers)).collect(),
            results: &results,
        };
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.set_item(s);
        gather.done(b);
        Ok(item)
    }

    /// One contribution of a `set`: `lhs (=|+=) rhs [@rank]` at `span`,
    /// under the clause `body` (`has_body`: one is written).
    fn contribution(
        &mut self,
        rc: &mut Rc,
        (lhs, rhs, add, rank, span): (&SyntaxNode, &SyntaxNode, bool, Option<Rank>, Span),
        mut body: Vec<Lit>,
        has_body: bool,
    ) -> L<Contribution> {
        let c = Chain::of(lhs).ok_or(Skip)?;
        if add && rank.is_some() {
            return self.error(span, "a rank applies to `=`, not `+=`");
        }
        let made = self.helpers.len();
        let target = self.set_target(rc, &c, &mut body, span)?;
        match &target {
            Target::Element(_, _, list, _, _, block) => {
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
            }
            Target::Cell(_, _, path, Some(block)) if !has_body => {
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
            Target::Cell(..) => {}
        }
        let mid = (body.len(), self.helpers.len());
        // A contribution to an input's cell is normal unless ranked (R-38),
        // written to the cell itself, so that its condition may read
        // another input.
        let (value, value_node) = match &target {
            Target::Input(_) => self.arg_node(rc, rhs, Pos::Value, &mut body)?,
            _ => self.wanting(Want::Schema, |l| l.arg_node(rc, rhs, Pos::Value, &mut body))?,
        };
        let ranked = || str_term(rank.unwrap_or(Rank::Normal).name());
        let (pred, args) = match &target {
            Target::Element(typ, addr, list, key, rest, _) => {
                let elem = crate::program::lower::element_write(key.0.clone(), rest, value.clone());
                let args = vec![
                    typ.0.clone(),
                    addr.0.clone(),
                    str_term(list),
                    elem,
                    ranked(),
                ];
                ("arg", args)
            }
            Target::Input(k) => {
                let input = str_term(crate::modules::INPUT);
                (
                    "arg",
                    vec![input, str_term(""), str_term(k), value.clone(), ranked()],
                )
            }
            Target::Cell(typ, addr, path, _) => {
                let mut args = vec![typ.0.clone(), addr.0.clone(), str_term(path), value.clone()];
                args.extend(rank.map(|r| str_term(r.name())));
                (if add { "arg_add" } else { "arg" }, args)
            }
        };
        let head = atom_at(pred, args, span);
        self.check_bound(rc, &body, &atom_terms(&head))?;
        Ok(Contribution {
            span,
            target,
            add,
            rank,
            value: value_node,
            head,
            body,
            made: (made, mid.1, self.helpers.len()),
            written: rc.vars.values().cloned().collect(),
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
                .find(|&s| self.names(s).module(&c.head).is_some())
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                self.termed_str(span, crate::modules::INPUT),
                self.scoped_name(scope, at, &c.head, span),
                c.fields()[1..].join("."),
                own.then(|| format!("use {}", c.head)),
            ));
        }
        // An element of a keyed list a variable ranges over: `set c.p = v
        // where c in w.containers` writes `w.containers[c].p` (R-69).
        if let Some(Elem { typ, addr, list }) = rc.elems.get(&c.head).cloned()
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
            let key = self.var_named(rc, &c.head, span);
            let key = (var(&key), self.var_node(rc, span, &key));
            let block = self.owning_block(scope, &typ.0, &addr.0);
            let (typ, addr) = (
                self.settled_part(rc, typ, span),
                self.settled_part(rc, addr, span),
            );
            let (addr, key) = (self.each_read(rc, addr), self.each_read(rc, key));
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        // An instance's input: `n.k`.
        if let [Op::Field(k)] = c.ops.as_slice()
            && let Some((at, path)) = self.instance_in(scope, &c.head)
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                self.termed_str(span, crate::modules::INPUT),
                self.scoped_name(scope, at, &c.head, span),
                k.clone(),
                own.then(|| format!("resource {path} {}", c.head)),
            ));
        }
        self.target_named(rc, c)?;
        let mut pre = Vec::new();
        let res = self.resolve(rc, c, &mut pre)?;
        body.extend(pre);
        let (typ, addr, path) = match res {
            Res::Ref { typ, addr, path } if !path.is_empty() => {
                let addr = self.each_read(rc, (self.term_of(addr), addr));
                ((self.term_of(typ), typ), addr, path)
            }
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
            Seg::I(t) => !matches!(self.term_of(*t), Term::Val(Value::Int(_))),
            Seg::K(_) => true,
            Seg::F(_) => false,
        });
        if let Some(i) = keyed
            && !matches!(&typ.0, Term::Val(Value::Str(t)) if t == "settings")
        {
            let (Seg::I(key) | Seg::K(key)) = path[i].clone() else {
                unreachable!("found above")
            };
            let key = (self.term_of(key), key);
            let Some(list) = self.path_string(&path[..i]).filter(|l| !l.is_empty()) else {
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
            let block = self.owning_block(scope, &typ.0, &addr.0);
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        let Some(path) = self.path_string(&path) else {
            return self.error(span, "a contribution's path is constant");
        };
        let block = self.owning_block(scope, &typ.0, &addr.0);
        Ok(Target::Cell(typ, addr, path, block))
    }

    /// The string `s`, its term and its node at `span`.
    fn termed_str(&mut self, span: Span, s: &str) -> Termed {
        (str_term(s), self.str_node(span, s))
    }

    /// The name `name` of a scope declared in `declared`, as read in
    /// `scope` ([`Self::scoped`]): its term and its node.
    fn scoped_name(&mut self, scope: ScopeId, declared: ScopeId, name: &str, span: Span) -> Termed {
        let n = self.str_node(span, name);
        let id = self.scoped(scope, declared, n, span);
        (self.term_of(id), id)
    }

    /// `part`, resolved elsewhere, where a target writes it: a copy whose
    /// reads are made already.
    fn settled_part(&mut self, rc: &Rc, (t, id): Termed, span: Span) -> Termed {
        (t, self.building(rc, span, |b| b.settled(id)))
    }

    /// A `set` target's part: the read of the `[_]` that binds it, at its
    /// first use ([`Rc::each_reads`]), else itself.
    fn each_read(&mut self, rc: &mut Rc, (t, id): Termed) -> Termed {
        let read = match &self.program.exprs[id].kind {
            ExprKind::Var(v) => rc.each_reads.remove(&self.program.vars[*v].lowered),
            _ => None,
        };
        (t, read.unwrap_or(id))
    }

    /// A `set`'s path starts with what it writes: a name in scope (a
    /// resource, an input, a copy, a variable of the clause) or a type,
    /// `T[k]`. One that starts with no name is the error at the target,
    /// whatever follows it (`web.spec`, `web.containers["web"]`).
    fn target_named(&mut self, rc: &Rc, c: &Chain) -> L<()> {
        let h = c.head.as_str();
        let named = c.is_bare()
            || c.call.is_some()
            || rc.vars.contains_key(h)
            || rc.types.contains_key(h)
            || rc.instances.contains_key(h)
            || self.declares(rc.scope, h)
            || self.decls.namespaces.contains(h)
            || self.decls.providers.contains(h)
            || self.decls.types.contains(h)
            || matches!(h, "world" | "settings" | "super" | "_")
            || self.lenient
            || self.any_type;
        if named {
            return Ok(());
        }
        let near = self.scope_names(rc);
        let help = match crate::diag::nearest(h, near.iter().map(String::as_str)) {
            Some(n) => format!("`{n}` is in scope"),
            None => format!(
                "a resource is named as its block declares it (`resource T {h}`), or by its \
                 type and key, `T[\"{h}\"]`"
            ),
        };
        let d = Diagnostic::error(
            self.span_of(c.range),
            format!(
                "`set` writes a resource's attribute or an input: nothing in scope is named `{h}`"
            ),
        )
        .with_help(help);
        self.diags.push(d);
        Err(Skip)
    }

    /// The block that owns the cells of `(T, A)` when it is declared in
    /// `scope`: `resource T A`.
    fn owning_block(&self, scope: ScopeId, typ: &Term, addr: &Term) -> Option<String> {
        let s = self.names(self.decl_scope(scope));
        match (typ, addr) {
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) => s
                .resources(a)
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
                    crate::modules::INPUT if s(1).as_deref() == Some("") => p,
                    crate::modules::INPUT => format!("{}.{p}", s(1).unwrap_or_default()),
                    _ => format!("{t}[{}]{}", a[1], crate::address::path_suffix(&p)),
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
                    crate::address::path_suffix(&s(2).unwrap())
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
                    crate::address::path_suffix(&s(2).unwrap())
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
    /// `deny "m" [{ .. }] [where B]`, `warn ..` built as a `Check` item
    /// (R-211 step 5): its message and detail the terms' nodes, its clause
    /// B's goals; `lower` writes `deny(m[, detail])`.
    fn check(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let kw = tokens(n).next().ok_or(Skip)?;
        let msg = tokens(n).find(|t| t.kind() == STRING).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        // The message is a string like any other: `${e}` reads the body's
        // variables (H-13).
        let message = self.string_expr(&mut rc, &msg, &mut body)?;
        let mut args = vec![self.lowered(message, &mut body)];
        let made_mid = self.helpers.len();
        let mut detail = None;
        if let Some(o) = node(n, OBJECT) {
            let (t, id) = self.arg_node(&mut rc, &o, Pos::Whole, &mut body)?;
            args.push(t);
            detail = Some(id);
        }
        let head = Atom {
            pred: kw.text().to_string(),
            args,
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        let kind = match kw.kind() {
            WARN_KW => crate::program::node::CheckKind::Warn,
            _ => crate::program::node::CheckKind::Deny,
        };
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let c = crate::program::build::CheckLowered {
            kind,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &body,
            message,
            detail,
            made: (&self.helpers[made..made_mid], &self.helpers[made_mid..]),
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.check_item(c);
        gather.done(b);
        Ok(item)
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
        bound.extend(rc.bound_around().cloned());
        for (src, low) in &rc.vars {
            if rc.outer.contains(low)
                || rc.inner.contains(low)
                || (bound.contains(low) && rc.binders.contains(src))
                || unbound.contains(low)
            {
                continue;
            }
            let Some(span) = rc.first.get(src).copied() else {
                continue;
            };
            failed = true;
            let near = self.scope_names(rc);
            let d = unknown_name(
                span,
                src,
                &format!("\"{src}\""),
                true,
                near.iter().map(String::as_str),
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

    /// A statement's body (its clauses together), built once as written,
    /// checked for what binds, its clause in the order its goals hold
    /// (R-10): each after what binds what it reads, otherwise as written.
    /// A body nested in it is checked with it. The literals it returns are
    /// the resolver's own, as written.
    fn lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<Vec<Lit>> {
        self.lits_after(rc, lits, Vec::new())
    }

    /// [`Self::lits`] after `seed`, literals the statement holds before
    /// its clause: a let's parameters' binding (R-187), which a `not { }`
    /// helper's body takes with the other positive literals before it.
    fn lits_after(&mut self, rc: &mut Rc, lits: &[SyntaxNode], seed: Vec<Lit>) -> L<Vec<Lit>> {
        if self.nested > 0 {
            return self.lits_as_written(rc, lits, seed);
        }
        let outer = rc.outer.clone();
        let out = self.lits_as_written(rc, lits, seed)?;
        let order = self.binding_order(rc, &outer_names(rc, &outer))?;
        if let Some(s) = &mut self.gather {
            s.order(&mut self.program, &order, &outer);
        }
        Ok(out)
    }

    /// The order the clause just built holds in (R-10,
    /// [`crate::program::bind`]), what is wrong with it said; `outer` the
    /// source names bound around it.
    fn binding_order(&mut self, rc: &Rc, outer: &BTreeSet<String>) -> L<Vec<usize>> {
        let clause = self.gather.as_ref().and_then(|g| g.body());
        let Some(clause) = clause.filter(|_| !(self.lenient || self.core)) else {
            let n = clause.map_or(0, |c| self.program.clauses[c].goals.len());
            return Ok((0..n).collect());
        };
        let cx = crate::program::bind::Context {
            outer,
            scope: rc.scope,
        };
        let bound = crate::program::bind::order(&self.program, clause, &cx);
        if !bound.diags.is_empty() {
            self.diags.extend(bound.diags);
            return Err(Skip);
        }
        Ok(bound.order)
    }

    fn lits_as_written(
        &mut self,
        rc: &mut Rc,
        lits: &[SyntaxNode],
        mut out: Vec<Lit>,
    ) -> L<Vec<Lit>> {
        // A statement's clause is gathered whole (R-211 step 4); a body
        // nested in a literal, with the literal.
        let whole = self.nested == 0 && self.gather.is_some() && !lits.is_empty();
        if self.nested == 0
            && let Some(s) = &mut self.gather
        {
            // The clause gathered is this body's, or none.
            s.take_clause();
            if whole {
                s.open();
            }
        }
        let mut failed = false;
        for l in lits {
            if self.lit(rc, l, &mut out).is_err() {
                failed = true;
            }
        }
        if whole {
            let span = self.span(&lits[0]);
            if let Some(s) = &mut self.gather {
                match failed {
                    true => s.failed(),
                    false => s.clause(&mut self.program, span),
                }
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// One literal, with the reads it hoists before it, into `out`.
    fn lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        self.lit_as(rc, n, out, Self::lit1)
    }

    /// [`Self::lit`] of `n` lowered by `lower`: gathered, the goal it
    /// builds the literal's (R-211 step 7c).
    fn lit_as(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        lower: impl FnOnce(&mut Self, &mut Rc, &SyntaxNode, &mut Vec<Lit>) -> L<()>,
    ) -> L<()> {
        let saved = std::mem::take(&mut self.after);
        let saved_goals = std::mem::take(&mut self.after_goals);
        let saved_tests = std::mem::take(&mut rc.tests);
        let start = out.len();
        if let Some(s) = &mut self.gather {
            s.open();
        }
        let r = self.bind(true, |l| lower(l, rc, n, out));
        let tests = std::mem::replace(&mut rc.tests, saved_tests);
        out.append(&mut self.after);
        self.after = saved;
        let after = std::mem::replace(&mut self.after_goals, saved_goals);
        if !after.is_empty()
            && let Some(s) = &mut self.gather
            && let Some(g) = s.take_built()
        {
            let span = self.span(n);
            let kind = GoalKind::Group {
                goals: vec![g],
                after,
            };
            let g = self.goal(rc, span, kind);
            self.built(g);
        }
        match (r.is_ok(), &mut self.gather) {
            (true, Some(_)) => self.built_literal(n, &out[..start], &tests),
            (false, Some(s)) => s.failed(),
            _ => {}
        }
        r
    }

    /// The written literal `n`, after the clause's `context`, made after
    /// the `tests`: the goal built of it gathered (R-211 step 4).
    fn built_literal(
        &mut self,
        n: &SyntaxNode,
        context: &[Lit],
        tests: &[(Option<ExprId>, GoalId)],
    ) {
        let span = self.span(n);
        let Some(s) = &mut self.gather else { return };
        s.literal(&mut self.program, span, context, tests, &self.helpers);
    }

    /// `v in PATH`, PATH with a `[_]` past its type's (R-162): `v` is
    /// each value the path reaches.
    fn each_membership(&self, rc: &Rc, n: &SyntaxNode) -> bool {
        terms(n)
            .nth(1)
            .and_then(|t| Chain::of(&t))
            .is_some_and(|c| self.type_each(rc, c).ops.iter().any(each::is_each))
    }

    /// A membership, positive whatever `n` says: the body of the helper
    /// a negated one lowers through.
    fn member_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let (lit, g) = self.membership(rc, n, out)?;
        out.push(lit);
        if let Some(g) = g {
            self.built(g);
        }
        Ok(())
    }

    fn lit1(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let span = self.span(n);
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content, out)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                self.built(g);
                out.push(Lit::Pos(atom));
            }
            LIT_TRUTH => self.truth_lit(rc, n, out, span)?,
            LIT_CMP => self.cmp(rc, n, out)?,
            LIT_IN => self.member_lit(rc, n, out)?,
            // `v not in PATH[_]`: no value the path reaches is `v`, a
            // helper over the membership (R-219).
            LIT_NOT_IN if self.each_membership(rc, n) => self.neg_helper(rc, n, out, span)?,
            LIT_NOT_IN => {
                let (lit, g) = self.membership(rc, n, out)?;
                out.push(negate(lit));
                if let Some(g) = g {
                    let g = self.building(rc, span, |b| b.not_one(g, Negated::NotIn));
                    self.built(g);
                }
            }
            LIT_HAS => self.has_lit(rc, n, out, span)?,
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

    /// `x.ready`: the read with `true` in its value column, or the value
    /// compared with `true`.
    fn truth_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>, span: Span) -> L<()> {
        let c = terms(n).next().and_then(|t| Chain::read(&t)).ok_or(Skip)?;
        let mark = out.len();
        let res = self.resolve(rc, &c, out)?;
        let e = match self.read_atom(&res, Term::Val(Value::Bool(true)), span) {
            Some(a) => {
                out.push(Lit::Pos(a));
                self.in_place(rc, &res, span).ok_or(Skip)?
            }
            None => {
                let local = out.split_off(mark);
                let id = self.realize_expr(rc, res, Pos::Content, (local, out), span)?;
                let t = self.lowered(id, out);
                out.push(Lit::Eq(t, Term::Val(Value::Bool(true))));
                id
            }
        };
        let g = self.goal(rc, span, GoalKind::Truth(e));
        self.built(g);
        Ok(())
    }

    /// `has r`, `has r.p`, `has x.f`: whether the resource exists, the
    /// attribute or the path into a value has one.
    fn has_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>, span: Span) -> L<()> {
        let t = terms(n).next().ok_or(Skip)?;
        let c = Chain::read(&t).ok_or(Skip)?;
        if self.bare_resource(rc, &c) {
            let (typ, addr) = self.reference(rc, &c, out, span)?;
            let args = vec![self.term_of(typ), self.term_of(addr)];
            out.push(Lit::Pos(atom_at(IDENTITY, args, span)));
            let g = self.goal(rc, span, GoalKind::Has(Has::Resource { typ, addr }));
            self.built(g);
            return Ok(());
        }
        let res = self.resolve(rc, &c, out)?;
        match &res {
            Res::Ref { typ, addr, path } if path.is_empty() => {
                let args = vec![self.term_of(*typ), self.term_of(*addr)];
                let a = atom_at(IDENTITY, args, span);
                out.push(Lit::Pos(a));
                let (typ, addr) = (*typ, *addr);
                let g = self.goal(rc, span, GoalKind::Has(Has::Resource { typ, addr }));
                self.built(g);
                return Ok(());
            }
            Res::Ref { path, .. } if is_identity(path) => {
                return self.has_identity(&t, span);
            }
            _ => {}
        }
        self.has_path(rc, res, out, span)
    }

    /// `has R.p`, `has x.f`, `has R.p.q` of what `res` resolved: the read
    /// itself, or the walk to the path; a resource's attribute path marked
    /// as the test (R-106).
    fn has_path(&mut self, rc: &mut Rc, res: Res, out: &mut Vec<Lit>, span: Span) -> L<()> {
        let start = out.len();
        let marked = self.has_atom(&res, span);
        let mark = self.has_mark(&res);
        // The reads the mark covers are the test's own: where the
        // schema answers it they are gone (R-106), so nothing
        // after it reads through them (R-204).
        let reads = marked.as_ref().map(|_| rc.reads.clone());
        let mut before = Vec::new();
        let test = match self.read_atom(&res, Term::Wildcard, span) {
            Some(a) => {
                out.push(Lit::Pos(a));
                // A marked read's resource is the mark's.
                let res = match mark {
                    Some(_) => self.settled(rc, res, span),
                    None => res,
                };
                Has::Read(self.in_place(rc, &res, span).ok_or(Skip)?)
            }
            // A nested path, or a field of a value: it has a value
            // when the walk to it does.
            None if matches!(res, Res::Ref { .. } | Res::Var { .. } | Res::Value { .. }) => {
                // The reads resolving it stay before the mark, or before
                // the walk when no mark holds them.
                if mark.is_none() {
                    before = self.settled_reads(rc, &res, span);
                }
                let res = self.settled(rc, res, span);
                let value = self.realize_expr(rc, res, Pos::Content, (Vec::new(), out), span)?;
                let t = self.lowered(value, out);
                let v = fresh(rc, HAS_VAR);
                out.push(Lit::Eq(var(&v), t));
                let var = self.building(rc, span, |b| b.var(&v));
                rc.inner.insert(v);
                Has::Walk { var, value }
            }
            None => {
                return self.error(
                    span,
                    "`has` takes an attribute of a resource (`has r.p`), a value name \
                     or a field of a value",
                );
            }
        };
        mark_has(out, start, marked, false);
        if let Some(reads) = reads {
            rc.reads = reads;
        }
        let mut g = self.goal(rc, span, GoalKind::Has(test));
        if let Some(mark) = mark {
            g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
        }
        let g = self.after(rc, span, before, g);
        self.built(g);
        Ok(())
    }

    /// `not L`: an atom, a read with a value, a membership; anything else
    /// through a helper relation.
    fn not(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>, span: Span) -> L<()> {
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content, out)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                self.built(g);
                out.push(Lit::Not(atom));
                return Ok(());
            }
            LIT_IN
                if !n.descendants().any(|d| d.kind() == TUPLE) && !self.each_membership(rc, n) =>
            {
                let (lit, g) = self.membership(rc, n, out)?;
                out.push(negate(lit));
                if let Some(g) = g {
                    let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                    self.built(g);
                }
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
                    .then(|| self.has_atom(&res, span))
                    .flatten();
                if let Some(a) = self.read_atom(&res, value, span) {
                    out.extend(pre);
                    let start = out.len();
                    out.push(Lit::Not(a));
                    mark_has(out, start, marked, true);
                    let mark = (n.kind() == LIT_HAS).then(|| self.has_mark(&res)).flatten();
                    // A marked read's resource is the mark's.
                    let res = match mark {
                        Some(_) => self.settled(rc, res, span),
                        None => res,
                    };
                    let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                    let kind = match n.kind() {
                        LIT_HAS => GoalKind::Has(Has::Read(e)),
                        _ => GoalKind::Truth(e),
                    };
                    let g = self.goal(rc, span, kind);
                    let mut g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                    if let Some(mark) = mark {
                        g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
                    }
                    self.built(g);
                    return Ok(());
                }
                if marked.is_some() {
                    let start = out.len();
                    self.neg_helper(rc, n, out, span)?;
                    mark_has(out, start, marked, true);
                    let mark = self.has_mark(&res).map(|m| {
                        let (typ, addr) =
                            self.building(rc, span, |b| (b.settled(m.typ), b.settled(m.addr)));
                        HasMark { typ, addr, ..m }
                    });
                    if let (Some(mark), Some(s)) = (mark, &mut self.gather)
                        && let Some(g) = s.take_built()
                    {
                        let g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
                        self.built(g);
                    }
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
                        let Ok(id) = self.probe(|l| {
                            l.bind(false, |l| l.expr(&mut rc2, v, Pos::Content, &mut vpre))
                        }) else {
                            continue;
                        };
                        let value = self.lowered(id, &mut vpre);
                        if !vpre.is_empty() {
                            continue;
                        }
                        if let Some(a) = self.read_atom(&res, value, span) {
                            *rc = rc2;
                            out.extend(pre);
                            out.push(Lit::Not(a));
                            let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                            let ops = vec![(CmpOp::Eq, id)];
                            let g = self.goal(rc, span, GoalKind::Compare { lhs: e, ops });
                            let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                            self.built(g);
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
        let body_from = self.gather.as_ref().map(|s| s.goals());
        let mut inner = Vec::new();
        let mut rc2 = rc.clone();
        rc2.reads.clear();
        rc2.values.clear();
        self.nested += 1;
        let lowered = match n.kind() {
            BODY => self.body(&mut rc2, n).map(|b| inner = b),
            // `v not in PATH[_]`: the helper's body is the membership.
            LIT_NOT_IN => self.lit_as(&mut rc2, n, &mut inner, Self::member_lit),
            _ => self.lit(&mut rc2, n, &mut inner),
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
            b.extend(rc.bound_around().cloned());
            b
        };
        let mut used = BTreeSet::new();
        for l in &inner {
            lit_vars(l, &mut used);
        }
        let shared: Vec<String> = used.intersection(&outer_bound).cloned().collect();
        let number = self.program.helpers.neg();
        let pred = format!("__neg_{number}");
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
        self.helpers.push(Stmt::Rule(RuleStmt::helper(
            Helper::Negation,
            head.clone(),
            body,
        )));
        if let (Some(s), Some(from)) = (&mut self.gather, body_from) {
            s.negation(
                number,
                results.iter().map(|v| v.to_string()).collect(),
                from,
            );
            s.negated(&mut self.program, &shared, span, negated(n));
        }
        // The helper's own variables are bound in the helper.
        for v in used.difference(&outer_bound) {
            rc.inner.insert(v.clone());
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
                let mark = out.len();
                let res = self.resolve(rc, &list, out)?;
                let local = out.split_off(mark);
                let lid = self.realize_expr(rc, res, Pos::Content, (local, out), span)?;
                let l = self.lowered(lid, out);
                let (i, iid) = self.bind(true, |x| x.arg_node(rc, &ix, Pos::Content, out))?;
                let (pat, pid) = self.pattern(rc, p, out)?;
                out.push(Lit::Pos(atom_at("member", vec![l, i, pat], span)));
                let g = self.building(rc, span, |b| {
                    let value = b.element_in_place(lid, iid);
                    let pat = b.pattern_of(pid);
                    b.bind_goal(span, pat, value)
                });
                self.built(g);
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
                let Ok(id) = self
                    .probe(|l| l.bind(binding, |l| l.expr(&mut rc2, v, Pos::Content, &mut vpre)))
                else {
                    continue;
                };
                let value = self.lowered(id, &mut vpre);
                if !vpre.is_empty() {
                    continue;
                }
                if let Some(a) = self.read_atom(&res, value, span) {
                    *rc = rc2;
                    out.extend(pre);
                    out.push(Lit::Pos(a));
                    let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                    let g = self.building(rc, span, |b| match binding {
                        true => {
                            let pat = b.pattern_of(id);
                            b.bind_goal(span, pat, e)
                        }
                        false => {
                            let kind = GoalKind::Compare {
                                lhs: e,
                                ops: vec![(CmpOp::Eq, id)],
                            };
                            b.goal_node(span, kind)
                        }
                    });
                    self.built(g);
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
            let mut ids = Vec::new();
            for (i, t) in ts.iter().enumerate() {
                let binding = i == 0 && ops[0] == EQ;
                ids.push(self.bind(binding, |l| l.ref_expr(rc, t, Pos::Content, out))?);
            }
            let (a, b) = (self.lowered(ids[0], out), self.lowered(ids[1], out));
            out.push(if ops[0] == NEQ {
                Lit::Neq(a, b)
            } else {
                Lit::Eq(a, b)
            });
            let pair = match ops[0] {
                NEQ => vec![(CmpOp::Ne, ids[0], ids[1])],
                _ => vec![(CmpOp::Eq, ids[0], ids[1])],
            };
            let g = self.compare_goal(rc, span, ops[0] == EQ, pair);
            self.built(g);
            return Ok(());
        }
        let mut lowered = Vec::new();
        let mut ids = Vec::new();
        for (i, t) in ts.iter().enumerate() {
            // `=` binds either side; `==`, `!=` and the orders test.
            let binding = ops.get(i.saturating_sub(1)) == Some(&EQ) && i <= 1
                || (i == 0 && ops.first() == Some(&EQ));
            let (t, id) = self.bind(binding, |l| l.arg_node(rc, t, Pos::Content, out))?;
            lowered.push(t);
            ids.push(id);
        }
        let mut pairs = Vec::new();
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
            let (was_a, was_b) = (a.clone(), b.clone());
            let (a, b) = match crate::types::operands(sign, a, b) {
                Ok(ab) => ab,
                Err(why) => return self.error(span, why),
            };
            let ia = self.read_as(ids[i], &was_a, a.clone());
            let ib = self.read_as(ids[i + 1], &was_b, b.clone());
            let (lit, cmp) = match op {
                EQ | EQ2 => (Lit::Eq(a, b), CmpOp::Eq),
                NEQ => (Lit::Neq(a, b), CmpOp::Ne),
                LT => (Lit::Lt(a, b), CmpOp::Lt),
                LE => (Lit::Le(a, b), CmpOp::Le),
                GT => (Lit::Gt(a, b), CmpOp::Gt),
                _ => (Lit::Ge(a, b), CmpOp::Ge),
            };
            out.push(lit);
            pairs.push((cmp, ia, ib));
        }
        let g = self.compare_goal(rc, span, ops[0] == EQ, pairs);
        self.built(g);
        Ok(())
    }

    /// The goal of a comparison written at `span`, its pairs `pairs` as
    /// typed: `x = e` binds (`bind`, its first operator `=`); a chain whose
    /// middle terms each pair reads alike is one comparison, else a goal
    /// per pair.
    fn compare_goal(
        &mut self,
        rc: &Rc,
        span: Span,
        bind: bool,
        pairs: Vec<(CmpOp, ExprId, ExprId)>,
    ) -> GoalId {
        let term = |l: &Self, id: ExprId| format!("{:?}", l.term_of(id));
        let shared = pairs
            .windows(2)
            .all(|p| term(self, p[0].2) == term(self, p[1].1));
        self.building(rc, span, |b| {
            if pairs.len() > 1 && shared {
                let lhs = pairs[0].1;
                let ops = pairs.iter().map(|(op, _, x)| (*op, *x)).collect();
                return b.goal_node(span, GoalKind::Compare { lhs, ops });
            }
            let mut goals: Vec<GoalId> = pairs
                .into_iter()
                .map(|(op, x, y)| match (op, bind) {
                    (CmpOp::Eq, true) => {
                        let pat = b.pattern_of(x);
                        b.bind_goal(span, pat, y)
                    }
                    _ => {
                        let kind = GoalKind::Compare {
                            lhs: x,
                            ops: vec![(op, y)],
                        };
                        b.goal_node(span, kind)
                    }
                })
                .collect();
            match goals.len() {
                1 => goals.pop().expect("one"),
                _ => {
                    let kind = GoalKind::Group {
                        goals,
                        after: Vec::new(),
                    };
                    b.goal_node(span, kind)
                }
            }
        })
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
        let negs = self.program.helpers.negs;
        let r = f(self);
        if r.is_err() {
            self.diags.truncate(n);
            self.helpers.truncate(helpers);
            self.program.helpers.negs = negs;
        }
        r
    }

    /// `x in coll` at `span`, `x` the node `x` in a binding position.
    fn member_goal(&mut self, rc: &Rc, span: Span, x: ExprId, coll: Coll) -> GoalId {
        self.building(rc, span, |b| {
            let pat = b.pattern_of(x);
            b.goal_node(span, GoalKind::Member { pat, coll })
        })
    }

    /// `x in list` at `span`: a list written as the element is a value,
    /// not a tuple (a tuple is a key and a value).
    fn element_goal(&mut self, rc: &Rc, span: Span, x: ExprId, list: ExprId) -> GoalId {
        self.building(rc, span, |b| {
            let pat = b.element_of(x);
            let coll = Coll::Expr(list);
            b.goal_node(span, GoalKind::Member { pat, coll })
        })
    }

    /// A namespace's or a renamed type's test as the collection a
    /// membership ranges over: enumerated by a `want` after it, or a test
    /// alone.
    fn tested(coll: Coll, enumerate: bool) -> Coll {
        match coll {
            Coll::Namespace { ns, types, typ, .. } => Coll::Namespace {
                ns,
                types,
                typ,
                enumerate,
            },
            Coll::ProviderType {
                typ, names, var, ..
            } => Coll::ProviderType {
                typ,
                names,
                var,
                enumerate,
            },
            coll => coll,
        }
    }

    /// The resource attribute `t` names, `(T, A, path)`, when it is one
    /// at a constant path (`w.spec.template.spec.containers`). Resolved on
    /// the side: the caller lowers `t` itself.
    fn resource_list(&mut self, rc: &Rc, t: &SyntaxNode) -> Option<Elem> {
        let c = Chain::of(t).filter(|c| !c.is_bare())?;
        let (diags, helpers, negs) = (
            self.diags.len(),
            self.helpers.len(),
            self.program.helpers.negs,
        );
        let mut rc = rc.clone();
        let res = self.probe(|l| l.resolve(&mut rc, &c, &mut Vec::new()));
        self.diags.truncate(diags);
        self.helpers.truncate(helpers);
        self.program.helpers.negs = negs;
        match res {
            Ok(Res::Ref { typ, addr, path }) => {
                let list = self.path_string(&path).filter(|p| !p.is_empty())?;
                Some(Elem {
                    typ: (self.term_of(typ), typ),
                    addr: (self.term_of(addr), addr),
                    list,
                })
            }
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
    ) -> L<(ExprId, ExprId)> {
        if c.is_bare()
            && let Some(types) = self.resource(rc.scope, &c.head)
        {
            let typ = match &self.want {
                _ if types.len() == 1 => &types[0],
                Want::Type(t) if types.contains(t) => t,
                _ => return self.ambiguous(&c.head, &types, span),
            };
            let typ = typ.clone();
            let typ = self.str_node(span, &typ);
            return Ok((typ, self.resource_addr(rc.scope, &c.head, span)));
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
    fn deferred_ref(&mut self, rc: &Rc, name: &str, span: Span) -> Option<ExprId> {
        let types = self.resource(rc.scope, name)?;
        if self.want != Want::Schema || types.len() < 2 {
            return None;
        }
        let int = |l: &mut Self, i: u32| l.node(span, ExprKind::Lit(Value::Int(i64::from(i))));
        let mut args = vec![
            self.str_node(span, name),
            int(self, span.file),
            int(self, span.start),
            int(self, span.end),
        ];
        for t in &types {
            let typ = self.str_node(span, t);
            let addr = self.resource_addr(rc.scope, name, span);
            let empty = self.str_node(span, "");
            let r = vec![typ, addr, empty];
            args.push(self.building(rc, span, |b| b.form(crate::address::REF, r)));
        }
        Some(self.building(rc, span, |b| b.form(crate::types::AMBIGUOUS_REF, args)))
    }

    /// A relation atom, its arguments lowered at `pos`: positional, or
    /// named by the relation's columns (`p(a: x)`, H-12); its arguments'
    /// nodes, and how the read reaches the relation.
    fn atom_args(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<(Atom, AtomArgs, Reach)> {
        let span = self.span(n);
        if let Some((a, args)) = self.exported_relation(rc, n, pos, pre)? {
            return Ok((a, args, Reach::Own));
        }
        let (pred, via) = self.relation_named(rc, n, span)?;
        let list = node(n, ARG_LIST);
        let named: Vec<SyntaxNode> = list
            .iter()
            .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG))
            .collect();
        if !named.is_empty() {
            if list.iter().any(|l| terms(l).next().is_some()) {
                return self.error(
                    span,
                    "a relation's arguments are all positional or all named",
                );
            }
            let (a, args) = self.named_args(rc, pred, &named, pos, pre, span)?;
            return Ok((a, args, via));
        }
        let terms_of = |l: &Option<SyntaxNode>| -> Vec<SyntaxNode> {
            l.as_ref().map(|l| terms(l).collect()).unwrap_or_default()
        };
        let (a, args) = match crate::zset::REF_RELATIONS.iter().find(|(p, ..)| *p == pred) {
            Some(_) => self.ref_relation_args(rc, pred, &terms_of(&list), pos, pre, span)?,
            // A program relation's reference column (R-185): a resource
            // there is the reference, as in the plan's relations.
            None if !self
                .ref_columns(
                    rc.scope,
                    &pred,
                    list.as_ref().map_or(0, |l| terms(l).count()),
                )
                .is_empty() =>
            {
                let (args, nodes) = self.ref_args(rc, &pred, list.as_ref(), pos, pre)?;
                let a = Atom {
                    pred,
                    args,
                    record: None,
                    span,
                };
                (a, AtomArgs::Positional(nodes))
            }
            None if pos == Pos::Content => {
                self.content_args(rc, pred, &terms_of(&list), pos, pre, span)?
            }
            None => {
                let (args, nodes) = self.args(rc, n, pos, pre)?;
                Self::positional(pred, args.into_iter().zip(nodes).collect(), span)
            }
        };
        Ok((a, args, via))
    }

    /// The relation the atom `n` names, and how the read reaches it: a
    /// used module's (`releases.release(..)`, the activation's
    /// `releases::release`, R-65), the scope's around a component's
    /// (`super.p(..)`, R-186), the module's own by its name in its file,
    /// or one in scope by its name.
    fn relation_named(&mut self, rc: &Rc, n: &SyntaxNode, span: Span) -> L<(String, Reach)> {
        let Some(pred) = self.callee(n) else {
            return self.error(span, "a relation is named by a plain name (`p` or `m.p`)");
        };
        if let Some((m, p)) = pred.split_once('.')
            && let Some(path) = self.use_in(rc.scope, m)
        {
            let Some(scope) = self.module_at(&path).map(|m| m.0) else {
                return Err(Skip);
            };
            if !self.names(scope).is_relation(p) {
                return self.error(span, format!("the module {path} has no relation `{p}`"));
            }
            return Ok((format!("{m}::{p}"), Reach::Own));
        }
        if let Some(c) = n.children().find_map(|c| Chain::of(&c))
            && c.head == "super"
        {
            let (from, rest) = self.super_chain(rc, &c, span)?;
            let p = &rest.head;
            let known = self.chain_of(from).into_iter().any(|s| {
                let sc = self.names(s);
                sc.is_relation(p) || sc.takes(p)
            });
            if !rest.ops.is_empty() || !known {
                let around = self.scope_name(from);
                return self.error(span, format!("`super.{p}`: {around} has no relation `{p}`"));
            }
            return Ok((p.clone(), self.relation_reach(rc.scope, from, p)));
        }
        if let Some((m, p)) = pred.split_once('.')
            && let Some(from) = self.self_module(rc.scope, m)
            && self.names(from).is_relation(p)
        {
            // The instance's the read is in (R-186).
            return Ok((p.to_string(), self.relation_reach(rc.scope, from, p)));
        }
        if !pred.contains('.') {
            self.undeclared_relation(rc, &pred, span)?;
            let via = self.relation_reach(rc.scope, rc.scope, &pred);
            return Ok((pred, via));
        }
        Ok((pred, Reach::Own))
    }

    /// `pred(a: x, ..)`: the arguments `named` by the relation's columns.
    fn named_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        named: &[SyntaxNode],
        pos: Pos,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        let mut fields = BTreeMap::new();
        let mut nodes = Vec::new();
        for f in named {
            let key = tokens(f).next().ok_or(Skip)?.text().to_string();
            let (value, id) = self.arg_node(rc, &terms(f).next().ok_or(Skip)?, pos, pre)?;
            if fields.insert(key.clone(), value).is_some() {
                return self.error(self.span(f), format!("column `{key}` given twice"));
            }
            nodes.push((key, id));
        }
        let a = Atom {
            pred,
            args: Vec::new(),
            record: Some(fields),
            span,
        };
        Ok((a, AtomArgs::Record(nodes)))
    }

    /// A relation of the plan's whose one column holds a reference
    /// (`zset::REF_RELATIONS`): that argument the reference.
    fn ref_relation_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        list: &[SyntaxNode],
        pos: Pos,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        if let Err(e) = crate::zset::ref_arity(&pred, list.len()) {
            return self.error(span, e);
        }
        let at = crate::zset::ref_column(&pred, list.len());
        let mut args = Vec::new();
        for (i, t) in list.iter().enumerate() {
            args.push(if Some(i) == at {
                let id = self.ref_expr(rc, t, pos, pre)?;
                (self.lowered(id, pre), id)
            } else {
                self.arg_node(rc, t, pos, pre)?
            });
        }
        // `moved(T, "old-address", r)`: the old address is a path too
        // (R-112).
        if pred == "moved" {
            self.address_key(&args[1].0, span)?;
        }
        Ok(Self::positional(pred, args, span))
    }

    /// In a body an argument is a pattern (R-58): `pair((a, b))`; a
    /// relation of several named columns takes one object pattern, `zone({
    /// name, index })`, its record pattern.
    fn content_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        list: &[SyntaxNode],
        pos: Pos,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        if let [o] = list
            && o.kind() == OBJECT
            && self.named_columns(rc.scope, &pred)
        {
            let (record, nodes) = self.record_pattern(rc, o, pre)?;
            let a = Atom {
                pred,
                args: Vec::new(),
                record: Some(record),
                span,
            };
            return Ok((a, nodes));
        }
        let mut args = Vec::new();
        for t in list {
            args.push(match t.kind() {
                TUPLE => self.pattern(rc, t, pre)?,
                _ => self.arg_node(rc, t, pos, pre)?,
            });
        }
        Ok(Self::positional(pred, args, span))
    }

    /// The atom `pred(args)` at `span`, each argument its term and node.
    fn positional(pred: String, args: Vec<(Term, ExprId)>, span: Span) -> (Atom, AtomArgs) {
        let (args, nodes): (Vec<Term>, Vec<ExprId>) = args.into_iter().unzip();
        let a = Atom {
            pred,
            args,
            record: None,
            span,
        };
        (a, AtomArgs::Positional(nodes))
    }

    /// A relation's argument at `pos`: its term, the reads it makes into
    /// `pre`, and its node.
    fn arg_node(
        &mut self,
        rc: &mut Rc,
        t: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<(Term, ExprId)> {
        let id = self.expr(rc, t, pos, pre)?;
        Ok((self.lowered(id, pre), id))
    }

    /// The goal of the atom `a` whose arguments are `args`: its relation's.
    fn rel_goal(&mut self, rc: &Rc, (a, via): (&Atom, Reach), args: AtomArgs) -> GoalId {
        let span = a.span;
        let rel = RelRef::reached(a.pred.clone(), via, span);
        let args = self.rel_args(rc, span, args);
        self.goal(rc, span, GoalKind::Rel { rel, args })
    }

    /// The atom's arguments `args` at `span`, each in a binding position.
    fn rel_args(&mut self, rc: &Rc, span: Span, args: AtomArgs) -> RelArgs {
        self.building(rc, span, |b| match args {
            AtomArgs::Positional(xs) => {
                RelArgs::Positional(xs.into_iter().map(|x| b.pattern_of(x)).collect())
            }
            AtomArgs::Record(xs) => {
                RelArgs::Record(xs.into_iter().map(|(k, x)| (k, b.pattern_of(x))).collect())
            }
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
    ) -> L<Option<(Atom, AtomArgs)>> {
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
            let Res::Output { inst, key, of, .. } = self.deployed_path(rc, &c, &d, pre, span)?
            else {
                return Err(Skip);
            };
            let read = PlanRead::Output {
                copy: inst,
                key,
                of,
            };
            let rows = self.read_var(rc, "output", read, 2, &p, pre, span);
            let (cols, nodes) = self.args(rc, n, pos, pre)?;
            let parts = nodes.into_iter().map(ListPart::Elem).collect();
            let list = self.node(span, ExprKind::List { parts });
            let a = atom_at("member", vec![self.term_of(rows), Term::List(cols)], span);
            return Ok(Some((a, AtomArgs::Positional(vec![rows, list]))));
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
                .module_at(path)
                .is_some_and(|(m, _)| self.names(m).exports(&p));
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
        let inst_term = self.term_of(inst);
        // A column the component's relation holds references in is the
        // reference here too (R-204): `s` in `blue.subnet(s)` is the
        // subnet, its address what `s in net.subnet` reads.
        let list: Vec<SyntaxNode> = node(n, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let (cols, nodes) = match self.exported_ref_columns(rc, n, list.len()) {
            Some(refs) if !refs.is_empty() => self.ref_args_by(rc, &p, &refs, &list, pos, pre)?,
            _ => {
                let mut cols = Vec::new();
                for t in &list {
                    cols.push(self.arg_node(rc, t, pos, pre)?);
                }
                cols.into_iter().unzip()
            }
        };
        let a = atom_at(
            crate::modules::ROWS,
            vec![inst_term, str_term(&p), Term::List(cols)],
            span,
        );
        let parts = nodes.into_iter().map(ListPart::Elem).collect();
        let list = self.node(span, ExprKind::List { parts });
        let args = AtomArgs::Positional(vec![inst, self.str_node(span, &p), list]);
        Ok(Some((a, args)))
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
        self.module_at(&at).is_some_and(|m| m.1).then_some(at)
    }

    /// The scope whose copies of the component `path` a read in this body
    /// sees: the body's own when it makes one, else its user's (the
    /// `instance_of` user column, as `c[t]` reads it).
    fn copies_scope(&mut self, rc: &Rc, path: &str, span: Span) -> ExprId {
        let own = self
            .own_scopes(rc.scope)
            .iter()
            .find(|s| {
                self.names(**s)
                    .instances()
                    .map(|(_, c)| c)
                    .any(|p| p == path)
            })
            .copied();
        let user = self.str_node(span, "");
        self.scoped(rc.scope, own.unwrap_or(self.root()), user, span)
    }

    /// A copy where a resource is taken (R-67): its name in scope (`blue`)
    /// or a variable `x in network` binds, as a reference `ref(PATH, name,
    /// "")`, the copy's address `network["blue"]`.
    fn instance_ref(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> Option<ExprId> {
        if !c.is_bare() {
            return None;
        }
        let (path, name) = if let Some(path) = rc.instances.get(&c.head).cloned() {
            let v = self.var_named(rc, &c.head, span);
            (path, self.var_node(rc, span, &v))
        } else {
            if rc.vars.contains_key(&c.head) || self.resource(rc.scope, &c.head).is_some() {
                return None;
            }
            let (at, path) = self.instance_in(rc.scope, &c.head)?;
            let name = self.str_node(span, &c.head);
            (path, self.scoped(rc.scope, at, name, span))
        };
        let args = vec![self.str_node(span, &path), name, self.str_node(span, "")];
        Some(self.building(rc, span, |b| b.form(crate::address::REF, args)))
    }

    /// A resource where a relation takes one (R-42) as its node, the reads
    /// no node of it holds into `pre`.
    fn ref_expr(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<ExprId> {
        let span = self.span(n);
        if n.kind() == LITERAL {
            return self.error(
                span,
                "a resource here, not a value: its name in scope, `T[\"a\"]`, or a variable \
                 (R-42)",
            );
        }
        let Some(c) = Chain::of(n) else {
            return self.expr(rc, n, pos, pre);
        };
        if let Some(r) = self.instance_ref(rc, &c, span) {
            return Ok(r);
        }
        if c.is_bare()
            && !rc.vars.contains_key(&c.head)
            && self.resource(rc.scope, &c.head).is_some()
        {
            let mut local = Vec::new();
            let (typ, addr) = self.reference(rc, &c, &mut local, span)?;
            let empty = self.str_node(span, "");
            let args = vec![typ, addr, empty];
            return Ok(self.building(rc, span, |b| b.form(crate::address::REF, args)));
        }
        let mut local = Vec::new();
        match self.resolve(rc, &c, &mut local)? {
            Res::Ref { typ, addr, path } if path.is_empty() => {
                let empty = self.str_node(span, "");
                let args = vec![typ, addr, empty];
                let id = self.building(rc, span, |b| b.form(crate::address::REF, args));
                let left = self.building(rc, span, |b| b.not_held(id, local));
                pre.extend(left);
                Ok(id)
            }
            res => {
                if c.is_bare()
                    && let Res::Val(v) = &res
                    && matches!(self.program.exprs[*v].kind, ExprKind::Var(_))
                {
                    rc.untyped_refs.insert(c.head.clone());
                }
                self.realize_expr(rc, res, pos, (local, pre), span)
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
    ) -> L<ExprId> {
        let arg = terms(list).next().ok_or(Skip)?;
        let r = self.ref_expr(rc, &arg, Pos::Content, pre)?;
        match self.term_of(r) {
            Term::Func { .. } => {
                Ok(self.building(rc, span, |b| b.form(crate::address::REF, vec![r])))
            }
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
                    format!(
                        "`{name}` is an aggregate: name its result after `where`, `n = \
                         {name}(x)`"
                    ),
                )
                .with_help(format!(
                    "`p(k, n) where n = {name}(x), ..` gives one `n` per `k`; `let n = \
                     {name}(x) where ..` gives one in all"
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

    /// `n`'s positional arguments at `pos`: each its term and node
    /// ([`Self::arg_node`]).
    fn args(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<(Vec<Term>, Vec<ExprId>)> {
        let mut out = Vec::new();
        for t in node(n, ARG_LIST).iter().flat_map(terms) {
            out.push(self.arg_node(rc, &t, pos, pre)?);
        }
        Ok(out.into_iter().unzip())
    }

    /// The arguments of `n` as parts of what a chain resolves to
    /// ([`Self::part`]).
    fn arg_parts(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<Vec<ExprId>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for t in terms(&list) {
            out.push(self.part(rc, &t, pre)?);
        }
        Ok(out)
    }

    // --- terms ------------------------------------------------------------

    /// A term as its node (R-211 step 7), the reads it makes held by the
    /// node; a read no node of it holds goes into `pre`. The helper
    /// statements made while it is built are its terms'.
    fn expr(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<ExprId> {
        let made = self.helpers.len();
        let id = self.expr_of(rc, n, pos, pre);
        if let Some(s) = &mut self.gather {
            s.term(made..self.helpers.len());
        }
        id
    }

    /// The term the node `id` lowers to, the reads it makes into `pre`.
    fn lowered(&self, id: ExprId, pre: &mut Vec<Lit>) -> Term {
        let (t, reads) = crate::program::lower_expr(&self.program, id);
        pre.extend(reads);
        t
    }

    fn expr_of(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<ExprId> {
        match n.kind() {
            LITERAL => self.literal(rc, n, pre),
            PAREN => {
                let inner = terms(n).next().ok_or(Skip)?;
                self.bind(false, |l| l.expr(rc, &inner, Pos::Content, pre))
            }
            BIN_EXPR => self.binary(rc, n, pre),
            UNARY_EXPR => self.unary(rc, n, pre),
            RANGE => self.range(rc, n, pre),
            SPREAD => self.misplaced_spread(n),
            TUPLE => self.tuple_value(n),
            CHAIN | CALL_CHAIN => self.chain_term(rc, n, pos, pre),
            CALL => self.call(rc, n, pos, pre),
            LIST if !n.children().any(|c| c.kind() == SPREAD) => self.list_expr(rc, n, pos, pre),
            LIST => self.list(rc, n, pos, pre),
            OBJECT => self.object(rc, n, pos, pre),
            COMPREHENSION => self.comprehension(rc, n, pos),
            k => {
                let span = self.span(n);
                self.error(span, format!("unexpected {k:?} as a term"))
            }
        }
    }

    /// `f` with a builder of the statement's nodes at `span`, its variables
    /// the statement's (none outside one).
    fn building<T>(
        &mut self,
        rc: &Rc,
        span: Span,
        f: impl FnOnce(&mut crate::program::Builder) -> T,
    ) -> T {
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        match self.gather.as_mut() {
            Some(g) => {
                let mut b = g.builder(&mut self.program, span, &written);
                let out = f(&mut b);
                g.done(b);
                out
            }
            None => f(&mut crate::program::Builder::new(
                &mut self.program,
                span,
                &written,
            )),
        }
    }

    /// A node of the program at `span`.
    fn node(&mut self, span: Span, kind: ExprKind) -> ExprId {
        self.program.expr_node(span, kind)
    }

    /// The constant string `s`'s node at `span`.
    fn str_node(&mut self, span: Span, s: &str) -> ExprId {
        self.node(span, ExprKind::Lit(Value::Str(s.to_string())))
    }

    /// The goal `kind` at `span`.
    fn goal(&mut self, rc: &Rc, span: Span, kind: GoalKind) -> GoalId {
        self.building(rc, span, |b| b.goal_node(span, kind))
    }

    /// Whether a read built so far in the statement tests the copy in the
    /// scope `scope` itself (an `Output` holding its test).
    fn held(&self, scope: ExprId) -> bool {
        self.gather.as_ref().is_some_and(|g| g.holds(scope))
    }

    /// The written literal being lowered is the goal `g`, built of its
    /// terms' nodes (R-211 step 7c).
    fn built(&mut self, g: GoalId) {
        if let Some(s) = &mut self.gather {
            s.built(g);
        }
    }

    /// What `res` reads when it is one read, its parts as resolved: the
    /// forms a literal tests or binds in place ([`Self::read_atom`]).
    fn in_place_read(res: &Res) -> Option<PlanRead> {
        Some(match res {
            Res::Ref { typ, addr, path } if path.len() == 1 => match &path[0] {
                Seg::F(p) => PlanRead::Attr {
                    typ: *typ,
                    addr: *addr,
                    attr: crate::address::path_key(p).into_owned(),
                },
                Seg::I(_) | Seg::K(_) => return None,
            },
            Res::Output {
                inst,
                key,
                path,
                of,
                ..
            } if path.is_empty() => PlanRead::Output {
                copy: *inst,
                key: key.clone(),
                of: of.clone(),
            },
            Res::World { typ, addr, path } => PlanRead::World {
                typ: typ.clone(),
                addr: *addr,
                path: path.clone(),
            },
            Res::Value { decl, via, path } if path.is_empty() => {
                PlanRead::Value(decl.clone(), via.clone())
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } if path.is_empty() => PlanRead::Lookup {
                rel: pred.clone(),
                args: args.clone(),
                out: *out,
            },
            _ => return None,
        })
    }

    /// The read `res` is, in place, at `span`: its node.
    fn in_place(&mut self, rc: &Rc, res: &Res, span: Span) -> Option<ExprId> {
        let read = Self::in_place_read(res)?;
        Some(self.building(rc, span, |b| b.read_node(read.of())))
    }

    /// The node of the variable `v` (its lowered name) of the statement.
    fn var_node(&mut self, rc: &Rc, span: Span, v: &str) -> ExprId {
        self.building(rc, span, |b| b.variable(v))
    }

    /// The node of a resolved name's term: a constant, a variable (a
    /// variable's type, `x in T` over several), `_`.
    fn name_node(&mut self, rc: &Rc, span: Span, t: &Term) -> ExprId {
        match t {
            Term::Val(v) => self.node(span, ExprKind::Lit(v.clone())),
            Term::Var(v) => self.var_node(rc, span, v),
            Term::Wildcard => self.node(span, ExprKind::Hole),
            t => unreachable!("a resolved name is a constant or a variable: {t:?}"),
        }
    }

    /// A second node of the leaf `id` (a constant, a variable): a node
    /// stands in one place.
    fn leaf_copy(&mut self, id: ExprId) -> ExprId {
        let e = self.program.exprs[id].clone();
        debug_assert!(matches!(e.kind, ExprKind::Lit(_) | ExprKind::Var(_)));
        self.program.exprs.insert(e)
    }

    /// A part of what a chain resolves to (an index, a key, an address):
    /// the term's node, the reads it makes into `pre` too, where the
    /// resolver's literals hold them.
    fn part(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<ExprId> {
        let id = self.expr(rc, n, Pos::Content, pre)?;
        self.lowered(id, pre);
        Ok(id)
    }

    /// The read `id` the resolver hoisted, its own literal into `pre`
    /// (the reads of its parts are there already): the last its lowering
    /// writes.
    fn hoisted_into(&self, id: ExprId, pre: &mut Vec<Lit>) {
        let (_, mut reads) = crate::program::lower_expr(&self.program, id);
        pre.extend(reads.pop());
    }

    /// The term node `id` is, its reads aside: what a typing of a term
    /// reads (`types::operands`, `types::range_ends`).
    fn term_of(&self, id: ExprId) -> Term {
        crate::program::lower_expr(&self.program, id).0
    }

    /// `id`, whose term is `was`, as a typing read it, `now` (an ambiguous
    /// quantity read as the other side's dimension, R-66): the node of the
    /// constant `now` where it differs.
    fn read_as(&mut self, id: ExprId, was: &Term, now: Term) -> ExprId {
        if format!("{now:?}") == format!("{was:?}") {
            return id;
        }
        let Term::Val(v) = now else { return id };
        let span = self.program.exprs[id].span;
        self.node(span, ExprKind::Lit(v))
    }

    /// `id`, whose term is `was`, as a declared type reads it, `now`: a
    /// constant read as the type ([`Self::read_as`]), the value read `as`
    /// it (`__as(v, "T")`), a written list's or object's parts each read
    /// so.
    fn retyped(&mut self, rc: &Rc, id: ExprId, was: &Term, now: Term) -> ExprId {
        if *was == now {
            return id;
        }
        let e = self.program.exprs[id].clone();
        match (&now, was, e.kind) {
            (Term::Func { name, args }, _, _) if name == crate::types::AS => {
                match args.as_slice() {
                    [v, Term::Val(Value::Str(ty))] if v == was => {
                        let ty = TypeExpr::Name(ty.clone());
                        self.node(e.span, ExprKind::As { value: id, ty })
                    }
                    [v, Term::Val(Value::Str(ty))] => {
                        let ty = TypeExpr::Name(ty.clone());
                        let value = self.retyped(rc, id, was, v.clone());
                        self.node(e.span, ExprKind::As { value, ty })
                    }
                    _ => id,
                }
            }
            (Term::List(ns), Term::List(ws), ExprKind::List { parts })
                if e.hoisted.is_none()
                    && ns.len() == parts.len()
                    && ws.len() == parts.len()
                    && parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                let parts = parts
                    .into_iter()
                    .zip(ws.iter().zip(ns))
                    .map(|(p, (w, n))| match p {
                        ListPart::Elem(x) => ListPart::Elem(self.retyped(rc, x, w, n.clone())),
                        p => p,
                    })
                    .collect();
                self.node(e.span, ExprKind::List { parts })
            }
            (Term::Obj(ns), Term::Obj(ws), ExprKind::Object { parts })
                if e.hoisted.is_none()
                    && parts.iter().all(|p| {
                        matches!(p, ObjPart::Field { key, .. }
                            if ws.contains_key(key) && ns.contains_key(key))
                    }) =>
            {
                let parts = parts
                    .into_iter()
                    .map(|p| match p {
                        ObjPart::Field {
                            key,
                            key_span,
                            value,
                            pun,
                        } => {
                            let value = self.retyped(rc, value, &ws[&key], ns[&key].clone());
                            ObjPart::Field {
                                key,
                                key_span,
                                value,
                                pun,
                            }
                        }
                        p => p,
                    })
                    .collect();
                self.node(e.span, ExprKind::Object { parts })
            }
            _ => self.read_as(id, was, now),
        }
    }

    /// A literal: an int, a quantity (R-66), a string, an IPv6 address, a
    /// bool.
    fn literal(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<ExprId> {
        match tokens(n).next() {
            Some(t) if t.kind() == STRING => self.string_expr(rc, &t, pre),
            _ => self.literal_expr(n),
        }
    }

    /// A literal's node: an int, a quantity (one whose dimension its
    /// position decides kept as written), a string read whole (its holes
    /// are [`Lowerer::string_expr`]'s), a bool.
    fn literal_expr(&mut self, n: &SyntaxNode) -> L<ExprId> {
        let span = self.span(n);
        let t = tokens(n).next().ok_or(Skip)?;
        let kind = match t.kind() {
            INT => match crate::lexer::int_value(t.text()) {
                Some(i) => ExprKind::Lit(Value::Int(i)),
                None => return self.error(span, "integer out of range"),
            },
            // A bare IPv6 address is its text, as its quoted spelling is:
            // its position reads it.
            INET => ExprKind::Lit(Value::Str(t.text().to_string())),
            QUANTITY => match crate::quantity::literal(t.text()) {
                Ok(crate::quantity::Literal::Known(q)) => ExprKind::Lit(Value::Quantity(q)),
                Ok(crate::quantity::Literal::Ambiguous) => ExprKind::Quantity {
                    text: t.text().to_string(),
                },
                Err(why) => return self.error(span, why),
            },
            STRING => ExprKind::Lit(Value::Str(self.string(&t)?)),
            TRUE_KW => ExprKind::Lit(Value::Bool(true)),
            _ => ExprKind::Lit(Value::Bool(false)),
        };
        Ok(self.node(span, kind))
    }

    /// A chain: `x.len` (R-155), a resource by its bare name given as a
    /// value (R-43), a read.
    fn chain_term(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<ExprId> {
        let span = self.span(n);
        let mut c = Chain::read(n).ok_or(Skip)?;
        let mut local = Vec::new();
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
            let res = self.resolve(rc, &c, &mut local)?;
            let base = self.realize_expr(rc, res, Pos::Content, (local, pre), span)?;
            let path = vec![crate::program::node::Step::Len];
            return Ok(self.node(span, ExprKind::Field { base, path }));
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
            if let Some(id) = self.deferred_ref(rc, &c.head, span) {
                return Ok(id);
            }
            let (typ, addr) = self.reference(rc, &c, &mut local, span)?;
            let empty = self.str_node(span, "");
            let args = vec![typ, addr, empty];
            return Ok(self.building(rc, span, |b| b.form(crate::address::REF, args)));
        }
        let res = self.resolve(rc, &c, &mut local)?;
        if let Res::Ref { path, .. } = &res
            && matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
        {
            return self.identity_read(n, span);
        }
        self.realize_expr(rc, res, pos, (local, pre), span)
    }

    /// A call of a function, with its named arguments; `env.var`, a
    /// loader, `ref`. A call of a function is its node, its arguments'
    /// nodes; a call the lowering writes as a form of its own (an
    /// operator's function, `ref`, `__path`, ..) and the others are still
    /// built from what they lower to.
    fn call(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<ExprId> {
        let span = self.span(n);
        if let Some(id) = self.env_var_call(rc, n, pos, pre) {
            return id;
        }
        if let Some(id) = self.loader_call(rc, n, pre) {
            return id;
        }
        // A let with parameters (R-187).
        if let Some(id) = self.function_call(rc, n, pre) {
            return id;
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
        if !self.lenient && self.calls != Calls::Data && !crate::functions::callable(&name) {
            let at = n
                .children()
                .find_map(|c| Chain::of(&c))
                .map_or(span, |c| self.span_of(c.range));
            self.near_function(rc.scope, &name, at)?;
        }
        self.check_function(&name, span);
        let mut args = self.bind(false, |l| l.arg_exprs(rc, n, pre))?;
        let mut defaults = Vec::new();
        if let (false, Some(f)) = (named.is_empty(), declared) {
            let mut given = Vec::new();
            for (k, t) in named {
                given.push((
                    k,
                    Some(self.bind(false, |l| l.expr(rc, &t, Pos::Content, pre))?),
                ));
            }
            let placed = crate::functions::with_named(f, args, given, |v| {
                defaults.push(v);
                None
            });
            args = match placed {
                Ok(a) => a,
                Err(why) => return self.error(span, why),
            };
        }
        // A parameter left out before a named one is its default.
        let mut defaults = defaults.into_iter();
        let args: Vec<ExprId> = args
            .into_iter()
            .map(|a| match a {
                Some(a) => a,
                None => {
                    let v = defaults.next().expect("a default per argument left out");
                    self.node(span, ExprKind::Lit(v))
                }
            })
            .collect();
        let was: Vec<Term> = args.iter().map(|&a| self.term_of(a)).collect();
        self.check_aggregated(&name, &was, span);
        // `cloud_ref(T, name, path)`, a form of the language, is
        // the lowering's `__cloud_ref` (R-155).
        let name = match name.as_str() {
            "cloud_ref" => crate::address::CLOUD_REF.to_string(),
            "ref" => crate::address::REF.to_string(),
            _ => name,
        };
        let now = self.typed_args(&name, was.clone(), span)?;
        let args: Vec<ExprId> = args
            .into_iter()
            .zip(was.iter().zip(now))
            .map(|(a, (was, now))| self.read_as(a, was, now))
            .collect();
        Ok(self.building(rc, span, |b| b.form(&name, args)))
    }

    /// A call's positional arguments, each its node.
    fn arg_exprs(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<Vec<Option<ExprId>>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for t in terms(&list) {
            out.push(Some(self.expr(rc, &t, Pos::Content, pre)?));
        }
        Ok(out)
    }

    /// `[a, b]`, written element by element: its node, each element's.
    /// A list with a spread is `spread.rs`'s.
    fn list_expr(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<ExprId> {
        let mut parts = Vec::new();
        for t in terms(n) {
            parts.push(ListPart::Elem(self.expr(rc, &t, pos, pre)?));
        }
        Ok(self.node(self.span(n), ExprKind::List { parts }))
    }

    /// An object, its keys given or computed (`{ "${k}": v }`), its
    /// fields written or spread (`{ ..base, k: v }`, R-199): its node,
    /// each field's value its node.
    fn object(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos, pre: &mut Vec<Lit>) -> L<ExprId> {
        self.written_twice(n)?;
        if n.children().any(|c| c.kind() == SPREAD) {
            return self.spread_object(rc, n, pos, pre);
        }
        let fields: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == OBJECT_FIELD).collect();
        let parts = self.object_fields(rc, &fields, pos, pre)?;
        Ok(self.node(self.span(n), ExprKind::Object { parts }))
    }

    /// Written fields, each its key and its value's node; a key with holes
    /// (`{ "${k}": v }`) is computed, so each key of the fields is (the
    /// object is built at run time, `functions::OBJECT`).
    fn object_fields(
        &mut self,
        rc: &mut Rc,
        fields: &[SyntaxNode],
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Vec<ObjPart>> {
        let computed = self.computed_keys(fields);
        let mut parts = Vec::new();
        for f in fields {
            let k = tokens(f).next().ok_or(Skip)?;
            let key_span = self.span_of(k.text_range());
            let computed_key = match (computed, k.kind()) {
                (false, _) => None,
                (true, STRING) => Some(self.string_expr(rc, &k, pre)?),
                (true, _) => Some(self.node(key_span, ExprKind::Lit(Value::Str(k.text().into())))),
            };
            let key = match k.kind() {
                _ if computed => String::new(),
                STRING => self.string(&k)?,
                _ => k.text().to_string(),
            };
            let (value, pun) = match terms(f).next() {
                Some(t) => (self.expr(rc, &t, pos, pre)?, false),
                // `{ a }` is `{ a: a }`.
                None => {
                    let name = if computed { k.text() } else { &key };
                    (self.punned(rc, name, &k, pos, pre)?, true)
                }
            };
            parts.push(match computed_key {
                Some(key) => ObjPart::Computed { key, value },
                None => ObjPart::Field {
                    key,
                    key_span,
                    value,
                    pun,
                },
            });
        }
        Ok(parts)
    }

    /// Whether a field's key has holes (`{ "${k}": v }`): the object is
    /// built at run time (`functions::OBJECT`).
    fn computed_keys(&self, fields: &[SyntaxNode]) -> bool {
        fields
            .iter()
            .filter_map(|f| tokens(f).next())
            .any(|k| k.kind() == STRING && !self.text && has_hole(k.text()))
    }

    /// `{ a }`'s value: the name `a` read.
    fn punned(
        &mut self,
        rc: &mut Rc,
        key: &str,
        k: &SyntaxToken,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<ExprId> {
        let c = Chain {
            head: key.to_string(),
            head_kind: k.kind(),
            call: None,
            range: k.text_range(),
            ops: Vec::new(),
        };
        let mut local = Vec::new();
        let res = self.resolve(rc, &c, &mut local)?;
        self.realize_expr(rc, res, pos, (local, pre), self.span_of(k.text_range()))
    }

    /// A list comprehension: its body resolved nested, its item over it.
    /// Its node's clause is the goals its literals were built as; outside
    /// a statement, or a literal not built, the clause is built from what
    /// the body lowered to.
    fn comprehension(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let span = self.span(n);
        let body_node = node(n, BODY).ok_or(Skip)?;
        let saved = (
            std::mem::take(&mut rc.reads),
            std::mem::take(&mut rc.values),
        );
        if let Some(g) = &mut self.gather {
            g.open();
        }
        self.nested += 1;
        let body = self.body(rc, &body_node);
        self.nested -= 1;
        let at = self.span_of(body_node.text_range());
        let clause = match &mut self.gather {
            Some(g) => g.close(&mut self.program, at),
            None => None,
        };
        let mut body = body?;
        let item_node = terms(n).next().ok_or(Skip)?;
        // A resource collected into a value is its reference.
        let item_pos = if pos == Pos::Value {
            Pos::Value
        } else {
            Pos::Whole
        };
        let tests = std::mem::take(&mut rc.tests);
        let item = self.bind(false, |l| l.expr(rc, &item_node, item_pos, &mut body))?;
        (rc.reads, rc.values) = saved;
        // A test the item made before a read it holds none of is the
        // body's last goal.
        let item_tests = std::mem::replace(&mut rc.tests, tests);
        let clause = clause.unwrap_or_else(|| self.building(rc, at, |b| b.clause_of(Vec::new())));
        let held = |l: &Self, s: Option<ExprId>| s.is_some_and(|s| l.held(s));
        let unheld: Vec<GoalId> = item_tests
            .into_iter()
            .filter(|&(s, _)| !held(self, s))
            .map(|(_, g)| g)
            .collect();
        self.program.clauses[clause].goals.extend(unheld);
        Ok(self.node(span, ExprKind::Comprehension { item, clause }))
    }

    /// Arithmetic; `us-east` and `us-test-1a`, meant as strings, are
    /// errors that say so.
    fn binary(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<ExprId> {
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let op = tokens(n).next().ok_or(Skip)?;
        // `us-test-1a`, names and a quantity with no spaces (R-66
        // lexes `1a` as one token): meant as a string.
        let word = |k: SyntaxKind| matches!(k, IDENT | QUANTITY | INT | MINUS) || k.is_keyword();
        let toks: Vec<_> = n
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .collect();
        if op.kind() == MINUS
            && toks.iter().all(|t| word(t.kind()))
            && toks.iter().any(|t| t.kind() == QUANTITY)
            && toks.iter().any(|t| t.kind() == IDENT)
        {
            let d = Diagnostic::error(span, format!("`{}` is arithmetic, not a name", n.text()))
                .with_help(format!(
                    "`-` subtracts here, a quantity from a name; a name with a `-` in it is \
                 a string, \"{}\"",
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
            let d = Diagnostic::error(span, format!("`{}` is arithmetic on two names", n.text()))
                .with_help(format!(
                    "`-` subtracts `{}` from `{}`; a name with a `-` in it is a string, \
                 \"{}\"",
                    ts[1].text(),
                    ts[0].text(),
                    n.text()
                ));
            self.diags.push(d);
            return Err(Skip);
        }
        let bin = match op.kind() {
            PLUS => BinOp::Add,
            MINUS => BinOp::Sub,
            STAR => BinOp::Mul,
            SLASH => BinOp::Div,
            _ => BinOp::Mod,
        };
        let a = self.bind(false, |l| l.expr(rc, &ts[0], Pos::Content, pre))?;
        let b = self.bind(false, |l| l.expr(rc, &ts[1], Pos::Content, pre))?;
        let (ta, tb) = (self.term_of(a), self.term_of(b));
        match crate::types::operands(op.text(), ta.clone(), tb.clone()) {
            Ok((x, y)) => {
                let lhs = self.read_as(a, &ta, x);
                let rhs = self.read_as(b, &tb, y);
                Ok(self.node(span, ExprKind::Binary { op: bin, lhs, rhs }))
            }
            Err(why) => self.error(span, why),
        }
    }

    /// A negation: of a literal, its value.
    fn unary(&mut self, rc: &mut Rc, n: &SyntaxNode, pre: &mut Vec<Lit>) -> L<ExprId> {
        let span = self.span(n);
        let inner = terms(n).next().ok_or(Skip)?;
        let e = self.bind(false, |l| l.expr(rc, &inner, Pos::Content, pre))?;
        let kind = match &self.program.exprs[e].kind {
            ExprKind::Lit(Value::Int(i)) if inner.kind() == LITERAL => {
                ExprKind::Lit(Value::Int(-i))
            }
            ExprKind::Lit(Value::Quantity(q)) if inner.kind() == LITERAL => {
                match q.checked_mul(-1) {
                    Some(q) => ExprKind::Lit(Value::Quantity(q)),
                    None => return self.error(span, "quantity out of range"),
                }
            }
            _ => ExprKind::Neg(e),
        };
        Ok(self.node(span, kind))
    }

    /// `lo..hi` (its end left out) or `lo..=hi` (its end in it): a range
    /// (R-180), the value when its ends are known, else
    /// `__range(lo, hi, inclusive)`. An ambiguous quantity end is read as
    /// the other end's (`100m..=1` is a cpu's).
    fn range(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<ExprId> {
        let span = self.span(n);
        let ends: Vec<SyntaxNode> = terms(n).collect();
        let [lo, hi] = ends.as_slice() else {
            return Err(Skip);
        };
        let lo = self.expr(rc, lo, Pos::Content, out)?;
        let hi = self.expr(rc, hi, Pos::Content, out)?;
        let (tl, th) = (self.term_of(lo), self.term_of(hi));
        let (rl, rh) = crate::types::range_ends(tl.clone(), th.clone());
        let inclusive = tokens(n).any(|t| t.kind() == DOT2_EQ);
        if let (Term::Val(a), Term::Val(b)) = (&rl, &rh) {
            match crate::range::Range::new(a.clone(), b.clone(), inclusive) {
                Ok(r) => return Ok(self.node(span, ExprKind::Lit(r.into()))),
                // Known ends that make no range: why.
                Err(why) => return self.error(span, format!("`{}` is no range: {why}", n.text())),
            }
        }
        let (lo, hi) = (self.read_as(lo, &tl, rl), self.read_as(hi, &th, rh));
        Ok(self.node(span, ExprKind::Range { lo, hi, inclusive }))
    }

    /// A string's node: its text, or its pieces of text and holes.
    fn string_expr(&mut self, rc: &mut Rc, t: &SyntaxToken, pre: &mut Vec<Lit>) -> L<ExprId> {
        use crate::program::node::Piece as Part;
        let text = t.text();
        let span = self.span_of(t.text_range());
        if self.text || !text.contains("${") {
            let s = self.string(t)?;
            return Ok(self.node(span, ExprKind::Lit(Value::Str(s))));
        }
        let base: u32 = t.text_range().start().into();
        let mut parts = Vec::new();
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
                Piece::Text(lit) => {
                    let s = unescape(&format!("\"{lit}\"")).map_err(|e| {
                        self.diags.push(Diagnostic::error(span, e));
                        Skip
                    })?;
                    if s.contains("%s") {
                        return self.error(span, "an interpolated string cannot hold `%s`");
                    }
                    if !s.is_empty() {
                        parts.push(Part::Text(s));
                    }
                }
                Piece::Hole(hole, at) => {
                    let at = base + at as u32;
                    parts.push(Part::Hole(self.bind(false, |l| l.hole(rc, hole, at, pre))?));
                }
            }
        }
        if !parts.iter().any(|p| matches!(p, Part::Hole(_))) {
            let s: String = parts
                .into_iter()
                .map(|p| match p {
                    Part::Text(s) => s,
                    Part::Hole(_) => unreachable!("no hole"),
                })
                .collect();
            return Ok(self.node(span, ExprKind::Lit(Value::Str(s))));
        }
        Ok(self.node(span, ExprKind::Interp { parts }))
    }

    /// An interpolation hole: a term, read now (a content position).
    fn hole(&mut self, rc: &mut Rc, src: &str, at: u32, pre: &mut Vec<Lit>) -> L<ExprId> {
        // A variable `in T` types is a reference: it interpolates as its
        // address, `T["A"]`, as an untyped one does (R-42).
        let parse = parse::parse_term(src.trim());
        let typed = terms(&parse.syntax())
            .next()
            .and_then(|t| Chain::of(&t))
            .is_some_and(|c| c.is_bare() && rc.types.contains_key(&c.head));
        let pos = if typed { Pos::Value } else { Pos::Content };
        self.text_expr(rc, src, at, pos, pre)
    }

    /// A block entry's value: its term, or, for an entry that is only a
    /// simple name, the pun: the name as a term, resolved where the value
    /// would be (R-33), `zone` for `zone`. A pun is one name on both sides
    /// (R-216): `spec.selector.color` alone is an error.
    fn entry_value(
        &mut self,
        rc: &mut Rc,
        a: &SyntaxNode,
        pos: Pos,
        reads: &mut Vec<Lit>,
    ) -> L<(Term, ExprId)> {
        if let Some(t) = terms(a).next() {
            return self.arg_node(rc, &t, pos, reads);
        }
        let path = node(a, BLOCK_PATH).ok_or(Skip)?;
        let seg = tokens(&path).last().ok_or(Skip)?;
        let named = seg.kind().is_word()
            && !matches!(
                seg.kind(),
                NOT_KW | IN_KW | HAS_KW | WHERE_KW | IF_KW | TRUE_KW | FALSE_KW
            );
        let written = path.text().to_string();
        let one = tokens(&path).nth(1).is_none();
        if named && !one {
            let parent = written[..written.len() - seg.text().len()].trim_end_matches('.');
            let d = Diagnostic::error(
                self.span(a),
                format!("`{written}` alone is not a pun: a pun is one simple name on both sides"),
            )
            .with_help(format!(
                "write `{written} = {seg}`, or `{parent} = {{ {seg} }}`"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        if !named || !one {
            let d = Diagnostic::error(
                self.span(a),
                format!("`{written}` names no value: an entry is `path = term`"),
            )
            .with_help(format!("give it a value: `{written} = ..`"));
            self.diags.push(d);
            return Err(Skip);
        }
        let at: u32 = seg.text_range().start().into();
        let id = self.text_expr(rc, seg.text(), at, pos, reads)?;
        Ok((self.lowered(id, reads), id))
    }

    /// The term `src`, written at byte `at`, as its node in `pos`.
    fn text_expr(
        &mut self,
        rc: &mut Rc,
        src: &str,
        at: u32,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<ExprId> {
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
            self.expr(rc, &t, pos, pre)
        })();
        self.offset = saved;
        r
    }

    // --- resolution -------------------------------------------------------

    /// In a field's `check`, a bare name of a field of an enclosing object
    /// (not the field checked, nor a variable): the chain read by the
    /// object's path, `lo` as `db.lo`.
    fn field_in_check(&self, rc: &Rc, c: &Chain) -> Option<Chain> {
        let h = c.head.as_str();
        if self.checked.as_deref().is_none_or(|n| n == h) || rc.vars.contains_key(h) {
            return None;
        }
        let (path, _) = self
            .objects
            .iter()
            .rev()
            .find(|(_, f)| f.iter().any(|f| f == h))?;
        let mut words = path.split('.');
        let head = words.next()?.to_string();
        let mut ops: Vec<Op> = words.map(|w| Op::Field(w.to_string())).collect();
        ops.push(Op::Field(h.to_string()));
        ops.extend(c.ops.iter().cloned());
        Some(Chain {
            head,
            head_kind: IDENT,
            call: None,
            range: c.range,
            ops,
        })
    }

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
        let field = self.field_in_check(rc, c);
        let c = field.as_ref().unwrap_or(c);
        let h = c.head.as_str();
        if self.lenient && !rc.vars.contains_key(h) && self.checked.as_ref().is_none_or(|n| n == h)
        {
            if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
                return self.error(span, "a name here is a path: `a.b.c`");
            }
            let name = c.fields().join(".");
            return Ok(Res::Val(self.str_node(span, &name)));
        }
        if h == "_" && c.is_bare() {
            return Ok(Res::Val(self.node(span, ExprKind::Hole)));
        }
        if h == "_" {
            return self.error(
                span,
                "`_` is a placeholder and is never accessed: `_.p` and `_[k]` read nothing; \
                 name it (`env.p`)",
            );
        }
        // `super.x`: the scope around this component (R-186).
        if h == "super" && !rc.vars.contains_key(h) {
            return self.super_read(rc, c, pre, span);
        }
        // A let with parameters has no value until called (R-187).
        if !rc.vars.contains_key(h)
            && let Some((_, def)) = self.function_def(rc.scope, h)
        {
            return self.not_called(span, h, &def);
        }
        // A copy whose `instance` is the error reads nothing, and says so
        // there.
        if !rc.vars.contains_key(h) && self.unbound_instance(rc.scope, h) {
            return Err(Skip);
        }
        // A copy `x in network` binds (R-67): its name, `x.k` its output.
        if let Some(path) = rc.instances.get(h).cloned() {
            let v = self.var_named(rc, h, span);
            let inst = self.var_node(rc, span, &v);
            return self.output_of(rc, (inst, None), &path, &c.ops, pre, span);
        }
        // A typed variable: a reference.
        if let Some(t) = rc.types.get(h).cloned() {
            if let Some(Op::Field(first)) = c.ops.first() {
                self.namespace_attr(rc, h, first, span)?;
            }
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            let name = self.var_node(rc, span, &v);
            let addr = match rc.row_refs.contains(h) {
                true => {
                    let via = Reach::Absolute;
                    self.node(span, ExprKind::Scoped { name, via })
                }
                false => name,
            };
            let typ = self.name_node(rc, span, &t);
            return Ok(Res::Ref { typ, addr, path });
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
            self.undeclared(rc, h, span)?;
            if self.is_value(rc.scope, h) && own != Some(true) {
                return self.value(rc, rc.scope, c, pre, span);
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
                    typ: self.str_node(span, &types[0]),
                    addr: self.resource_addr(rc.scope, h, span),
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
            let var = self.var_node(rc, span, &v);
            return Ok(Res::Var { var, path });
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
        let fix = c.ops.iter().all(|o| matches!(o, Op::Field(..)));
        let near = self.scope_names(rc);
        self.diags.push(unknown_name(
            span,
            h,
            &quoted,
            fix,
            near.iter().map(String::as_str),
        ));
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
        let span = self.span(call);
        let id = self.bind(false, |l| l.part(rc, call, pre))?;
        let t = self.term_of(id);
        let key = format!("call {}", self.read_key_of(id));
        let var = match rc.reads.get(&key) {
            Some(Term::Var(v)) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            _ => {
                let name = self.callee(call).unwrap_or_default();
                let base = capitalise(name.rsplit('.').next().unwrap_or_default());
                let v = fresh(rc, &base);
                pre.push(Lit::Eq(var(&v), t));
                rc.reads.insert(key, var(&v));
                self.building(rc, span, |b| b.bound_into(id, &v))
            }
        };
        let path = self.segs(rc, ops, pre)?;
        Ok(Res::Var { var, path })
    }

    /// A value name: `k(V)`, read once per rule. A `let` holding a
    /// reference (H-6) reads through it: `cfg.x` is `cfg(E), setting(E,
    /// "x", V)`. Looked up from `from` outward: `rc.scope`, or the scope
    /// `super` names.
    fn value(
        &mut self,
        rc: &mut Rc,
        from: ScopeId,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let name = c.head.clone();
        let (decl, via) = self.value_decl(rc.scope, from, &name);
        let key = read_key(&name, &via);
        if !c.is_bare()
            && let Some(typ) = self.input_ref(from, &name)
        {
            return self.through_ref_input(rc, (c, &decl, via), typ, pre, span);
        }
        let ty = match self.value_type(from, &name) {
            Ok(t) => t,
            Err(e) => return self.error(span, e),
        };
        // A `let` holding a live object is its name alone; one holding a
        // resource is the reference (R-43).
        let Some(ty) = ty.filter(|t| !c.is_bare() || matches!(t, VType::Ref(_))) else {
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Value { decl, via, path });
        };
        let key = match rc.values.get(&key) {
            Some(v) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            None => {
                let v = fresh(rc, &capitalise(&name));
                let read = self.value_read(rc, &v, (decl, via), pre, span);
                rc.values.insert(key, v.clone());
                read
            }
        };
        match ty {
            VType::Ref(typ) => {
                let path = self.segs(rc, &c.ops, pre)?;
                Ok(Res::Ref {
                    typ: self.str_node(span, &typ),
                    addr: key,
                    path,
                })
            }
            VType::World(typ) => {
                let segs = self.segs(rc, &c.ops, pre)?;
                let Some(path) = self.path_string(&segs) else {
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

    /// `k.p` of an input `k` typed `ref(T)` (`c` the chain, `decl` and
    /// `via` the input as read): it holds the reference itself, so a dot
    /// reads through it, the address taken out of the reference (R-101),
    /// read once per rule. It is its user's, so no copy's scope goes in
    /// front of it.
    fn through_ref_input(
        &mut self,
        rc: &mut Rc,
        (c, decl, via): (&Chain, &DeclRef, Reach),
        typ: String,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let key = format!("ref {}", read_key(&decl.name, &via));
        let read = match rc.values.get(&key) {
            Some(v) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            None => {
                let name = fresh(rc, &capitalise(&decl.name));
                let rel = via.relation(&decl.name);
                let id = self.building(rc, span, |b| b.address_into((&rel, via), &typ, &name));
                self.hoisted_into(id, pre);
                rc.values.insert(key, name.clone());
                id
            }
        };
        let path = self.segs(rc, &c.ops, pre)?;
        let via = Reach::Absolute;
        Ok(Res::Ref {
            typ: self.str_node(span, &typ),
            addr: self.node(span, ExprKind::Scoped { name: read, via }),
            path,
        })
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
        Ok(Res::Val(self.var_node(rc, span, &v)))
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
                    let t = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
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
        let addr = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
        let segs = self.segs(rc, &rest[1..], pre)?;
        let Some(path) = self.path_string(&segs) else {
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
        // Inside a module's file the module reads itself by its own name
        // (`backups.repository` in its component): its value or resource
        // is the one of the instance the read is in, as a bare read's is
        // (R-186).
        if let Some(from) = self.self_module(rc.scope, h)
            && let Some(Op::Field(x)) = c.ops.first()
        {
            let rest = Chain {
                head: x.clone(),
                head_kind: IDENT,
                call: None,
                range: c.range,
                ops: c.ops[1..].to_vec(),
            };
            if let Some(r) = self.item_read(rc, from, false, &rest, pre, span)? {
                return Ok(Some(r));
            }
        }
        // Its other items under the name its user's `use` binds it to
        // (`use backups as b`).
        let alias = self.alias_in(rc.scope, h);
        let h = alias.as_deref().unwrap_or(h);
        if let Some(Op::Field(x)) = c.ops.first() {
            self.alternatives_agree(rc.scope, h, x, span)?;
        }
        if let Some((at, path)) = self.instance_in(rc.scope, h) {
            let name = self.str_node(span, h);
            let inst = self.scoped(rc.scope, at, name, span);
            return self
                .output_of(rc, (inst, None), &path, &c.ops, pre, span)
                .map(Some);
        }
        if let Some(d) = self.stack_in(rc.scope, h) {
            return self.deployed_path(rc, c, &d, pre, span).map(Some);
        }
        // A used stack by its full name, its module path (R-200):
        // `stacks.platform[env].out`, as `platform[env].out` reads it.
        if let Some((d, n)) = self.stack_by_path(rc.scope, h, &c.ops) {
            let short = Chain {
                head: d.name.clone(),
                head_kind: IDENT,
                call: None,
                range: c.range,
                ops: c.ops[n..].to_vec(),
            };
            return self.deployed_path(rc, &short, &d, pre, span).map(Some);
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
                if self.module_at(&at).is_some() {
                    break;
                }
                at = format!("{at}.{f}");
                ops = &ops[1..];
            }
            if !self.module_at(&at).is_some() {
                return Ok(None);
            }
            path = Some(at);
        }
        let Some(mut at) = path.take() else {
            return Ok(None);
        };
        while let Some(Op::Field(f)) = ops.first() {
            let next = format!("{at}.{f}");
            if !self.module_at(&next).is_some_and(|m| m.1) {
                break;
            }
            at = next;
            ops = &ops[1..];
        }
        let component = self.module_at(&at).is_some_and(|m| m.1);
        match ops.first() {
            Some(Op::Index(ts, _)) if component && ts.len() == 1 => {
                let mut t = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
                // `c[_]` binds each copy (R-162): a name its resources'
                // addresses are made of (`c[_].vm`, R-191).
                if matches!(self.program.exprs[t].kind, ExprKind::Hole) {
                    let copy = fresh(rc, "Copy");
                    t = self.var_node(rc, span, &copy);
                }
                // The copies of `at` this body makes, or its user's.
                let own = self
                    .own_scopes(rc.scope)
                    .iter()
                    .find(|s| {
                        self.names(**s)
                            .instances()
                            .map(|(_, c)| c)
                            .any(|p| *p == at)
                    })
                    .copied();
                let at_scope = own.unwrap_or(self.root());
                let user = self.str_node(span, "");
                let parent = self.scoped(rc.scope, at_scope, user, span);
                self.copy_test(rc, (&at, parent, t), pre, span);
                let inst = self.scoped(rc.scope, at_scope, t, span);
                // The test is the read's when it names the copy as the
                // read does: the body's own copies.
                let of = (inst == t).then(|| (at.clone(), parent));
                self.output_of(rc, (inst, of), &at, &ops[1..], pre, span)
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
                let Some((module, _)) = self.module_at(&at) else {
                    return Err(Skip);
                };
                let declared = self
                    .chain_of(rc.scope)
                    .into_iter()
                    .find(|s| self.names(*s).module(h).is_some())
                    .unwrap_or(self.root());
                let own = self.names(module);
                if let Some(at) = own.value_at(x) {
                    let path = self.segs(rc, &ops[1..], pre)?;
                    let decl = DeclRef {
                        scope: module,
                        name: x.clone(),
                        at,
                    };
                    let via = Reach::Use(h.to_string());
                    return Ok(Some(Res::Value { decl, via, path }));
                }
                if own.output(x).is_some() {
                    let name = self.str_node(span, h);
                    let inst = self.scoped(rc.scope, declared, name, span);
                    return self
                        .output_of(rc, (inst, None), &at, ops, pre, span)
                        .map(Some);
                }
                if let Some(types) = own.resources(x) {
                    if types.len() > 1 {
                        return self.ambiguous(x, &types, span).map(Some);
                    }
                    let name = self.str_node(span, h);
                    let scope = self.scoped(rc.scope, declared, name, span);
                    let path = self.segs(rc, &ops[1..], pre)?;
                    let x = self.str_node(span, &crate::address::name_segment(x));
                    return Ok(Some(Res::Ref {
                        typ: self.str_node(span, &types[0]),
                        addr: self
                            .building(rc, span, |b| b.form(crate::address::SCOPED, vec![scope, x])),
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

    /// Each declaration of `h` in `s`, when it is declared more than once
    /// (R-104): the module or component path each binds it to, and its
    /// statement. A copy whose component names nothing is its own error.
    fn alternatives(&self, s: ScopeId, h: &str) -> Option<Vec<(String, SyntaxNode)>> {
        let bound: Vec<_> = self.names(s).bound(h).collect();
        if bound.len() < 2 {
            return None;
        }
        let alts = bound.into_iter().filter_map(|d| match &d.kind {
            DeclKind::Use { written, .. } => {
                Some((self.module_path_of(s, written), d.node.clone()))
            }
            DeclKind::Copy { written, .. } => {
                let path = self.component_path(s, written).ok()?;
                Some((path, d.node.clone()))
            }
            _ => None,
        });
        Some(alts.collect())
    }

    /// `h.x` where `h` is declared more than once, each under a clause
    /// (R-104): every declaration has `x`, of one type, else the read is
    /// an error naming every declaration.
    fn alternatives_agree(&mut self, scope: ScopeId, h: &str, x: &str, span: Span) -> L<()> {
        let Some(alts) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.names(*s).bound(h).next().is_some())
            .and_then(|s| self.alternatives(s, h))
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
            let item = self.module_at(path).and_then(|(m, _)| {
                let sc = self.names(m);
                if sc.output(x).is_some() {
                    let ty = sc.output_type(x).map_or("any", String::as_str);
                    return Some(format!("output {x}: {ty}"));
                }
                if let Some(i) = sc.input(x) {
                    let ty = node(i, TYPE_EXPR)
                        .map(|t| t.text().to_string().replace(char::is_whitespace, ""))
                        .unwrap_or_default();
                    return Some(format!("input {x}: {ty}"));
                }
                if sc.is_value(x) {
                    return Some(format!("let {x}"));
                }
                sc.resources(x)
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

    /// The test that `copy` is a copy of `component` made in `scope`,
    /// `instance_of(component, scope, copy)`, into `pre` before what reads
    /// the copy by its key; its goal kept for the builders.
    fn copy_test(
        &mut self,
        rc: &mut Rc,
        (component, scope, copy): (&str, ExprId, ExprId),
        pre: &mut Vec<Lit>,
        span: Span,
    ) {
        let pred = crate::modules::INSTANCE_OF;
        let args = vec![str_term(component), self.term_of(scope), self.term_of(copy)];
        let test = Lit::Pos(atom_at(pred, args, span));
        let of = self.str_node(span, component);
        let g = self.building(rc, span, |b| {
            let args = [of, b.settled(scope), b.settled(copy)]
                .into_iter()
                .map(|a| b.pattern_of(a))
                .collect();
            let rel = RelRef::own(pred, span);
            let args = RelArgs::Positional(args);
            b.goal_node(span, GoalKind::Rel { rel, args })
        });
        rc.tests.push((Some(scope), g));
        pre.push(test);
    }

    /// `.k.path` after an instance's scope `inst`, of the component at
    /// `path`: its output `k`, a reference when the output is typed by a
    /// resource type; `of`, the test that the copy exists, made before the
    /// read when it is named by its key.
    fn output_of(
        &mut self,
        rc: &mut Rc,
        (inst, of): (ExprId, Option<(String, ExprId)>),
        path: &str,
        ops: &[Op],
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        match ops.first() {
            None => Ok(Res::Val(inst)),
            Some(Op::Field(k)) => {
                let segs = self.segs(rc, &ops[1..], pre)?;
                // A resource of the copy (R-191), `c[t].vm`: its address
                // under the copy's, `__scoped(t, "vm")`, as a used
                // module's `k3s.admin` is.
                if let Some(types) = self.copy_resource(path, k) {
                    if types.len() > 1 {
                        return self.ambiguous(k, &types, span);
                    }
                    let x = self.str_node(span, &crate::address::name_segment(k));
                    return Ok(Res::Ref {
                        typ: self.str_node(span, &types[0]),
                        addr: self
                            .building(rc, span, |b| b.form(crate::address::SCOPED, vec![inst, x])),
                        path: segs,
                    });
                }
                let typed = self
                    .module_at(path)
                    .and_then(|(m, _)| self.names(m).output(k).flatten().cloned());
                if let Some(t) = &typed
                    && !segs.is_empty()
                {
                    // A typed output: the address it holds, then a reference.
                    let read = PlanRead::Output {
                        copy: inst,
                        key: k.clone(),
                        of,
                    };
                    let v = self.read_var(rc, "output", read, 2, k, pre, span);
                    return Ok(Res::Ref {
                        typ: self.str_node(span, t),
                        addr: v,
                        path: segs,
                    });
                }
                Ok(Res::Output {
                    inst,
                    key: k.clone(),
                    path: segs,
                    typ: typed,
                    of,
                })
            }
            Some(_) => self.error(span, "after a component's resource: `.output`"),
        }
    }

    /// The types of the resource `k` the component at `path` makes, when
    /// `k` names one of them and no output of it.
    fn copy_resource(&self, path: &str, k: &str) -> Option<Vec<String>> {
        let (m, _) = self.module_at(path).filter(|m| m.1)?;
        let sc = self.names(m);
        if sc.output(k).is_some() {
            return None;
        }
        sc.resources(k)
    }

    /// `k` read of a deployment of `d` is one of its outputs (R-208): the
    /// stack's file is at hand, so a name it does not output is an error
    /// at the read, naming what it does; a key is the deployment's name.
    fn deployed_output(&mut self, d: &Deployed, k: &str, span: Span) -> L<()> {
        let Some(outputs) = &d.outputs else {
            return Ok(());
        };
        if outputs.iter().any(|o| o == k) {
            return Ok(());
        }
        let (m, short) = (&d.path, &d.name);
        let d = match d.keys.iter().any(|x| x == k) {
            true => Diagnostic::error(
                span,
                format!(
                    "`{k}` is a key of {m}, not an output: it names the deployment, \
                     `{short}[{k}=..]`, and what is read of it is an output"
                ),
            ),
            false => Diagnostic::error(span, format!("{m} has no output `{k}`")),
        };
        let d = match outputs.as_slice() {
            [] => d.with_note(format!("{m} declares no output")),
            _ => d.with_note(format!("its outputs: {}", outputs.join(", "))),
        };
        self.diags.push(d);
        Err(Skip)
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
                    let v = self.bind(true, |l| l.part(rc, t, pre))?;
                    let e = &self.program.exprs[v];
                    let escaped = match (&e.kind, e.hoisted) {
                        (ExprKind::Lit(Value::Str(s)), None) => Some(crate::stack::escape(s)),
                        (ExprKind::Lit(v), None) => Some(crate::stack::escape(&spell::bare(v))),
                        _ => None,
                    };
                    values.push(match escaped {
                        Some(s) => self.str_node(span, &s),
                        None => v,
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
                    .map(|v| match &self.program.exprs[*v].kind {
                        ExprKind::Lit(Value::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                let name = match constant {
                    Some(vs) => self.str_node(span, &text(vs)),
                    None => {
                        let holes = text(vec!["%s".to_string(); keys.len()]);
                        let holes = self.str_node(span, &holes);
                        let args = std::iter::once(holes).chain(values).collect();
                        self.building(rc, span, |b| b.form(crate::address::FORMAT, args))
                    }
                };
                (name, &c.ops[1..])
            }
            Some(Op::Field(_)) if keys.is_empty() => (self.str_node(span, &d.name), &c.ops[..]),
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
        self.deployed_output(d, k, span)?;
        let path = self.segs(rc, &rest[1..], pre)?;
        // The keyed read of a copy's output (`network[t].k`), the
        // deployment the instance: `instance_of(PATH, "", NAME), output(NAME,
        // k, V)`, both served from what it published (I-modules section 6).
        let root = self.root();
        let inst = self.scoped(rc.scope, root, name, span);
        let user = self.str_node(span, "");
        let user = self.scoped(rc.scope, root, user, span);
        self.copy_test(rc, (&d.path, user, inst), pre, span);
        Ok(Res::Output {
            inst,
            key: k.clone(),
            path,
            typ: None,
            of: Some((d.path.clone(), user)),
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
                    crate::address::Address {
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
        if let Some(cols) = self.program.scopes.externs.get(&name).cloned() {
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
                args.push(self.bind(true, |l| l.part(rc, t, pre))?);
            }
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Lookup {
                pred: name,
                args,
                out: outs[0],
                path,
            }));
        }
        if k == 1
            && self.program.scopes.declares_relation(&name)
            && !self.decls.types.contains(&name)
        {
            let mut args = Vec::new();
            for t in ts {
                args.push(self.bind(true, |l| l.part(rc, t, pre))?);
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
        // A dotted name in no namespace a type or a provider `use` names
        // is no type: `web.spec.containers["web"]` reads the name `web`,
        // as `web.spec` does.
        let namespace = [&self.decls.namespaces, &self.decls.providers]
            .iter()
            .any(|ns| ns.contains(&c.head));
        if k > 1 && !namespace && !self.any_type {
            return Ok(None);
        }
        // `T[e]` in a namespace no declaration or builtin schema closes: a
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
            let addr = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
            self.address_key(&self.term_of(addr), span)?;
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Ref {
                typ: self.str_node(span, &name),
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
            && let Some(fixed) = crate::address::old_scope(a)
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

    /// Hoist the read `read`, `pred(args.., V)` (V at `out`), once per
    /// rule: its node, the read into a new variable the first time, its
    /// literal into `pre`; the variable after.
    #[allow(clippy::too_many_arguments)]
    fn read_var(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        read: PlanRead,
        out: usize,
        hint: &str,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> ExprId {
        let args = self.read_args(&read);
        let (name, new) = self.hoist_read(rc, pred, &args, out, hint);
        if !new {
            return self.var_node(rc, span, &name);
        }
        let id = self.building(rc, span, |b| b.read_into(read.of(), &name));
        self.hoisted_into(id, pre);
        id
    }

    /// The columns of the read `read` but its value's, each as a rule's
    /// reads are kept by it ([`Self::read_key_of`]): what names it once per
    /// rule.
    fn read_args(&self, read: &PlanRead) -> Vec<String> {
        let text = |s: &str| format!("{:?}", str_term(s));
        match read {
            PlanRead::Value(..) => Vec::new(),
            PlanRead::Attr { typ, addr, attr } => {
                vec![self.read_key_of(*typ), self.read_key_of(*addr), text(attr)]
            }
            PlanRead::Output { copy, key, .. } => vec![self.read_key_of(*copy), text(key)],
            PlanRead::World { typ, addr, path } => {
                vec![text(typ), self.read_key_of(*addr), text(path)]
            }
            PlanRead::Lookup { args, .. } => args.iter().map(|a| self.read_key_of(*a)).collect(),
        }
    }

    /// What a rule's reads are kept by for the term `id`: the term it
    /// lowers to and how each name in it is reached, so two names written
    /// alike and reached two ways (`repo`, `super.repo`) are two.
    fn read_key_of(&self, id: ExprId) -> String {
        let mut reaches = Vec::new();
        let mut walk = vec![id];
        while let Some(e) = walk.pop() {
            let x = &self.program.exprs[e];
            if let ExprKind::Scoped { via, .. } = &x.kind {
                reaches.push(format!("{via:?}"));
            }
            walk.extend(x.kind.children());
        }
        format!("{:?} {}", self.term_of(id), reaches.join(" "))
    }

    /// The variable the read `pred(args.., V)` (V at `out`) binds, once
    /// per rule, and whether it is read here first: what it reads kept
    /// for the program's builder.
    fn hoist_read(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        args: &[String],
        out: usize,
        hint: &str,
    ) -> (String, bool) {
        let key = format!("{pred}{args:?}@{out}");
        if let Some(Term::Var(v)) = rc.reads.get(&key) {
            return (v.clone(), false);
        }
        let name = fresh(rc, &capitalise(hint));
        rc.reads.insert(key, var(&name));
        (name, true)
    }

    /// A resolved chain as its node at `pos` (R-211 step 7): what it reads
    /// in term position its read's node, a path into it the steps, each
    /// read's variable named once per rule, over the nodes of what
    /// resolved it; of the reads resolving it, `local` (the literals the
    /// resolver made of them), those no node holds into `pre`.
    fn realize_expr(
        &mut self,
        rc: &mut Rc,
        res: Res,
        pos: Pos,
        (local, pre): (Vec<Lit>, &mut Vec<Lit>),
        span: Span,
    ) -> L<ExprId> {
        let plan = self.plan(rc, res, pos, span)?;
        let (id, left) = self.building(rc, span, |b| {
            let id = plan.build(b);
            (id, b.not_held(id, local))
        });
        pre.extend(left);
        Ok(id)
    }

    /// The goals binding the reads [`Self::settled`] takes out of `res`'s
    /// parts, each `x = read`: what holds the settled `res` holds them
    /// before it.
    fn settled_reads(&mut self, rc: &Rc, res: &Res, span: Span) -> Vec<GoalId> {
        let segs = |path: &[Seg]| -> Vec<ExprId> {
            path.iter()
                .filter_map(|s| match s {
                    Seg::I(x) | Seg::K(x) => Some(*x),
                    Seg::F(_) => None,
                })
                .collect()
        };
        let parts = match res {
            Res::Ref { addr, path, .. } => [vec![*addr], segs(path)].concat(),
            Res::Var { var, path } => [vec![*var], segs(path)].concat(),
            Res::Value { path, .. } => segs(path),
            _ => Vec::new(),
        };
        self.building(rc, span, |b| {
            parts.into_iter().flat_map(|p| b.settled_reads(p)).collect()
        })
    }

    /// `goal` after the goals `before`: a group of them, at `span`.
    fn after(&mut self, rc: &Rc, span: Span, before: Vec<GoalId>, goal: GoalId) -> GoalId {
        if before.is_empty() {
            return goal;
        }
        let mut goals = before;
        goals.push(goal);
        let after = Vec::new();
        self.goal(rc, span, GoalKind::Group { goals, after })
    }

    /// `res` with the reads its parts make left where they were made: each
    /// part that holds a read is the term it lowered to, its reads'
    /// variables in their place.
    fn settled(&mut self, rc: &Rc, res: Res, span: Span) -> Res {
        let settle = |l: &mut Self, id: ExprId| l.building(rc, span, |b| b.settled(id));
        let segs = |l: &mut Self, path: Vec<Seg>| -> Vec<Seg> {
            path.into_iter()
                .map(|s| match s {
                    Seg::I(i) => Seg::I(settle(l, i)),
                    Seg::K(k) => Seg::K(settle(l, k)),
                    f => f,
                })
                .collect()
        };
        match res {
            Res::Ref { typ, addr, path } => Res::Ref {
                typ,
                addr: settle(self, addr),
                path: segs(self, path),
            },
            Res::Var { var, path } => Res::Var {
                var: settle(self, var),
                path: segs(self, path),
            },
            Res::Value { decl, via, path } => Res::Value {
                decl,
                via,
                path: segs(self, path),
            },
            res => res,
        }
    }

    /// What a resolved chain at `pos` reads and the path into it, each
    /// variable named in the order the reads are made.
    fn plan(&mut self, rc: &mut Rc, res: Res, pos: Pos, span: Span) -> L<Plan> {
        Ok(match res {
            Res::Val(t) => Plan::Node(t),
            Res::Type(t) => Plan::Node(self.str_node(span, &t)),
            Res::Var { var: v, path } => self.plan_path(rc, Plan::Node(v), path, None),
            Res::Ref { typ, addr, path } if path.is_empty() => Plan::Node(match pos {
                Pos::Value => {
                    let empty = self.str_node(span, "");
                    let args = vec![typ, addr, empty];
                    self.building(rc, span, |b| b.form(crate::address::REF, args))
                }
                _ => addr,
            }),
            Res::Ref { typ, addr, path } if pos != Pos::Content => {
                let Some(p) = self.path_string(&path) else {
                    return self.error(span, "a reference's path is constant");
                };
                let p = self.str_node(span, &p);
                let args = vec![typ, addr, p];
                Plan::Node(self.building(rc, span, |b| b.form(crate::address::REF, args)))
            }
            Res::Ref { typ, addr, path } => {
                let Seg::F(first) = &path[0] else {
                    return self.error(span, "a resource's attribute is `r.name`");
                };
                let first = crate::address::path_key(first).into_owned();
                let read = PlanRead::Attr {
                    typ,
                    addr,
                    attr: first.clone(),
                };
                let base = self.plan_read(rc, "attr", (3, &first), read, span);
                let at = (self.leaf_copy(typ), first);
                self.plan_path(rc, base, path[1..].to_vec(), Some(at))
            }
            Res::Output {
                inst,
                key,
                path,
                typ,
                of,
            } => {
                let read = PlanRead::Output {
                    copy: inst,
                    key: key.clone(),
                    of,
                };
                let base = self.plan_read(rc, "output", (2, &key), read, span);
                match typ {
                    // A typed output holds an address: given as a value, it
                    // is the reference (R-43).
                    Some(typ) if pos == Pos::Value && path.is_empty() => Plan::RefTo {
                        typ,
                        addr: Box::new(base),
                    },
                    _ => self.plan_path(rc, base, path, None),
                }
            }
            Res::World { typ, addr, path } => {
                let last = path.rsplit('.').next().unwrap_or(&path).to_string();
                let read = PlanRead::World { typ, addr, path };
                self.plan_read(rc, "cloud_attr", (3, &last), read, span)
            }
            Res::Value { decl, via, path } => {
                let base = self.value_plan(rc, decl, via, span);
                self.plan_path(rc, base, path, None)
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } => {
                let hint = pred.replace('.', "_");
                let read = PlanRead::Lookup {
                    rel: pred.clone(),
                    args,
                    out,
                };
                let base = self.plan_read(rc, &pred, (out, &hint), read, span);
                self.plan_path(rc, base, path, None)
            }
        })
    }

    /// The value `decl` read `via`: its variable when the rule read it
    /// already, else the read into a new one, named after the value (a
    /// used module's after its relation, `m::k`).
    fn value_plan(&mut self, rc: &mut Rc, decl: DeclRef, via: Reach, span: Span) -> Plan {
        let key = read_key(&decl.name, &via);
        if let Some(v) = rc.values.get(&key) {
            let v = v.clone();
            return Plan::Node(self.var_node(rc, span, &v));
        }
        let pred = via.relation(&decl.name);
        let used = matches!(via, Reach::Use(_));
        let name = fresh(rc, &capitalise(if used { &pred } else { &decl.name }));
        rc.values.insert(key, name.clone());
        Plan::Read {
            read: PlanRead::Value(decl, via),
            var: name,
        }
    }

    /// The read `read`, `pred(args.., V)` (V at `out`): its variable when
    /// the rule read it already, else the read into a new one.
    fn plan_read(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        (out, hint): (usize, &str),
        read: PlanRead,
        span: Span,
    ) -> Plan {
        let args = self.read_args(&read);
        match self.hoist_read(rc, pred, &args, out, hint) {
            (v, false) => Plan::Node(self.var_node(rc, span, &v)),
            (v, true) => Plan::Read { read, var: v },
        }
    }

    /// Fields and indexes into `base`. From the attribute `at` (the
    /// resource's type and the attribute's top segment) of a resource, an
    /// element by its key, `containers["api"]`, is the element of the
    /// keyed list whose one key field (`type_list_key`) has that value
    /// (R-35); elsewhere a key is a position.
    fn plan_path(
        &mut self,
        rc: &mut Rc,
        base: Plan,
        path: Vec<Seg>,
        mut at: Option<(ExprId, String)>,
    ) -> Plan {
        let mut steps = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let flush = |steps: &mut Vec<PlanStep>, fields: &mut Vec<String>| {
            if fields.is_empty() {
                return;
            }
            let p = fields
                .iter()
                .map(|f| crate::address::path_key(f))
                .collect::<Vec<_>>()
                .join(".");
            fields.clear();
            steps.push(PlanStep::Fields(p));
        };
        for s in path {
            match s {
                Seg::F(f) => {
                    if let Some((_, list)) = &mut at {
                        list.push('.');
                        list.push_str(&crate::address::path_key(&f));
                    }
                    fields.push(f)
                }
                Seg::K(key) if at.is_some() => {
                    let (typ, list) = at.take().expect("matched");
                    flush(&mut steps, &mut fields);
                    let item = fresh(rc, "Item");
                    let keys = fresh(rc, "Keys");
                    let field = fresh(rc, "Key");
                    steps.push(PlanStep::Keyed {
                        key,
                        typ,
                        list,
                        item,
                        keys,
                        field,
                    });
                }
                Seg::I(i) | Seg::K(i) => {
                    at = None;
                    flush(&mut steps, &mut fields);
                    let item = fresh(rc, "Item");
                    steps.push(PlanStep::Element { i, item });
                }
            }
        }
        flush(&mut steps, &mut fields);
        match steps.is_empty() {
            true => base,
            false => Plan::Path {
                base: Box::new(base),
                steps,
            },
        }
    }

    /// The read of a value hoisted, `k(V)`: its node, the read into
    /// `var_name`, its literal into `pre`.
    fn value_read(
        &mut self,
        rc: &Rc,
        var_name: &str,
        (decl, via): (DeclRef, Reach),
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> ExprId {
        let read = ReadOf::Value(decl, via);
        let id = self.building(rc, span, |b| b.read_into(read, var_name));
        self.hoisted_into(id, pre);
        id
    }

    /// The read a resolved chain is, with `value` in its value column: the
    /// direct forms `R.p == c`, `k == c`, `not R.p`. `None` when the chain
    /// is not one read.
    fn read_atom(&self, res: &Res, value: Term, span: Span) -> Option<Atom> {
        let t = |id: &ExprId| self.term_of(*id);
        match res {
            Res::Ref { typ, addr, path } if path.len() == 1 => match &path[0] {
                Seg::F(p) => Some(atom_at(
                    "attr",
                    vec![
                        t(typ),
                        t(addr),
                        str_term(&crate::address::path_key(p)),
                        value,
                    ],
                    span,
                )),
                Seg::I(_) | Seg::K(_) => None,
            },
            Res::Output {
                inst, key, path, ..
            } if path.is_empty() => {
                Some(atom_at("output", vec![t(inst), str_term(key), value], span))
            }
            Res::World { typ, addr, path } => Some(atom_at(
                "cloud_attr",
                vec![str_term(typ), t(addr), str_term(path), value],
                span,
            )),
            Res::Value { decl, via, path } if path.is_empty() => {
                Some(atom_at(&via.relation(&decl.name), vec![value], span))
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } if path.is_empty() => {
                let mut args: Vec<Term> = args.iter().map(t).collect();
                args.insert(*out, value);
                Some(atom_at(pred, args, span))
            }
            _ => None,
        }
    }

    /// `has r.PATH` of a resource's attribute (a path of fields):
    /// `__has(T, A, "PATH")`, which the compiler answers from the schema in
    /// a rule that writes under PATH ([`mark_has`]).
    fn has_atom(&self, res: &Res, span: Span) -> Option<Atom> {
        let m = self.has_mark(res)?;
        Some(atom_at(
            crate::partition::HAS,
            vec![self.term_of(m.typ), self.term_of(m.addr), str_term(&m.path)],
            span,
        ))
    }

    /// The mark of `has r.PATH` ([`Self::has_atom`]), its parts the nodes
    /// of what resolved `r`.
    fn has_mark(&self, res: &Res) -> Option<HasMark> {
        let Res::Ref { typ, addr, path } = res else {
            return None;
        };
        let keys: Vec<String> = path
            .iter()
            .map(|s| match s {
                Seg::F(p) => Some(crate::address::path_key(p).into_owned()),
                Seg::I(_) | Seg::K(_) => None,
            })
            .collect::<Option<_>>()?;
        (!keys.is_empty()).then(|| HasMark {
            typ: *typ,
            addr: *addr,
            path: keys.join("."),
        })
    }

    /// A constant path as a stored path: `a.b[0].c`, `a."b.c"`.
    fn path_string(&self, path: &[Seg]) -> Option<String> {
        let mut out = String::new();
        for s in path {
            match s {
                Seg::F(f) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(&crate::address::path_key(f));
                }
                Seg::I(i) => match (&self.program.exprs[*i].kind, self.program.exprs[*i].hoisted) {
                    (ExprKind::Lit(Value::Int(i)), None) => out.push_str(&format!("[{i}]")),
                    _ => return None,
                },
                Seg::K(_) => return None,
            }
        }
        Some(out)
    }
}

/// What a `set` sets.
enum Target {
    /// `(T, A, path)`, and the block that owns the cell when it is
    /// declared in the same scope.
    Cell(Termed, Termed, String, Option<String>),
    /// One element of the keyed list at `(T, A, path)` (R-35, R-69): the
    /// key, the fields below the element, and the owning block.
    Element(Termed, Termed, String, Termed, Vec<String>, Option<String>),
    /// A stack input.
    Input(String),
}

/// One line of a `set` as the resolver lowered it (R-211 step 5): its
/// head, and its body: the clause's literals, then from `seed` the reads
/// its target hoisted and from `mid` its value's, which made the
/// statement's helpers `made.0..made.1` and `made.1..made.2`.
struct Contribution {
    span: Span,
    target: Target,
    add: bool,
    rank: Option<Rank>,
    value: ExprId,
    head: Atom,
    body: Vec<Lit>,
    made: (usize, usize, usize),
    /// The variables the program wrote, by the name each lowers to.
    written: BTreeSet<String>,
}

impl Contribution {
    /// Its parts for the builder, `helpers` the statement's.
    fn lowered<'a>(&'a self, helpers: &'a [Stmt]) -> crate::program::build::WriteLowered<'a> {
        use crate::program::build::TargetLowered;
        let target = match &self.target {
            Target::Cell(typ, addr, path, _) => TargetLowered::Attr {
                typ: typ.1,
                addr: addr.1,
                path,
            },
            Target::Element(typ, addr, list, key, rest, _) => TargetLowered::Element {
                typ: typ.1,
                addr: addr.1,
                list,
                key: key.1,
                rest,
            },
            Target::Input(k) => TargetLowered::Input(k),
        };
        let (a, b, c) = self.made;
        crate::program::build::WriteLowered {
            span: self.span,
            target,
            add: self.add,
            rank: self.rank,
            value: self.value,
            head: &self.head,
            body: &self.body,
            made: (&helpers[a..b], &helpers[b..c]),
        }
    }
}

/// A relation's rows read from a document (R-39), as the resolver
/// lowered them: the externs reading it, then the rule `head :- body`,
/// its clause's literals then from `seed` the document's reads, which
/// made the statement's helpers from `made`.
struct TableRows {
    externs: Vec<Stmt>,
    head: Atom,
    body: Vec<Lit>,
    /// The goal reading the document's rows.
    rows: GoalId,
    made: usize,
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

/// The variable `has x.f` of a value's field binds its walk to,
/// `Has' = __path(X, "f")`, read nowhere else: the mark the secret pass
/// knows the test by (`secrets`, E0301). The apostrophe is in no name a
/// program's variable lowers to.
pub const HAS_VAR: &str = "Has'";

/// Whether `v` is a `has` walk's variable ([`HAS_VAR`]).
pub fn is_has_var(v: &str) -> bool {
    v.strip_prefix(HAS_VAR)
        .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
}

/// `has r` of a resource (R-152): `__identity(T, A)`, which the compiler
/// makes a read of the resource's identity (`partition::IDENTITY`).
const IDENTITY: &str = crate::partition::IDENTITY;

/// A path that reads a resource's id: `r.id`.
fn is_identity(path: &[Seg]) -> bool {
    matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
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

/// A membership negated: a read negated, a negation read, the type
/// test of a reference column's `r in T` (R-42) the type that differs
/// (R-219).
fn negate(l: Lit) -> Lit {
    match l {
        Lit::Pos(a) => Lit::Not(a),
        Lit::Not(a) => Lit::Pos(a),
        Lit::Eq(a, b) => Lit::Neq(a, b),
        Lit::Neq(a, b) => Lit::Eq(a, b),
        other => other,
    }
}

fn atom_terms(a: &Atom) -> Vec<&Term> {
    a.args
        .iter()
        .chain(a.record.iter().flat_map(|r| r.values()))
        .collect()
}

/// The variables a body binds: arguments of its positive relation reads
/// (patterns included), and both sides of an equality.
pub(crate) fn bound_vars(body: &[Lit]) -> BTreeSet<String> {
    fn pattern(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(v) => {
                out.insert(v.clone());
            }
            // A relation's column takes a reference apart (R-42), in a
            // row of a copy's relation too (`__rows(.., [ref(T, A, "")])`).
            Term::Func { name, args } if name == crate::address::REF => {
                args.iter().for_each(|x| pattern(x, out))
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

/// The source names of the lowered variables in `outer`.
fn outer_names(rc: &Rc, outer: &BTreeSet<String>) -> BTreeSet<String> {
    rc.vars
        .iter()
        .filter(|(_, low)| outer.contains(*low))
        .map(|(src, _)| src.clone())
        .collect()
}

/// How the negation whose body is `n` is written: `not { B }` (the body),
/// `x not in e`, or `not L` (the literal).
fn negated(n: &SyntaxNode) -> Negated {
    match n.kind() {
        BODY => Negated::Block,
        LIT_NOT_IN => Negated::NotIn,
        _ => Negated::Literal,
    }
}

/// Every variable literal `l` names, a comprehension's included.
pub(crate) fn lit_vars(l: &Lit, out: &mut BTreeSet<String>) {
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

#[cfg(test)]
mod tests {
    use crate::ast::{Lit, Stmt, Term};
    use crate::spell;

    #[test]
    fn a_core_variable_reads_as_the_source_name() {
        use super::source_name;
        assert_eq!(source_name("AvailabilityZone"), "availability_zone");
        assert_eq!(source_name("Z"), "z");
        assert_eq!(source_name("__v1"), "__v1");
    }

    fn lits(ls: &[Lit]) -> String {
        ls.iter().map(spell::lit).collect::<Vec<_>>().join(", ")
    }

    /// Every statement as the core prints it, one per line.
    fn show(stmts: &[Stmt]) -> Vec<String> {
        stmts
            .iter()
            .filter_map(|s| {
                Some(match s {
                    Stmt::Rule(r) => spell::rule(r),
                    Stmt::Fact(a) => spell::atom(a),
                    Stmt::Resource(r) => {
                        let fields = r
                            .fields
                            .iter()
                            .map(|f| format!("{} = {}", f.key, spell::term(&f.value)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let body = r.body.as_deref().map(lits).unwrap_or_default();
                        format!(
                            "resource {} {} {{ {fields} }} :- {body}",
                            spell::term(&r.typ),
                            spell::term(&r.name)
                        )
                    }
                    Stmt::Instance(i) => {
                        let inputs = i
                            .inputs
                            .iter()
                            .map(|(k, v, _)| format!("{k} = {}", spell::term(v)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let body = i.body.as_deref().map(lits).unwrap_or_default();
                        format!("instance {} {} {{ {inputs} }} :- {body}", i.module, i.name)
                    }
                    Stmt::Output(o) => {
                        format!(
                            "output {} = {:?}",
                            o.name,
                            o.value.as_ref().map(spell::term)
                        )
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
            project: false,
        }];
        super::lower(&units, &[0], file, super::Mode::Program)
            .map_err(|d| crate::diag::Diagnostics(d).into())
    }

    fn parse(src: &str) -> anyhow::Result<crate::ast::Program> {
        parse_as(src, false)
    }

    /// The scope tree built alone is the tree the lowering builds: a
    /// stack's file, a module's file, a component in each, their names.
    #[test]
    fn the_scopes_alone_are_the_lowerings() {
        let unit = |name: &str, src: &str, path: Option<&str>| {
            let parse = crate::syntax::parser::parse(src);
            assert!(parse.errors.is_empty(), "{:?}", parse.errors);
            super::Unit {
                file: crate::diag::add_source(name, src),
                root: parse.syntax(),
                path: path.map(str::to_string),
                project: false,
            }
        };
        let units = [
            unit(
                "main.df",
                "\ninput n: int = 1\nlet k = n\nuse lan {}\n\
                 component box { input size: int\nlet twice = size * 2 }\n\
                 resource box b { size = k }\n",
                None,
            ),
            unit(
                "lan.df",
                "\ninput cidr: string = \"10.0.0.0/8\"\n\
                 component sub { input c: string\nlet d = c }\n",
                Some("lan"),
            ),
        ];
        let lowered = super::lower_stack(&units, &[0], true, super::Mode::Program, None, &[])
            .unwrap_or_else(|d| panic!("{d:?}"));
        let nodes = lowered.nodes.expect("the program's nodes");
        let alone = super::scopes(&units, &[0], &[]);
        assert_eq!(format!("{alone:?}"), format!("{:?}", nodes.scopes));
        assert!(alone.ids().count() >= 4, "{alone:?}");
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

    /// `let NAME = LITERAL [@rank]` is built as a `let` item (R-211): it
    /// lowers to its `let` fact, its errors the resolver's.
    #[test]
    fn a_literal_let_is_built_as_the_resolver_lowers_it() {
        let lowered = lower(
            "let a = 1\nlet b = \"x\" @default\nlet c = 500m\nlet d = true\np(v) where v = a",
        );
        assert!(
            lowered.contains(&"let(\"b\", \"x\", \"default\")".to_string()),
            "{lowered:?}"
        );
        for (src, msg) in [
            ("let a = 99999999999999999999", "integer out of range"),
            ("let a = 1 @later", "unknown rank `@later`"),
        ] {
            let e = error(src);
            assert!(e.contains(msg), "{src}: {e}");
        }
    }

    /// Every form of `let` is built as a `let` item (R-211 step 5) and
    /// lowers to its rules: typed (its `decl`), secret (its
    /// `__secret_let` paths), reading, with a clause, a `not { }` in it, a
    /// comprehension's helper in its value, an aggregate folded by its
    /// head or through a helper.
    #[test]
    fn every_let_is_built_as_a_let_item() {
        let src = "p(1)\nq(2)\np(2)\n\
             let a: int = 1\n\
             let b = x + 1 where p(x), x > 1\n\
             let c = a * 2\n\
             let s: secret(string) = \"pw\"\n\
             let o: { host: string, password: secret(string) } = { host = \"h\", password = \"p\" }\n\
             let r = x where p(x), not { q(x) }\n\
             let l = [y | p(y), not { q(y) }]\n\
             let n = count(x) where p(x)\n\
             let m = k where k = count(x), p(x), k > 1\n";
        let lowered = lower(src);
        for s in [
            "let(\"n\", count(X), \"normal\") :- p(X)",
            "__secret_let(\"o\", \"password\")",
            "__agg_0(count(X)) :- p(X)",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
    }

    /// Every form of rule and fact is built as a `rule` item (R-211 step
    /// 5) and lowers to its rules: a fact, a record fact, a
    /// ranked write, a head that reads (its reads after the clause), a
    /// helper its head's terms make, a constraint read as data, an
    /// aggregate folded by the head or through helpers.
    #[test]
    fn every_rule_is_built_as_a_rule_item() {
        let src = "decl zone(name, index)\n\
             p(1)\n\
             p(2)\n\
             zone(name: \"a\", index: 1)\n\
             arg(\"net.vpc\", \"a\", \"cidr\", \"x\") @override\n\
             let o = { a = 1 }\n\
             r(o.a, x) where p(x)\n\
             l([y | p(y), not { q(y) }]) where p(1)\n\
             q(2)\n\
             type_refine(\"t\", regex(\"^a$\"))\n\
             c(n) where n = count(x), p(x)\n\
             g(n) where n = count(x), p(x), n > 1\n";
        let lowered = lower(src);
        for s in [
            "arg(\"net.vpc\", \"a\", \"cidr\", \"x\", \"override\")",
            "r(__path(O, \"a\"), X) :- p(X), o(O)",
            "__neg_0(Y) :- p(Y), q(Y)",
            "c(count(X)) :- p(X)",
            "g(N) :- __agg_0(N), N > 1",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
    }

    /// Every form of check, output and input is built as an item (R-211
    /// step 5) and lowers to its rules: a deny with a detail
    /// and an aggregate, a warn, an output declared at its first row with a
    /// value or a rule, an object output, an exported relation, an input
    /// with a default, a refinement, a clause and object fields, one
    /// declared under two clauses (R-104).
    #[test]
    fn every_check_output_and_input_is_an_item() {
        let src = "input size: int = 3 check size > 0\n\
             input region: string = \"eu\" where cloud(\"aws\")\n\
             input region: string = \"us\" where cloud(\"gcp\")\n\
             input db { host: string = \"h\", port: int = 5432 check port > 0 }\n\
             p(1)\np(2)\ncloud(\"aws\")\n\
             deny \"too many: ${n}\" { n } where n = count(x), p(x), n > 1\n\
             warn \"p ${x}\" where p(x), not { cloud(\"gcp\") }\n\
             output total = 3\n\
             output each = x where p(x)\n\
             output each = 9\n\
             output obj { a = 1, b: { c = \"d\" } }\n\
             output p\n";
        let lowered = parse_as(src, true);
        let lowered = lowered
            .map(|p| show(&p.statements))
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert!(
            lowered.iter().any(|l| l.contains("__agg_0")),
            "{lowered:#?}"
        );
    }

    /// Every form of `set` is built as a `Set` item (R-211 step 5) and
    /// lowers to its rules: an attribute, `+=`, ranked, a
    /// `[_]` target (its reads before the value's), an element of a keyed
    /// list by its variable (R-69) and by its key (R-35), an input, a
    /// block whose lines share one clause (its aggregates folded per line,
    /// each through helpers of its own), `set from` a document.
    #[test]
    fn every_set_is_built_as_a_set_item() {
        let src = "input size: int = 1\n\
             resource db.postgres pg { public = false }\n\
             let k = 1\n\
             p(1)\np(2)\n\
             set pg.tags = { a = 1 } where p(1)\n\
             set pg.tags += { b = 2 } where p(2)\n\
             set pg.public = true @override where p(2)\n\
             set db.postgres[_].public = true where p(1)\n\
             set db.postgres[_].tags = { a = k } where p(1)\n\
             set c.owner = \"o\" where c in pg.databases\n\
             set pg.databases[\"a\"].owner = \"o\" where p(1)\n\
             set size = 2 where p(2)\n\
             set { pg.public = false @default, pg.tags = { c = n } } where n = count(x), p(x), n > 1\n\
             set from yaml.decode(io.read(\"c.yaml\")) where p(1)\n";
        let lowered = parse_as(src, true);
        let lowered = lowered
            .map(|p| show(&p.statements))
            .unwrap_or_else(|e| panic!("{e:#}"));
        for s in [
            "arg_add(\"db.postgres\", \"pg\", \"tags\", {b = 2}) :- p(2)",
            "\"override\") :- p(2)",
            "__agg_0(count(X))",
            "__agg_1(count(X))",
            "arg(\"input\", \"\", Path, Value, \"normal\")",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
    }

    /// Every form of resource block is built as a `Resource` item (R-211
    /// step 5) and lowers to its rules: entries that read
    /// (`+=`, ranked, an indexed path, a comprehension's helper), a
    /// bare, a quoted and a dotted header, a name from the clause bound
    /// after the entries' reads, a value body written out and one read.
    #[test]
    fn every_resource_is_built_as_a_resource_item() {
        let src = "let o = { public = true }\n\
             let k = 1\n\
             p(1)\np(2)\nq(2)\n\
             resource db.postgres a @default { public = false, tags += { a = k } @override }\n\
             resource db.postgres \"b.c\" { tags = { l = [x | p(x), not { q(x) }] } }\n\
             resource db.postgres \"d-${n}\" { tags = { n = k } } where p(n)\n\
             resource db.postgres e = { public = true }\n\
             resource db.postgres f = o\n\
             resource db.postgres g { databases[0].owner = \"o\" }\n";
        let lowered = lower(src);
        for s in [
            "resource \"db.postgres\" Addr { tags = {n = K} } :- p(N), k(K), Addr = __segment(str.format(\"d-%s\", N))",
            "resource \"db.postgres\" \"\\\"b.c\\\"\"",
            "__neg_0(X) :- p(X), q(X)",
            "resource \"db.postgres\" \"f\" {  = O } :- o(O)",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
        // A block binds no aggregate: a `let` does.
        let e =
            error("p(1)\nresource db.postgres h { tags = { n = n } } where n = count(x), p(x)\n");
        assert!(
            e.contains("an aggregate is bound in the body of a rule"),
            "{e}"
        );
    }

    /// Every form of relation input is built as a `RelationInput` item
    /// (R-211 step 5) and lowers to its rules: a stack's
    /// rows from a document under a clause, a component's relation its
    /// user gives.
    #[test]
    fn every_relation_input_is_an_item() {
        let src = "input p from csv.decode(io.read(\"p.csv\")) where z(1)\n\
             decl p(a: int, b: string)\n\
             z(1)\n\
             component c {\n  input q\n  decl q(a: int)\n}\n";
        let lowered = parse_as(src, true);
        let lowered = lowered
            .map(|p| show(&p.statements))
            .unwrap_or_else(|e| panic!("{e:#}"));
        let s = "p(A, B) :- z(1), Path = \"p.csv\", table.csv.p(Path, At, A, B)";
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }

    /// Every form of `let` with parameters is built as a `LetFn` item
    /// (R-211 step 5) and lowers to its rules: no clause, a
    /// clause whose `not { }` takes the demand among its positive
    /// literals, a value that reads, an aggregate folded by the head.
    #[test]
    fn every_let_with_parameters_is_an_item() {
        let src = "p(1, 2)\nq(2)\nlet k = 3\n\
             let f(a) = a + k\n\
             let g(a, b) = x where p(a, x), not { q(b) }\n\
             let h(a) = n where n = count(x), p(x, a)\n\
             r(f(1), g(1, 2), h(2))\n";
        let lowered = lower(src);
        for s in [
            "f(A, add(A, K)) :- f?(A), k(K)",
            "__neg_0(B) :- g?(A, B), p(A, X), q(B)",
            "h(A, count(X)) :- h?(A), p(X, A)",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
    }

    /// Every form of a provider's `use` is built as a `Provider` item
    /// (R-211 step 5) and lowers to its rules: its source and
    /// settings, renamed (R-115), an `expect_account` under the clause
    /// again (its `not { }` a helper of its own), declared twice under
    /// clauses (R-104).
    #[test]
    fn every_provider_use_is_an_item() {
        let src = "let k = \"eu\"\nz(1)\nq(2)\n\
             use p1 { source = \"./p1\", region = k, expect_account = \"a\" } where z(1), not { q(1) }\n\
             use p2 { source = \"./p2\", r = 1 } where z(1)\n\
             use p2 { r = 2 } where q(2)\n\
             use p3 as c { source = \"./p3\" }\n";
        let lowered = parse_as(src, true);
        let lowered = lowered
            .map(|p| show(&p.statements))
            .unwrap_or_else(|e| panic!("{e:#}"));
        for s in [
            "provider_config(\"p1\", {region = K}) :- z(1), not __neg_0(), k(K)",
            "provider_expect_account(\"p1\", \"a\") :- z(1), not __neg_1()",
            "__declared(\"use p2\", 1) :- q(2)",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
        }
    }

    /// A row a copy's block gives its component's relation input folds
    /// its aggregate as a rule does: the head applies it, or a helper
    /// does (it was left in the body as `N = count(X)`, a call of no
    /// function), a table's rows too.
    #[test]
    fn a_row_of_a_copy_folds_its_aggregate() {
        let src = "z(1)\nz(2)\n\
             component c {\n  input p\n  input t\n  decl p(a: int)\n  decl t(a: int)\n  q(x) where p(x), t(x)\n}\n\
             resource c one {\n  p(n) where n = count(x), z(x)\n\
               t from csv.decode(io.read(\"t.csv\")) where n = count(x), z(x), n > 1\n}\n";
        let lowered = parse_as(src, true).unwrap_or_else(|e| panic!("{e:#}"));
        let rows: Vec<String> = lowered
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Instance(i) => Some(show(&i.rows)),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            rows,
            [
                "p(count(X)) :- z(X)",
                "t(A) :- __agg_0(A, N), N > 1",
                "__agg_0(A, count(X)) :- z(X), Path = \"t.csv\", table.csv.t(Path, At, A)",
            ]
        );
    }

    /// An aggregate is bound in the body of a rule, a check or a `let`
    /// (R-59): an input's clause, which every read of the input repeats,
    /// and a provider's `use` bind none, as a resource's and a copy's do
    /// not; the help says where it is bound instead.
    #[test]
    fn an_inputs_and_a_providers_clause_bind_no_aggregate() {
        let want = "an aggregate is bound in the body of a rule, a check or a `let`; bind it in \
                    a `let` and read that here";
        for src in [
            "input region: string = \"eu\" where n = count(x), z(x), n > 1\nz(1)\n",
            "input region: string = \"eu\" where n = count(x), z(x), n > 1\n\
             input region: string = \"us\" where n = count(x), z(x), n <= 1\nz(1)\n",
            "z(1)\nuse fake where n = count(x), z(x), n > 1\n",
        ] {
            let e = file_error(src);
            assert!(e.contains(want), "{src}: {e}");
        }
        // Bound in a `let` and read there, the clause holds where it does.
        let ok = "input region: string = \"eu\" where many\n\
                  let many = true where n = count(x), z(x), n > 1\nz(1)\n";
        parse_as(ok, true).unwrap_or_else(|e| panic!("{e:#}"));
    }

    /// Every form of copy is built as a `Copy` item (R-211 step 5) and
    /// lowers to its rules: inputs that read, a name from
    /// the clause (R-191) bound before the inputs' reads, rows its block
    /// gives (a rule whose `not { }` helper follows the copy, a table),
    /// under a clause.
    #[test]
    fn every_copy_is_an_item() {
        let src = "let k = 1\nz(1)\nz(2)\nq(3)\n\
             component c {\n  input size: int\n  input p\n  input t\n  decl p(a: int)\n  decl t(a: int)\n  r(x) where p(x), t(x)\n}\n\
             resource c one {\n  size = k\n  p(x) where z(x), not { q(x) }\n\
               t from csv.decode(io.read(\"t.csv\"))\n}\n\
             resource c \"n-${i}\" { size = i } where z(i)\n";
        let lowered = parse_as(src, true);
        let lowered = lowered
            .map(|p| show(&p.statements))
            .unwrap_or_else(|e| panic!("{e:#}"));
        for s in [
            "instance c one { size = K } :- k(K)",
            "__neg_0(X) :- z(X), q(X)",
            "instance c n-${i} { size = I } :- z(I), Addr = __segment(str.format(\"n-%s\", I))",
        ] {
            assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
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

    /// An entry that is only a name is its pun (R-33), resolved as the
    /// value would be: a clause variable, a `let` (read through its cell),
    /// with a rank.
    #[test]
    fn a_bare_entry_is_its_name() {
        let got = lower(
            "let tags = { team = \"x\" }\n\
             resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet \"s-${zone}\" {\n\
               zone\n\
               meta.zone = zone\n\
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
            // `p`'s column holds the widget itself (R-185).
            "p(__ref(\"acme.widget\", X, \"\")) :- want(\"acme.widget\", X), attr(\"acme.widget\", \
             X, \"size\", Size), Size > 1"
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
             deny \"public\" { resource = p } where p in db.postgres, not p.public == false\n\
             set r.tags = { team = \"x\" } where r in resource\n\
             let xs = [1, 2]\n\
             let ys = [{ name = \"a\", net = 1 }]\n\
             q(x) where x = xs[i], i >= 0, x not in [3]\n\
             ok(1) where pg in db.postgres, has pg.public, not \"other\" in db.postgres\n\
             big(n) where n in world.net.vpc, world.net.vpc[n].size > 3\n\
             pair(n, c) where ys[_] = { name = n, net = c }\n",
        );
        assert_eq!(
            &got[1..],
            [
                "deny(\"public\", {resource = P}) :- want(\"db.postgres\", P), not attr(\"db.postgres\", P, \"public\", false)",
                "arg(Type, R, \"tags\", {team = \"x\"}) :- want(Type, R)",
                "let(\"xs\", [1, 2], \"normal\")",
                "let(\"ys\", [{name = \"a\", net = 1}], \"normal\")",
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
            "resource db.pg d { s = { a = 1 } }\n\
             c(x) where q(x), has x.limits\n\
             n(x) where q(x), not has x.limits\n\
             r(1) where not d.s.a == 2\n",
        );
        assert_eq!(
            &got[1..],
            [
                "c(X) :- q(X), Has' = __path(X, \"limits\")",
                "n(X) :- q(X), not __neg_0(X)",
                "__neg_0(X) :- q(X), Has' = __path(X, \"limits\")",
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
