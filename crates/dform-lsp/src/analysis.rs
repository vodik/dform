//! One evaluation of one deployment for the editor: `dform plan`'s path
//! (`cli::run_with`) up to and including the plan's policy pass, read
//! only. The program's files are read through the editor's buffers; the
//! world and state are the deployment's own, read and never written;
//! nothing is applied. What `plan` prints as an error or a warning comes
//! back as a [`Problem`] at the span it names.

use anyhow::Result;
use dform_core::ast::{Atom, Program, Span, Stmt, Term};
use dform_core::circuit::{Leaf, NodeId, View};
use dform_core::diag::{Diagnostic, Diagnostics};
use dform_core::engine::{self, EvalResult};
use dform_core::plugin::{self, Launch, Providers};
use dform_core::project::{self, Manifest};
use dform_core::query::Redactor;
use dform_core::schema::Schema;
use dform_core::store::{self, Location, S3Spec};
use dform_core::value::Value;
use dform_core::{
    executor, externs, inputs, ir, lint, loader, plan_print, refine, scenario, secrets, stack,
    state, stuck, tables, transform, watch, zset,
};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// What the editor reads a file's text through: its buffer if open, else
/// the disk.
pub type Reader<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;

/// The deployment to evaluate: a stack's file, the key values the
/// selected environment names (the rest take their defaults), and a
/// scenario.
#[derive(Debug, Clone)]
pub struct Target {
    pub file: PathBuf,
    pub keys: Vec<(String, String)>,
    pub scenario: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
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

/// By place: a span by its bytes (`Span`'s own equality holds of any two).
impl PartialEq for Where {
    fn eq(&self, other: &Where) -> bool {
        match (self, other) {
            (Where::Span(a), Where::Span(b)) => {
                (a.file, a.start, a.end) == (b.file, b.start, b.end)
            }
            (Where::Place(a), Where::Place(b)) => a == b,
            (Where::Top, Where::Top) => true,
            (Where::Bytes(f, a, b), Where::Bytes(g, c, d)) => (f, a, b) == (g, c, d),
            (Where::Line(f, a, b), Where::Line(g, c, d)) => (f, a, b) == (g, c, d),
            _ => false,
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

#[derive(Debug, Clone)]
pub struct Problem {
    pub severity: Severity,
    pub message: String,
    pub at: Where,
    pub related: Vec<(Where, String)>,
}

impl Problem {
    fn top(severity: Severity, message: impl Into<String>) -> Problem {
        Problem {
            severity,
            message: message.into(),
            at: Where::Top,
            related: Vec::new(),
        }
    }
}

/// An evaluation that got as far as facts: the policy pass's, which `dform
/// why` reads, with the program it evaluated.
pub struct Evaluated {
    /// `dform[env=staging] (env from its default)`.
    pub deployment: String,
    /// The stack's providers (as `--provider` names them).
    pub providers: Vec<String>,
    /// The file of each source the program's spans name.
    pub files: BTreeMap<u32, PathBuf>,
    pub res: EvalResult,
    pub schema: Schema,
    /// The program as evaluated, and as lowered (its facts' spans).
    pub program: Program,
    pub lowered: Option<Program>,
    pub redact: Redactor,
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
        let i: usize = id.strip_prefix('r')?.parse().ok()?;
        let span = self.res.rules.get(i)?.head.span;
        (!span.is_none()).then_some(span)
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
            problems.extend(of_error(&e));
            None
        }
    };
    if let Some(e) = &mut evaluated {
        problems.extend(policy_problems(e));
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
    let canon = |w: Where| match w.resolve(&cwd) {
        Where::Bytes(f, a, b) => Where::Bytes(std::fs::canonicalize(&f).unwrap_or(f), a, b),
        Where::Line(f, a, b) => Where::Line(std::fs::canonicalize(&f).unwrap_or(f), a, b),
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

/// An error as problems: its diagnostics at their spans, else its text at
/// the top of the stack's file.
pub fn of_error(e: &anyhow::Error) -> Vec<Problem> {
    match e.chain().find_map(|x| x.downcast_ref::<Diagnostics>()) {
        Some(Diagnostics(ds)) => ds.iter().map(of_diagnostic).collect(),
        None => vec![Problem::top(Severity::Error, format!("{e:#}"))],
    }
}

fn of_diagnostic(d: &Diagnostic) -> Problem {
    let mut message = d.message.clone();
    for n in &d.notes {
        message.push_str(&format!("\nnote: {n}"));
    }
    if let Some(h) = &d.help {
        message.push_str(&format!("\nhelp: {h}"));
    }
    Problem {
        severity: Severity::Error,
        message,
        at: Where::Span(d.span),
        related: d
            .labels
            .iter()
            .map(|(s, m)| (Where::Span(*s), m.clone()))
            .collect(),
    }
}

fn run(
    t: &Target,
    launch: &dyn Launch,
    read: Reader,
    version: &str,
    problems: &mut Vec<Problem>,
) -> Result<Evaluated> {
    let files = vec![t.file.clone()];
    let manifest = match project::manifest_root(&t.file) {
        Some(root) => Some(Manifest::load(&root.join(project::MANIFEST), version)?),
        None => None,
    };
    let root = match project::Project::find(t.file.parent().unwrap_or(Path::new(".")), version)? {
        Some(p) => p.state_root(),
        None => PathBuf::from(project::STATE_DIR),
    };
    let mut program = loader::load_program_with(&files, read)?;
    let relations = watch::take(&mut program)?;
    program.statements.extend(watch::read(&relations)?);
    if let Some(name) = &t.scenario {
        // The selected scenario is one stack's; another is evaluated as is.
        if scenario::names(&program)?.contains(name) {
            program = scenario::select(&program, name)?;
        } else {
            problems.push(Problem::top(
                Severity::Warning,
                format!("the selected scenario {name} is not this stack's: evaluated without it"),
            ));
        }
    }
    if let Some(m) = &manifest {
        with_default_unknowns(&mut program, m);
    }
    let mut cfg = stack::config(&program)?;
    if let Some(m) = &manifest {
        with_manifest(&mut cfg, m, &t.file);
    }
    let stack_name = cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&t.file));
    let key_names: Vec<&str> = cfg.keys.iter().map(|(k, _)| k.as_str()).collect();
    // The selected environment names keys of every stack in the project;
    // this stack takes its own.
    let keys: Vec<(String, String)> = t
        .keys
        .iter()
        .filter(|(k, _)| key_names.contains(&k.as_str()))
        .cloned()
        .collect();
    let providers = cfg.providers.clone();
    let lowered = transform::lower(&program).ok();
    let declared = lowered
        .as_ref()
        .map(|l| l.inputs.clone())
        .unwrap_or_default();
    let mut given = input_fact_keys(&program);
    let set_keys: Vec<String> = keys.iter().map(|(k, _)| k.clone()).collect();
    for w in lint::lint(&program, &set_keys) {
        problems.push(Problem::top(Severity::Warning, w));
    }
    let program = zset::with_policy_rules(program)?;
    let mut set = Vec::new();
    for (k, v) in &keys {
        given.insert(k.clone());
        set.push((k.clone(), value_of(v)));
    }
    let set_facts = inputs::set_facts(&declared, &set)?;
    let instance = stack::instance(&cfg, &stack_name, &program, &set_facts)?;
    let deployment = instance.name();
    inputs::check_required(&declared, &given)?;
    // Where the stack's deployments are, and this one's objects: there, or
    // where it was handed over to (`cli::run_with`).
    let base = match &cfg.backend {
        Some(stack::Backend::Local(dir)) => Location::Local(state::local_dir(&root, dir)),
        Some(stack::Backend::S3(spec)) => Location::S3(spec.clone()),
        None => Location::Local(root.join(&stack_name)),
    };
    let home = instance.dir(&root.join(&stack_name));
    let location = match stack::handed_over(&root, &deployment)? {
        Some((_, loc)) => loc,
        None => base.child(instance.segment().as_deref()),
    };
    let paths = state::StackPaths {
        state: match &location {
            Location::Local(dir) => state::state_path(dir),
            Location::S3(_) => state::state_path(&home),
        },
        world: stack::world_file(&location, &home),
        inventory: root.join("inventory.json"),
    };
    // A bucket is out of the editor's reach: an s3 deployment is evaluated
    // as if nothing were deployed, and another stack's outputs published
    // there are not read.
    let no_s3 = |spec: &S3Spec| -> Result<std::sync::Arc<dyn store::Store>> {
        anyhow::bail!("{spec}: the language server does not read s3")
    };
    let mut st = match &location {
        Location::S3(spec) => {
            problems.push(Problem::top(
                Severity::Warning,
                format!(
                    "deployment {deployment}: its state is in {spec}, which the language \
                     server does not read; evaluated as if nothing were deployed"
                ),
            ));
            state::State {
                version: 1,
                ..Default::default()
            }
        }
        Location::Local(_) => {
            store::Deployment::new(location.open(&no_s3)?, &deployment, Default::default())
                .load_state()?
        }
    };
    let (named, any_name) = lowered
        .as_ref()
        .map(|l| stack::named_outputs(&l.program))
        .unwrap_or_default();
    let remotes = manifest.as_ref().map(|m| m.remotes()).unwrap_or_default();
    let read_outputs = match stack::stack_outputs(
        &root,
        &deployment,
        (!any_name).then_some(&named),
        &remotes,
        &no_s3,
    ) {
        Ok(r) => r,
        Err(e) => {
            problems.push(Problem::top(
                Severity::Warning,
                format!("other stacks' outputs are not read: {e:#}"),
            ));
            Vec::new()
        }
    };
    let secret_outputs = stack::secret_outputs(&read_outputs);
    let backend = Providers::start_deferred(
        launch,
        &providers,
        &plugin::Config {
            world: paths.world.clone(),
            inventory: paths.inventory.clone(),
            chaos: Vec::new(),
            cache: Some(root.join("cache")),
            configured: provider_configs(&program),
            stack: deployment.clone(),
            blocks: cfg.provider_blocks.clone(),
        },
    )?;
    let (no_program, no_fns) = (Program { statements: vec![] }, vec![]);
    let program_dir = project::base_of(&t.file);
    let tables = tables::Tables::default();
    let externs = externs::Externs::new(
        lowered.as_ref().map_or(&no_program, |l| &l.program),
        lowered.as_ref().map_or(&no_fns, |l| &l.extern_fns),
        |f, inputs| {
            if let Some(r) = tables.answer(f, inputs) {
                return r;
            }
            if let Some(r) = externs::file(f, inputs, &program_dir) {
                return r;
            }
            if let Some(r) = externs::env_var(f, inputs) {
                return r;
            }
            let plus: Vec<bool> = f.args.iter().map(|b| b.input).collect();
            backend.query(&f.name, &plus, inputs)
        },
    );
    externs.preload_persisted(st.externs.clone());
    let mut base_extra = set_facts;
    base_extra.extend(stack::output_facts(&read_outputs));
    if let Some(m) = &manifest {
        base_extra.extend(m.facts());
    }
    let discovered = backend.discover(world_types(lowered.as_ref()).as_ref())?;
    let scope = catalog_scope(&program, &base_extra, &discovered, &st);
    backend.load_schema(scope.as_ref())?;
    backend.check_types(&program, &providers)?;
    if let Some(l) = &lowered {
        backend.check_configuration(&l.program, &l.extern_fns, |p| {
            p == dform_core::syntax::resolve::ENV_VAR
                || p.starts_with("file.")
                || tables::describe(p).is_some()
        })?;
        secrets::check(l, backend.schema(), &secret_outputs)?;
        refine::check(&l.program, backend.schema())?;
    }
    let secret_accounts = lowered
        .as_ref()
        .map(|l| secrets::secret_expected_accounts(l, backend.schema(), &secret_outputs))
        .unwrap_or_default();
    base_extra.extend(backend.catalog(scope.as_ref())?);
    base_extra.extend(discovered);

    let evaluate = |st: &state::State| -> Result<(EvalResult, Vec<String>, engine::Resumable)> {
        let mut extra = base_extra.clone();
        extra.extend(backend.world_facts(st)?);
        let (mut res, mut violations, mut resumable) =
            externs.eval_resumable(&program, &extra, zset::POLICY_INPUTS)?;
        if backend.configure_from(&res.facts)? {
            extra = base_extra.clone();
            extra.extend(backend.world_facts(st)?);
            (res, violations, resumable) =
                externs.eval_resumable(&program, &extra, zset::POLICY_INPUTS)?;
        }
        backend.check_accounts(&res.facts, &secret_accounts)?;
        violations.extend(inputs::violations(&res.facts, &declared));
        Ok((res, violations, resumable))
    };
    let (mut res, mut violations, mut resumable) = evaluate(&st)?;
    let moves = st.apply_moves(&zset::Lifecycle::from_facts(&res.facts, backend.schema())?.moved);
    if !moves.is_empty() {
        (res, violations, resumable) = evaluate(&st)?;
    }
    let schema = backend.schema();
    let strict = cfg.unknowns == stack::Unknowns::Strict;
    let collisions = if !cfg.keys.is_empty() && !cfg.isolated {
        let keys: Vec<String> = cfg.keys.iter().map(|(k, _)| k.clone()).collect();
        lint::key_collisions(&res, schema, &keys, &deployment)
    } else {
        Vec::new()
    };
    if !strict {
        for c in &collisions {
            problems.push(Problem::top(Severity::Warning, c.text.clone()));
        }
    }

    // The policy pass (E §2.8): the plan's deformations go back to the
    // program as facts, and what it denies of them is denied. A plan that
    // cannot be made leaves the program's own evaluation.
    let policy = || -> Result<(EvalResult, Vec<String>)> {
        let resources = ir::compile_resources(res.facts.iter().cloned(), schema)?;
        let adopts = ir::compile_adopts(res.facts.iter())?;
        let lifecycle = zset::Lifecycle::from_facts(&res.facts, schema)?;
        let mut plan = backend.plan(&resources, &adopts, &lifecycle, &st)?;
        let docs = resources
            .iter()
            .map(|r| ((r.addr.typ.clone(), r.addr.name.clone()), r.attrs.clone()))
            .collect();
        let sections = stuck::sections(&res.stuck, &res.may_derive, &res.facts, &docs, schema);
        executor::hold_deposed(&mut plan, &resources, &sections);
        let observed = backend.observe(&st)?;
        let before = observed
            .iter()
            .map(|(a, d)| (a.clone(), Some(d.clone())))
            .collect();
        let mut facts = zset::deformation_facts(
            plan.actions.iter().filter_map(|a| {
                let held = plan_print::waits_on(a, &sections).is_some();
                Some((zset::deformation_kind(&a.kind, held)?, &a.addr))
            }),
            &before,
            &observed,
        );
        facts.extend(
            res.may_derive
                .iter()
                .filter(|m| m.head.pred == "want")
                .map(|m| m.fact()),
        );
        let (again, all) = match resumable.with_at(&facts, None)? {
            (r, v) if externs.settle(&r.facts)? => (r, v),
            _ => {
                let mut extra = base_extra.clone();
                extra.extend(backend.world_facts(&st)?);
                extra.extend(facts);
                externs.eval_at(&program, &extra, None)?
            }
        };
        let denies = all
            .into_iter()
            .filter(|v| !violations.contains(v))
            .collect();
        Ok((again, denies))
    };
    let (mut res, denies) = match policy() {
        Ok(p) => p,
        Err(e) => {
            problems.extend(of_error(&e));
            (res, Vec::new())
        }
    };
    violations.extend(denies);
    if strict {
        lint::deny_collisions(&mut res, &collisions);
    }
    let redact = Redactor::new(&res.facts, schema);
    // A violation no deny fact or constraint accounts for (an input of the
    // wrong type): at the top of the stack's file.
    let constraints: BTreeSet<&str> = program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Constraint(c) => Some(c.message.as_str()),
            _ => None,
        })
        .collect();
    let denied: BTreeSet<String> = res
        .facts
        .iter()
        .filter(|a| a.pred == "deny")
        .filter_map(policy_text)
        .collect();
    for v in &violations {
        if !constraints.contains(v.as_str()) && !denied.contains(v) {
            problems.push(Problem::top(Severity::Error, redact.text(v)));
        }
    }
    let violated: Vec<String> = violations
        .into_iter()
        .filter(|v| constraints.contains(v.as_str()))
        .collect();
    let lowered_program = transform::lower(&program).ok().map(|l| l.program);
    for s in &program.statements {
        if let Stmt::Constraint(c) = s
            && violated.contains(&c.message)
        {
            problems.push(Problem {
                severity: Severity::Error,
                message: format!("constraint violated: {}", c.message),
                at: Where::Span(c.span),
                related: Vec::new(),
            });
        }
    }
    Ok(Evaluated {
        deployment: instance.describe(),
        providers,
        files: BTreeMap::new(),
        schema: schema.clone(),
        res,
        program,
        lowered: lowered_program,
        redact,
        index: OnceCell::new(),
    })
}

/// A `deny` or `warn` fact as the evaluator words its violation (`engine`'s
/// `format_policy_fact`).
fn policy_text(a: &Atom) -> Option<String> {
    let Some(Term::Val(Value::Str(msg))) = a.args.first() else {
        return None;
    };
    match a.args.get(1) {
        None => Some(msg.clone()),
        Some(Term::Val(ctx)) => Some(format!(
            "{msg} ctx={}",
            serde_json::to_string(&engine::value_to_json(ctx)).ok()?
        )),
        Some(_) => None,
    }
}

/// Every `deny` and `warn` fact, at the rule that derived it, the
/// contributions it reads as related information.
fn policy_problems(e: &Evaluated) -> Vec<Problem> {
    let mut out = Vec::new();
    for a in &e.res.facts {
        let severity = match a.pred.as_str() {
            "deny" => Severity::Error,
            "warn" => Severity::Warning,
            _ => continue,
        };
        let Some(text) = policy_text(a) else { continue };
        let Some(id) = e.res.circuit.fact_id(&engine::circuit_fact(a)) else {
            continue;
        };
        let (at, related) = provenance(e, id);
        out.push(Problem {
            severity,
            message: e.redact.text(&text),
            at: at.unwrap_or(Where::Top),
            related,
        });
    }
    out
}

/// Where fact `id` comes from: the nearest rule (or stated fact) that is
/// written somewhere, and the contributions of the attributes below it,
/// each with its rank.
pub fn provenance(e: &Evaluated, id: NodeId) -> (Option<Where>, Vec<(Where, String)>) {
    let c = &e.res.circuit;
    let mut at = None;
    let mut related = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = std::collections::VecDeque::from([(id, 0usize)]);
    while let Some((n, depth)) = queue.pop_front() {
        if depth > 8 || !seen.insert(n) || seen.len() > 400 {
            continue;
        }
        let View::Fact { fact, alts, .. } = c.view(n) else {
            continue;
        };
        let Some(&alt) = alts.first() else { continue };
        let View::Times { children, .. } = c.view(alt) else {
            continue;
        };
        let mut sigma = false;
        for ch in children {
            match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => {
                    sigma = id.starts_with('Σ');
                    if at.is_none()
                        && let Some(s) = e.rule_span(id)
                    {
                        at = Some(Where::Span(s));
                    }
                }
                View::Leaf(Leaf::Base { span }) if at.is_none() => {
                    at = Some(Where::Place(span.clone()));
                }
                _ => {}
            }
        }
        for ch in children {
            if let View::Fact { fact: f, .. } = c.view(*ch) {
                if sigma && let Some(w) = written(e, *ch) {
                    let rank = match f.args.get(4) {
                        Some(Value::Str(r)) => r.clone(),
                        _ => "?".into(),
                    };
                    let what = format!(
                        "contribution to {}.{} .{} at rank {rank}",
                        text_of(fact.args.first()),
                        text_of(fact.args.get(1)),
                        text_of(fact.args.get(2)),
                    );
                    if !related.iter().any(|(x, _)| *x == w) {
                        related.push((w, what));
                    }
                }
                queue.push_back((*ch, depth + 1));
            }
        }
    }
    (at, related)
}

