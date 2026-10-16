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
mod atoms;
mod copies;
mod documents;
mod inputs;
mod instances;
mod literals;
mod lookup;
mod paths;
mod reads;
mod rules;
mod sets;
mod terms;
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

    // --- resolution -------------------------------------------------------
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
mod tests;
