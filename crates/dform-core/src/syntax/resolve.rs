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
    Atom, BindArg, Config, ExternFn, FieldAssign, FieldOp, Lit, Program, Rank, Span, Stmt, Term,
    TypeExpr, Via, str_term, var,
};
use crate::diag::Diagnostic;
use crate::program::bind;
use crate::program::node::{
    BinOp, CmpOp, Coll, ExprKind, GoalId, GoalKind, Has, HasMark, ListPart, Negated, ObjPart,
    RelArgs, RelRef,
};
use crate::program::node::{CopyKind, TypeAttr};
use crate::program::scope::{self as scopes, DeclKind, DeclRef, Names, Reach};
use crate::program::{ClauseId, ExprId, ItemId, ScopeId};
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
mod atoms;
mod collect;
mod copies;
mod decls;
mod documents;
mod inputs;
mod instances;
mod literals;
mod lookup;
mod paths;
mod reads;
mod rules;
mod sets;
mod statements;
mod terms;
mod undeclared;
mod unfold;
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
    program.diags = l.said();
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
    /// What the body places for a `set` (`unfold`): its relation
    /// literals unfolded, each with its rule, and the literals that name
    /// a place.
    placing: unfold::Placing,
    /// Source variable -> the place it names (`p = w.spec.template`): a
    /// chain on it is a path into that resource.
    places: BTreeMap<String, unfold::Place>,
    /// Lowered names the variables of a rule unfolded into the statement
    /// hold: no variable of the statement's own takes one.
    taken: BTreeSet<String>,
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

/// A block's entries as lowered: each, its value's node, and where the
/// statement's helpers its terms made end.
type Entries = Vec<(FieldAssign, ExprId, usize)>;

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
    rank: Option<Rank>,
    /// Where the statement's helpers the head made start.
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
    /// What lowering a rule unfolded into a `set` said (`unfold`): said
    /// once the walk is done, unless the rule's own lowering said it.
    spliced: Vec<Diagnostic>,
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
    /// The goals of an object pattern's field reads, after the literal
    /// that binds it (`pattern`).
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
            spliced: Vec::new(),
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
/// target and its value, whose terms made the statement's helpers
/// `made.0..made.1` and `made.1..made.2`.
struct Contribution {
    span: Span,
    target: Target,
    op: FieldOp,
    rank: Option<Rank>,
    value: ExprId,
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
            op: self.op,
            rank: self.rank,
            value: self.value,
            made: (&helpers[a..b], &helpers[b..c]),
        }
    }
}

/// A relation's rows read from a document (R-39), as the resolver
/// lowered them: the externs reading it, then the rule `head :- ..`
/// whose document's reads made the statement's helpers from `made`.
struct TableRows {
    externs: Vec<Stmt>,
    head: Atom,
    /// The goal reading the document's rows.
    rows: GoalId,
    made: usize,
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

/// A relation's arguments as the terms of its head.
fn arg_terms(args: &RelArgs) -> Vec<bind::Term> {
    match args {
        RelArgs::Positional(ps) => ps.iter().map(|&p| bind::Term::Pattern(p)).collect(),
        RelArgs::Record(fs) => fs.iter().map(|&(_, p)| bind::Term::Pattern(p)).collect(),
    }
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

#[cfg(test)]
mod tests;