/// Where the firings of fact `id` are written: its first rule or stated
/// fact that has a place.
pub fn written(e: &Evaluated, id: NodeId) -> Option<Where> {
    let c = &e.res.circuit;
    let View::Fact { alts, .. } = c.view(id) else {
        return None;
    };
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        for ch in children {
            match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => {
                    if let Some(s) = e.rule_span(id) {
                        return Some(Where::Span(s));
                    }
                }
                View::Leaf(Leaf::Base { span }) => return Some(Where::Place(span.clone())),
                _ => {}
            }
        }
    }
    None
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => dform_core::partition::fmt_value(v),
        None => String::new(),
    }
}

/// A key value as `--set` reads it.
fn value_of(raw: &str) -> Value {
    if raw == "true" {
        Value::Bool(true)
    } else if raw == "false" {
        Value::Bool(false)
    } else if let Ok(i) = raw.parse::<i64>() {
        Value::Int(i)
    } else {
        Value::Str(raw.to_string())
    }
}

// What follows mirrors the command line's helpers of the same names
// (src/cli.rs), which are private to it.

fn with_default_unknowns(program: &mut Program, m: &Manifest) {
    let Some(u) = &m.defaults.unknowns else {
        return;
    };
    for s in &mut program.statements {
        if let Stmt::Stack(c) = s
            && !c.config.iter().any(|(k, _, _)| k == "unknowns")
        {
            c.config
                .push(("unknowns".into(), Term::Val(Value::Str(u.clone())), c.span));
        }
    }
}

