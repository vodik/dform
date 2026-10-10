//! One evaluation of one deployment for the editor: `dform plan`'s
//! (`dform_core::deployment::evaluate`), up to and including the plan's
//! policy pass, read only. The program's files are read through the
//! editor's buffers; the world and state are the deployment's own, read
//! and never written (a bucket's when there are credentials for it);
//! nothing is applied. What `plan` prints as an error or a warning comes
//! back as a [`Problem`] at the span it names, with the fixes its
//! diagnostic carries.

use anyhow::Result;
use dform_core::ast::{Program, Span, Stmt};
use dform_core::circuit::{Leaf, NodeId, View};
use dform_core::deployment::{self, At, Note, Notes};
use dform_core::engine::EvalResult;
use dform_core::lint::Collision;
use dform_core::plugin::Launch;
use dform_core::project;
use dform_core::query::Redactor;
use dform_core::schema::Schema;
use dform_core::store::{self, S3Spec};
use dform_core::{loader, transform};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use dform_core::deployment::{Reader, Severity};

/// The deployment to evaluate: a stack's file and the key values the
/// selected environment names (the rest take their defaults).
#[derive(Debug, Clone)]
pub struct Target {
    pub file: PathBuf,
    pub keys: Vec<(String, String)>,
}

/// Where a problem or a contributor is.
#[derive(Debug, Clone)]
pub enum Where {
    /// A span of a source the loader registered.
    Span(Span),
    /// `file:line:col ...`, as a given fact's provenance names it.
    Place(String),
    /// Nowhere more precise: the stack's own file, at its start.
    Top,
    /// A span, resolved: a file and a byte range in it.
    Bytes(PathBuf, usize, usize),
    /// A place, resolved: a file, a line and a column, 1-based.
    Line(PathBuf, usize, usize),
}

/// By place: a span by where it is (`Span`'s own equality holds of any two).
impl PartialEq for Where {
    fn eq(&self, other: &Where) -> bool {
        match (self, other) {
            (Where::Span(a), Where::Span(b)) => a.same_place(b),
            (Where::Place(a), Where::Place(b)) => a == b,
            (Where::Top, Where::Top) => true,
            (Where::Bytes(f, a, b), Where::Bytes(g, c, d)) => (f, a, b) == (g, c, d),
            (Where::Line(f, a, b), Where::Line(g, c, d)) => (f, a, b) == (g, c, d),
            _ => false,
        }
    }
}

impl From<At> for Where {
    fn from(at: At) -> Where {
        match at {
            At::Span(s) => Where::Span(s),
            At::Place(p) => Where::Place(p),
            At::Top => Where::Top,
        }
    }
}

impl Where {
    /// A span or place as a file: sources are named relative to the
    /// working directory (the evaluation's project root).
    fn resolve(self, cwd: &Path) -> Where {
        match self {
            Where::Span(s) => match dform_core::diag::location(s) {
                Some((name, _, _)) => {
                    Where::Bytes(cwd.join(name), s.start as usize, s.end as usize)
                }
                None => Where::Top,
            },
            Where::Place(p) => {
                // `stacks/dform.df:18:1 (arg)`.
                let at = p.split(' ').next().unwrap_or_default();
                let mut parts = at.rsplitn(3, ':');
                let (Some(col), Some(line), Some(name)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Where::Top;
                };
                match (line.parse(), col.parse()) {
                    (Ok(l), Ok(c)) => Where::Line(cwd.join(name), l, c),
                    _ => Where::Top,
                }
            }
            w => w,
        }
    }
}

/// A fix a problem's diagnostic carries, resolved: per edit, a file, a
/// byte range in it and the text that replaces it.
#[derive(Debug, Clone)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<(PathBuf, usize, usize, String)>,
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub severity: Severity,
    pub message: String,
    pub at: Where,
    pub related: Vec<(Where, String)>,
    pub fixes: Vec<Fix>,
}

impl Problem {
    fn top(severity: Severity, message: impl Into<String>) -> Problem {
        Problem {
            severity,
            message: message.into(),
            at: Where::Top,
            related: Vec::new(),
            fixes: Vec::new(),
        }
    }
}

