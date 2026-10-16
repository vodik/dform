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
mod instances;
mod lookup;
mod paths;
mod reads;
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

    // --- statements with bodies -------------------------------------------

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
            (true, Some(_)) => self.built_literal(n, &tests),
            (false, Some(s)) => s.failed(),
            _ => {}
        }
        r
    }

    /// The written literal `n`, made after the `tests`: the goal built of
    /// it gathered (R-211 step 4).
    fn built_literal(&mut self, n: &SyntaxNode, tests: &[(Option<ExprId>, GoalId)]) {
        let span = self.span(n);
        let Some(s) = &mut self.gather else { return };
        s.literal(&mut self.program, span, tests, &self.helpers);
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