fn with_manifest(cfg: &mut stack::Stack, m: &Manifest, file: &Path) {
    for p in &mut cfg.providers {
        if !p.contains('/')
            && let Some(src) = m.provider_source(p)
        {
            *p = src;
        }
    }
    let name = cfg.name.clone().unwrap_or_else(|| state::stack_name(file));
    if cfg.backend.is_none() {
        cfg.backend = m.backend(&name);
    }
}

fn input_fact_keys(program: &Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Fact(a) if a.pred == "input" => match a.args.first() {
                Some(Term::Val(Value::Str(k))) => Some(k.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn provider_configs(program: &Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Fact(a) => Some(a),
            Stmt::Rule(r) => Some(&r.head),
            _ => None,
        })
        .filter(|a| a.pred == "provider_config")
        .filter_map(|a| match a.args.first() {
            Some(Term::Val(Value::Str(n))) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn world_types(lowered: Option<&transform::Lowered>) -> Option<BTreeSet<String>> {
    use dform_core::ast::Lit;
    let mut out = BTreeSet::new();
    for st in &lowered?.program.statements {
        let body = match st {
            Stmt::Rule(r) => &r.body,
            Stmt::Constraint(c) => &c.body,
            _ => continue,
        };
        for l in body {
            let (Lit::Pos(a) | Lit::Not(a)) = l else {
                continue;
            };
            if !plugin::providers::INVENTORY
                .iter()
                .any(|(p, _)| *p == a.pred)
            {
                continue;
            }
            match a.args.first() {
                Some(Term::Val(Value::Str(t))) => {
                    out.insert(t.clone());
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

fn catalog_scope(
    program: &Program,
    given: &[Atom],
    discovered: &[Atom],
    st: &state::State,
) -> Option<BTreeSet<String>> {
    let lowered = transform::lower(program).ok()?;
    let facts: Vec<Atom> = given.iter().chain(discovered).cloned().collect();
    let mut named = dform_core::schema::named_types(&lowered.program, &facts)?;
    named.extend(
        st.resources
            .keys()
            .chain(st.deposed.keys())
            .chain(st.uncertain.keys())
            .filter_map(|k| state::parse_key(k).map(|a| a.typ)),
    );
    Some(named)
}

/// The key values and scenarios a stack offers: each enum value of each
/// key input, and each scenario's name.
pub fn choices(file: &Path, keys: &[String], read: Reader) -> Result<(Vec<String>, Vec<String>)> {
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
    let scenarios = scenario::names(&program)?;
    Ok((values, scenarios))
}