/// A deformation the plan of the selected deployment has for a resource,
/// as `dform plan` prints it (a no-op included).
#[derive(Debug, Clone)]
pub struct Planned {
    pub addr: dform_core::address::Address,
    pub kind: dform_core::provider::ActionKind,
    /// The nulls it waits on, when it is held until a boundary.
    pub on: Option<Vec<String>>,
}

/// An evaluation that got as far as facts: the policy pass's, which `dform
/// why` reads, with the program it evaluated.
pub struct Evaluated {
    /// The stack's file.
    pub file: PathBuf,
    /// `dform[env=staging] (env from its default)`.
    pub deployment: String,
    /// The plan, when one could be made.
    pub plan: Option<Vec<Planned>>,
    /// The stack's providers (as `--provider` names them).
    pub providers: Vec<String>,
    /// The file of each source the program's spans name.
    pub files: BTreeMap<u32, PathBuf>,
    pub res: EvalResult,
    pub schema: Schema,
    /// The program as evaluated, and as lowered (its facts' spans).
    pub program: Program,
    pub lowered: Option<Program>,
    /// Every relation's columns, declared or inferred (R-34).
    pub signatures: dform_core::infer::Signatures,
    pub redact: Redactor,
    /// The collision lint's findings over the program's own evaluation.
    pub collisions: Vec<Collision>,
    /// The stack's keys: a value's chain ends at one (`why`).
    pub keys: BTreeSet<String>,
    /// The circuit read upward: per fact node, the fact nodes derived
    /// from it; per rule id, the fact nodes it derived.
    index: OnceCell<Index>,
}

#[derive(Default)]
struct Index {
    parents: BTreeMap<NodeId, Vec<NodeId>>,
    by_rule: BTreeMap<String, Vec<NodeId>>,
}

impl Evaluated {
    fn index(&self) -> &Index {
        self.index.get_or_init(|| {
            let c = &self.res.circuit;
            let mut ix = Index::default();
            for f in c.facts() {
                let Some(p) = c.fact_id(&f) else { continue };
                let View::Fact { alts, .. } = c.view(p) else {
                    continue;
                };
                for a in alts {
                    let View::Times { children, .. } = c.view(*a) else {
                        continue;
                    };
                    for ch in children {
                        match c.view(*ch) {
                            View::Fact { .. } => {
                                let e = ix.parents.entry(*ch).or_default();
                                if !e.contains(&p) {
                                    e.push(p);
                                }
                            }
                            View::Leaf(Leaf::Rule { id }) => {
                                let e = ix.by_rule.entry(id.clone()).or_default();
                                if !e.contains(&p) {
                                    e.push(p);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            ix
        })
    }

    /// The fact nodes one firing of which reads fact `id`.
    pub fn parents(&self, id: NodeId) -> &[NodeId] {
        self.index().parents.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The fact nodes rule `id` (`r12`) derived.
    pub fn derived_by(&self, id: &str) -> &[NodeId] {
        self.index().by_rule.get(id).map_or(&[], Vec::as_slice)
    }

    /// Where rule `id` (`r12`) is written, when it is.
    pub fn rule_span(&self, id: &str) -> Option<Span> {
        deployment::rule_span(&self.res, id)
    }
}

/// The evaluation of `t` and what it found wrong.
pub struct Outcome {
    pub evaluated: Option<Evaluated>,
    pub problems: Vec<Problem>,
}

/// Evaluate `t`, its providers reached through `launch`, `version` the
/// running dform's (for dform.toml's requirement).
pub fn evaluate(t: &Target, launch: &dyn Launch, read: Reader, version: &str) -> Outcome {
    // The sources this evaluation registers are dropped with it, but for
    // the files the loader keeps; spans are resolved to files before then.
    let _scope = dform_core::diag::Scope::new();
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut problems = Vec::new();
    let mut evaluated = match run(t, launch, read, version, &mut problems) {
        Ok(e) => Some(e),
        Err(e) => {
            problems.extend(deployment::of_error(&e).into_iter().map(of_core));
            None
        }
    };
    if let Some(e) = &mut evaluated {
        let ids = e
            .res
            .rules
            .iter()
            .map(|r| r.head.span.file)
            .chain(e.program.statements.iter().filter_map(|s| match s {
                Stmt::Fact(a) => Some(a.span.file),
                _ => None,
            }))
            .chain(e.lowered.iter().flat_map(|p| {
                p.statements.iter().filter_map(|s| match s {
                    Stmt::Fact(a) => Some(a.span.file),
                    _ => None,
                })
            }))
            .filter(|f| *f != 0)
            .collect::<BTreeSet<u32>>();
        for id in ids {
            let span = Span {
                file: id,
                ..Default::default()
            };
            if let Some((name, _, _)) = dform_core::diag::location(span) {
                let f = cwd.join(name);
                e.files.insert(id, std::fs::canonicalize(&f).unwrap_or(f));
            }
        }
    }
    // A place in no file the editor can open (the policy rules every
    // evaluation carries, `zset::POLICY_RULES`) is the stack's file's top.
    let canon = |w: Where| match w.resolve(&cwd) {
        Where::Bytes(f, a, b) => match std::fs::canonicalize(&f) {
            Ok(f) => Where::Bytes(f, a, b),
            Err(_) if read(&f).is_ok() => Where::Bytes(f, a, b),
            Err(_) => Where::Top,
        },
        Where::Line(f, a, b) => match std::fs::canonicalize(&f) {
            Ok(f) => Where::Line(f, a, b),
            Err(_) if read(&f).is_ok() => Where::Line(f, a, b),
            Err(_) => Where::Top,
        },
        w => w,
    };
    for p in &mut problems {
        p.at = canon(std::mem::replace(&mut p.at, Where::Top));
        for (w, _) in &mut p.related {
            *w = canon(std::mem::replace(w, Where::Top));
        }
    }
    Outcome {
        evaluated,
        problems,
    }
}

/// A problem of the evaluation's, its fixes resolved to files while their
/// sources are registered; a fix with an edit that has no place is left
/// out.
fn of_core(p: deployment::Problem) -> Problem {
    let cwd = std::env::current_dir().unwrap_or_default();
    let fixes = p
        .fixes
        .iter()
        .filter_map(|f| {
            let edits = f
                .edits
                .iter()
                .map(|(span, text)| {
                    let (name, _, _) = dform_core::diag::location(*span)?;
                    let file = cwd.join(name);
                    let file = std::fs::canonicalize(&file).unwrap_or(file);
                    Some((file, span.start as usize, span.end as usize, text.clone()))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Fix {
                title: f.title.clone(),
                edits,
            })
        })
        .collect();
    Problem {
        severity: p.severity,
        message: p.message,
        at: p.at.into(),
        related: p.related.into_iter().map(|(w, m)| (w.into(), m)).collect(),
        fixes,
    }
}

fn run(
    t: &Target,
    launch: &dyn Launch,
    read: Reader,
    version: &str,
    problems: &mut Vec<Problem>,
) -> Result<Evaluated> {
    let mut notes = Notes::default();
    let unread = RefCell::new(Vec::new());
    let r = run_noted(t, launch, read, version, &mut notes, &unread, problems);
    // What the evaluation said as it went: the lint's warnings, the
    // collision lint's, a bucket not read; a
    // `warn` fact is published at its rule.
    for w in unread.into_inner() {
        problems.push(Problem::top(Severity::Warning, w));
    }
    for n in &notes.0 {
        match n {
            Note::Warning(w) | Note::Collision(w) => {
                problems.push(Problem::top(Severity::Warning, w.clone()))
            }
            Note::Policy(_) | Note::Resolved(_) | Note::TableMoved(_) | Note::Computed(..) => {}
        }
    }
    r
}

fn run_noted(
    t: &Target,
    launch: &dyn Launch,
    read: Reader,
    version: &str,
    notes: &mut Notes,
    unread: &RefCell<Vec<String>>,
    problems: &mut Vec<Problem>,
) -> Result<Evaluated> {
    let root = match project::Project::find(t.file.parent().unwrap_or(Path::new(".")), version)? {
        Some(p) => p.state_root(),
        None => PathBuf::from(project::STATE_DIR),
    };
    let target = deployment::Target {
        files: vec![t.file.clone()],
        ..Default::default()
    };
    let loaded = deployment::load(&target, version, read, notes)?;
    loaded.require_provider()?;
    // The selected environment names keys of every stack in the project;
    // this stack takes its own.
    let set = t
        .keys
        .iter()
        .filter(|(k, _)| loaded.cfg.keys.iter().any(|(x, _)| x == k))
        .map(|(k, v)| (k.clone(), deployment::value_of(v)))
        .collect();
    // A bucket is read when there are credentials for it; without, the
    // deployment is evaluated as if nothing were deployed there.
    let s3 = |spec: &S3Spec| -> Result<Arc<dyn store::Store>> {
        match dform_s3::S3Store::open(spec, "") {
            Ok(s) => Ok(Arc::new(s)),
            Err(e) => {
                let w = format!(
                    "{spec} is not read ({e:#}): evaluated as if nothing were deployed there"
                );
                if !unread.borrow().contains(&w) {
                    unread.borrow_mut().push(w);
                }
                Ok(Arc::new(store::MemoryStore::new()))
            }
        }
    };
    let located = loaded.locate(
        &deployment::Selection {
            root: root.clone(),
            set,
            ..Default::default()
        },
        &s3,
        notes,
    )?;
    let outputs = match located.read_outputs(&s3) {
        Ok(r) => r,
        Err(e) => {
            unread
                .borrow_mut()
                .push(format!("other stacks' outputs are not read: {e:#}"));
            Vec::new()
        }
    };
    let opts = deployment::Options {
        cache: Some(root.join("cache")),
        check_types: true,
        collisions: true,
        policy: true,
        ..deployment::Options::new(launch)
    };
    let mut ev = located.evaluate(outputs, &opts, notes)?;
    let plan = match &ev.policy {
        Some(Ok(p)) => Some(
            p.plan
                .actions
                .iter()
                .map(|a| Planned {
                    addr: a.addr.clone(),
                    kind: a.kind.clone(),
                    on: dform_core::report::waits_on(a, &p.sections),
                })
                .collect(),
        ),
        _ => None,
    };
    let explained = ev.explained();
    let program = ev.evaluator.program.clone();
    problems.extend(explained.problems(&program).into_iter().map(of_core));
    let lowered = transform::lower(&program).ok();
    Ok(Evaluated {
        file: t.file.clone(),
        deployment: ev.located.instance.describe(),
        plan,
        providers: ev.located.loaded.providers.clone(),
        files: BTreeMap::new(),
        schema: ev.schema().clone(),
        res: explained.res,
        lowered: lowered.as_ref().map(|l| l.program.clone()),
        signatures: lowered.map(|l| l.signatures).unwrap_or_default(),
        program,
        redact: explained.redact,
        collisions: std::mem::take(&mut ev.collisions),
        keys: ev
            .located
            .instance
            .key
            .iter()
            .map(|(k, _)| k.clone())
            .collect(),
        index: OnceCell::new(),
    })
}

/// Where fact `id` comes from: the nearest rule (or stated fact) that is
/// written somewhere, and the contributions of the attributes below it,
/// each with its rank.
pub fn provenance(e: &Evaluated, id: NodeId) -> (Option<Where>, Vec<(Where, String)>) {
    let (at, related) = deployment::provenance(&e.res, id);
    (
        at.map(Where::from),
        related.into_iter().map(|(w, m)| (w.into(), m)).collect(),
    )
}

/// Where the firings of fact `id` are written: its first rule or stated
/// fact that has a place.
pub fn written(e: &Evaluated, id: NodeId) -> Option<Where> {
    deployment::written(&e.res, id).map(Where::from)
}

/// The key values a stack offers: each enum value of each key input.
pub fn choices(file: &Path, keys: &[String], read: Reader) -> Result<Vec<String>> {
    let program = loader::load_program_with(&[file.to_path_buf()], read)?;
    let mut values = Vec::new();
    for st in &program.statements {
        let Stmt::Input(i) = st else { continue };
        if !keys.contains(&i.name) {
            continue;
        }
        if let dform_core::ast::TypeExpr::Apply(n, args) = &i.ty
            && n == "enum"
        {
            for a in args {
                if let dform_core::ast::TypeExpr::Str(v) | dform_core::ast::TypeExpr::Name(v) = a {
                    values.push(format!("{}={v}", i.name));
                }
            }
        }
    }
    Ok(values)
}
