//! Definite facts survive every resolution of the unknowns (E §4.2, the
//! claim F §5 item 3 calls "still a proof on paper").
//!
//! Each case is a stratified program from a small grammar of rule shapes
//! (`Case`, printed as source) over a fixed schema whose computed paths
//! are fresh, open and secret (`SCHEMA`), and a stream of choices that
//! resolves its nulls. The case is planned on an empty world with the mock
//! linked in (the direct backend). Then every null the plan carries gets a
//! value of its class: a fresh id distinct from every other constant (the
//! Unique Name Assumption), a random constant of the path's type for an
//! open one, and for a secret a value the world keeps (the evaluator never
//! sees it, E §2.2). Those values go into the mock's world, the addresses
//! into state, and the program is planned again: refresh and round 0
//! resolve the nulls (E Rule 4), the path an apply boundary takes. The
//! properties, between the two plans:
//!
//! 1. every deformation the first plan reports definite is derived again
//!    with the same document, its nulls substituted;
//! 2. a deny that fires after resolution fired before, or was reported
//!    undetermined (or may derive); and one that fired still fires;
//! 3. every resource the second plan wants was wanted before or is an
//!    instance of a pending group the first plan reported;
//! 4. a negation the first plan decided (a `not` some derived fact used)
//!    does not flip, and every fact of the program's own predicates
//!    survives, substituted.
//!
//! The second plan is a fresh evaluation over the refreshed world, as the
//! executor's boundary is (`cli` evaluates again after every tick); the
//! engine has no incremental resolution to drive instead.
//!
//! The grammar includes chains of positive reads over stuck heads, whose
//! heads may derive (F DR-2 revised's last clause): a negation or an
//! aggregate group over one is undetermined. The shrunk failures that
//! found Rule 3 missing that case are regressions at the end of this file.
//!
//! `PROPTEST_CASES` sets the case count (CI: the default below; nightly:
//! 10^4). The planted-bug tests (`hooks`) check that a broken Rule 3 is
//! caught within the default count.

use anyhow::{Result, anyhow};
use dform_core::ast::{Atom, Term};
use dform_core::circuit::Leaf;
use dform_core::engine::{self, EvalResult};
use dform_core::hooks::{self, Rule3};
use dform_core::ir::{self, Address, Resource};
use dform_core::partition::{fmt_atom, fmt_value};
use dform_core::plan_print::{self, Report};
use dform_core::plugin::{Config, Providers};
use dform_core::provider::json_to_value;
use dform_core::state::State;
use dform_core::stuck;
use dform_core::value::{Value, null_label};
use dform_core::zset::Lifecycle;
use dform_mock::Linked;
use proptest::prelude::*;
use proptest::test_runner::{
    Config as RunConfig, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner,
};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// Cases per property when `PROPTEST_CASES` is not set: what runs in CI.
const CASES: u32 = 1024;

/// The schema: `pt.src` has a computed path of every class and type the
/// grammar reads; `pt.mid` and `pt.dst` have fresh ids, and `pt.dst` a
/// sensitive path a secret may be written to.
const SCHEMA: &str = r#"
type_provider(pt.src, "fakecloud")
type_provider(pt.mid, "fakecloud")
type_provider(pt.dst, "fakecloud")
type_attr(pt.src, "id", "string", ["computed", "id"])
type_attr(pt.src, "endpoint", "string", ["computed"])
type_attr(pt.src, "zones", "list", ["computed"])
type_attr(pt.src, "size", "int", ["computed"])
type_attr(pt.src, "token", "string", ["computed", "sensitive"])
type_attr(pt.mid, "id", "string", ["computed", "id"])
type_attr(pt.mid, "endpoint", "string", ["computed"])
type_attr(pt.dst, "id", "string", ["computed", "id"])
type_attr(pt.dst, "tok", "string", ["sensitive"])
"#;

/// The constants programs are written with; an open null resolves to one
/// of them, so a comparison against it can go either way.
const POOL: [&str; 3] = ["a", "b", "c"];

// ---------------------------------------------------------------------------
// The grammar. Indices are raw bytes, taken modulo what the program has when
// it is printed, so every case prints and shrinking stays in the grammar.

#[derive(Clone, Copy, Debug, PartialEq)]
enum Arg {
    /// The rule's variable (bound by its generator).
    X,
    C(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmp {
    Lt,
    Ge,
}

/// The literal that binds `x`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gen {
    /// `b(x)`: a base fact.
    Base,
    /// `x in sS.zones`: member over an open list (Rule 2).
    Zones(u8),
    /// `x in pt.mid`: a want read (a stuck group makes it may-derive).
    Mid,
    /// `pJ(x)`: an earlier predicate.
    Pred(u8),
    /// `x in pt.dst` (a deny's), else as `Mid`.
    Dst,
}

/// A literal that tests.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Guard {
    /// `pJ(arg)`
    Pred(u8, Arg),
    /// `not pJ(arg)`
    NotPred(u8, Arg),
    /// `not "mK-c" in pt.mid`: negation over a want group.
    NotMid(u8, u8),
    /// `sS.endpoint == arg`: open, Rule 2.
    EqEp(u8, Arg),
    /// `sS.endpoint != arg`
    NeEp(u8, Arg),
    /// `sS.id == arg`: fresh, decided under UNA.
    EqId(u8, Arg),
    /// `sS.id != arg`
    NeId(u8, Arg),
    /// `sS.size < n`, `sS.size >= n`
    Size(u8, Cmp, u8),
    /// `arg in sS.zones`
    In(u8, Arg),
    /// `arg not in sS.zones`
    NotIn(u8, Arg),
    /// `"p-{sS.endpoint}" != "p-c"`: a builtin over a null.
    Fmt(u8, u8),
    /// `cK(n), n < m` over a count, `cK(l), "c" in l` over a set.
    Agg(u8, Cmp, u8),
}

#[derive(Clone, Debug, PartialEq)]
enum PredDef {
    /// `pI(x) if bind, guards`
    Rule(Gen, Vec<Guard>),
    /// `pI(x) if x = sS.endpoint` / `sS.id`: facts that carry a null.
    Carry(u8, bool),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AggKind {
    Count,
    Set,
}

/// A definition: a predicate `pI` or an aggregate `cI`, reading only the
/// definitions before it.
#[derive(Clone, Debug, PartialEq)]
enum Def {
    Pred(PredDef),
    Agg(AggDef),
}

#[derive(Clone, Debug, PartialEq)]
struct AggDef {
    kind: AggKind,
    bind: Gen,
    guards: Vec<Guard>,
}

/// A `pt.mid` or `pt.dst` resource rule: one static name, or one per `x`.
#[derive(Clone, Debug, PartialEq)]
struct ResDef {
    bind: Option<Gen>,
    guards: Vec<Guard>,
    /// Which of the tier's fields it sets (bits), and from which source.
    fields: u8,
    src: u8,
}

#[derive(Clone, Debug, PartialEq)]
struct DenyDef {
    bind: Option<Gen>,
    guards: Vec<Guard>,
}

#[derive(Clone, PartialEq)]
struct Program {
    base: Vec<u8>,
    srcs: u8,
    defs: Vec<Def>,
    mids: Vec<ResDef>,
    dsts: Vec<ResDef>,
    denies: Vec<DenyDef>,
}

/// A program and the choices that resolve its nulls.
#[derive(Clone)]
struct Case {
    program: Program,
    choices: Vec<u8>,
}

impl fmt::Debug for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\n{}\nchoices: {:?}\n", self.program, self.choices)
    }
}

/// What a body may read where it stands.
#[derive(Clone, Copy)]
struct Scope {
    /// The definitions it may read: `Program::defs[..defs]`.
    defs: usize,
    mid: bool,
    dst: bool,
    x: bool,
}

struct Printer<'a> {
    p: &'a Program,
}

impl Printer<'_> {
    /// The predicates among the first `n` definitions.
    fn preds(&self, n: usize) -> Vec<usize> {
        (0..n)
            .filter(|&i| matches!(self.p.defs[i], Def::Pred(_)))
            .collect()
    }

    /// The aggregates among the first `n` definitions, with their kinds.
    fn aggs(&self, n: usize) -> Vec<(usize, AggKind)> {
        (0..n)
            .filter_map(|i| match &self.p.defs[i] {
                Def::Agg(a) => Some((i, a.kind)),
                Def::Pred(_) => None,
            })
            .collect()
    }

    /// `pJ`, the `j`th predicate modulo those in scope.
    fn pred(&self, j: u8, sc: Scope) -> Option<String> {
        let ps = self.preds(sc.defs);
        (!ps.is_empty()).then(|| format!("p{}", ps[j as usize % ps.len()]))
    }

    fn src(&self, s: u8) -> String {
        format!("s{}", s % self.p.srcs.max(1))
    }

    fn arg(&self, a: Arg, sc: Scope) -> String {
        match a {
            Arg::X if sc.x => "x".into(),
            Arg::X => format!("\"{}\"", POOL[0]),
            Arg::C(c) => format!("\"{}\"", POOL[c as usize % POOL.len()]),
        }
    }

    fn bind(&self, g: Gen, sc: Scope) -> String {
        match g {
            Gen::Zones(s) => format!("x in {}.zones", self.src(s)),
            Gen::Dst if sc.dst => "x in pt.dst".into(),
            Gen::Mid | Gen::Dst if sc.mid => "x in pt.mid".into(),
            Gen::Pred(j) => match self.pred(j, sc) {
                Some(p) => format!("{p}(x)"),
                None => "b(x)".into(),
            },
            _ => "b(x)".into(),
        }
    }

    fn cmp(c: Cmp) -> &'static str {
        match c {
            Cmp::Lt => "<",
            Cmp::Ge => ">=",
        }
    }

    /// The guard as a literal, or `None` where it cannot stand.
    fn guard(&self, g: Guard, sc: Scope, i: usize) -> Option<String> {
        Some(match g {
            Guard::Pred(j, a) => format!("{}({})", self.pred(j, sc)?, self.arg(a, sc)),
            Guard::NotPred(j, a) => format!("not {}({})", self.pred(j, sc)?, self.arg(a, sc)),
            Guard::NotMid(k, c) if sc.mid => format!(
                "not \"m{}-{}\" in pt.mid",
                k as usize % self.p.mids.len(),
                POOL[c as usize % POOL.len()]
            ),
            Guard::EqEp(s, a) => format!("{}.endpoint == {}", self.src(s), self.arg(a, sc)),
            Guard::NeEp(s, a) => format!("{}.endpoint != {}", self.src(s), self.arg(a, sc)),
            Guard::EqId(s, a) => format!("{}.id == {}", self.src(s), self.arg(a, sc)),
            Guard::NeId(s, a) => format!("{}.id != {}", self.src(s), self.arg(a, sc)),
            Guard::Size(s, c, n) => format!("{}.size {} {}", self.src(s), Self::cmp(c), n % 4),
            Guard::In(s, a) => format!("{} in {}.zones", self.arg(a, sc), self.src(s)),
            Guard::NotIn(s, a) => format!("{} not in {}.zones", self.arg(a, sc), self.src(s)),
            Guard::Fmt(s, c) => format!(
                "\"p-{{{}.endpoint}}\" != \"p-{}\"",
                self.src(s),
                POOL[c as usize % POOL.len()]
            ),
            Guard::Agg(k, c, n) => {
                let aggs = self.aggs(sc.defs);
                if aggs.is_empty() {
                    return None;
                }
                let (k, kind) = aggs[k as usize % aggs.len()];
                match kind {
                    AggKind::Count => format!("c{k}(n{i}), n{i} {} {}", Self::cmp(c), n % 4),
                    AggKind::Set => {
                        format!("c{k}(l{i}), \"{}\" in l{i}", POOL[n as usize % POOL.len()])
                    }
                }
            }
            _ => return None,
        })
    }

    /// `bind, guard, ...`: the generator (when there is one) first.
    fn body(&self, bind: Option<Gen>, guards: &[Guard], sc: Scope) -> Vec<String> {
        let sc = Scope {
            x: bind.is_some(),
            ..sc
        };
        let mut out: Vec<String> = bind.map(|g| self.bind(g, sc)).into_iter().collect();
        for (i, g) in guards.iter().enumerate() {
            if let Some(l) = self.guard(*g, sc, i) {
                out.push(l);
            }
        }
        out
    }

    fn resource(
        &self,
        f: &mut fmt::Formatter<'_>,
        typ: &str,
        name: &str,
        r: &ResDef,
        sc: Scope,
    ) -> fmt::Result {
        let header = match r.bind {
            Some(_) => format!("\"{name}-{{x}}\""),
            None => name.to_string(),
        };
        writeln!(f, "resource {typ} {header} {{")?;
        let sc = Scope {
            x: r.bind.is_some(),
            ..sc
        };
        if let Some(g) = r.bind {
            writeln!(f, "  for {}", self.bind(g, sc))?;
        }
        let guards = self.body(None, &r.guards, sc);
        if !guards.is_empty() {
            writeln!(f, "  if {}", guards.join(", "))?;
        }
        let s = self.src(r.src);
        writeln!(f, "  src = {s}.id")?;
        if r.fields & 1 != 0 {
            writeln!(f, "  ep = {s}.endpoint")?;
        }
        if typ == "pt.dst" {
            if r.fields & 2 != 0 {
                writeln!(f, "  tok = {s}.token")?;
            }
            let statics: Vec<usize> = (0..self.p.mids.len())
                .filter(|&k| {
                    let m = &self.p.mids[k];
                    m.bind.is_none()
                })
                .collect();
            if r.fields & 4 != 0 && !statics.is_empty() {
                let k = statics[r.src as usize % statics.len()];
                writeln!(f, "  mid = m{k}.id")?;
            }
        }
        if r.bind.is_some() && r.fields & 8 != 0 {
            writeln!(f, "  v = x")?;
        }
        writeln!(f, "}}")
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pr = Printer { p: self };
        writeln!(f, "edition 2026\n\ndecl b/1")?;
        let mut base: Vec<usize> = self.base.iter().map(|c| *c as usize % POOL.len()).collect();
        base.sort();
        base.dedup();
        for c in base {
            writeln!(f, "b(\"{}\")", POOL[c])?;
        }
        for s in 0..self.srcs.max(1) {
            writeln!(f, "\nresource pt.src s{s} {{\n  label = \"s{s}\"\n}}")?;
        }
        // pt.mid reads only the sources; a definition pt.mid and the
        // definitions before it; pt.dst and the denies everything.
        let mid = !self.mids.is_empty();
        for (k, r) in self.mids.iter().enumerate() {
            writeln!(f)?;
            let sc = Scope {
                defs: 0,
                mid: false,
                dst: false,
                x: false,
            };
            pr.resource(f, "pt.mid", &format!("m{k}"), r, sc)?;
        }
        if !self.defs.is_empty() {
            writeln!(f)?;
        }
        for (i, d) in self.defs.iter().enumerate() {
            let sc = Scope {
                defs: i,
                mid,
                dst: false,
                x: true,
            };
            match d {
                Def::Pred(PredDef::Rule(g, guards)) => {
                    writeln!(f, "p{i}(x) if {}", pr.body(Some(*g), guards, sc).join(", "))?;
                }
                Def::Pred(PredDef::Carry(s, id)) => {
                    let path = if *id { "id" } else { "endpoint" };
                    writeln!(f, "p{i}(x) if x = {}.{path}", pr.src(*s))?;
                }
                Def::Agg(a) => {
                    let head = match a.kind {
                        AggKind::Count => "count",
                        AggKind::Set => "collect_set",
                    };
                    writeln!(
                        f,
                        "c{i}({head}(x)) if {}",
                        pr.body(Some(a.bind), &a.guards, sc).join(", ")
                    )?;
                }
            }
        }
        let all = Scope {
            defs: self.defs.len(),
            mid,
            dst: false,
            x: false,
        };
        for (k, r) in self.dsts.iter().enumerate() {
            writeln!(f)?;
            pr.resource(f, "pt.dst", &format!("d{k}"), r, all)?;
        }
        if !self.denies.is_empty() {
            writeln!(f)?;
        }
        let dst = !self.dsts.is_empty();
        for (k, d) in self.denies.iter().enumerate() {
            let sc = Scope { dst, ..all };
            let mut body = pr.body(d.bind, &d.guards, sc);
            if body.is_empty() {
                body.push("b(x)".into());
            }
            writeln!(f, "deny \"d{k}\" if {}", body.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// Strategies.

fn arg() -> impl Strategy<Value = Arg> {
    prop_oneof![3 => Just(Arg::X), 2 => any::<u8>().prop_map(Arg::C)]
}

fn cmp() -> impl Strategy<Value = Cmp> {
    prop_oneof![Just(Cmp::Lt), Just(Cmp::Ge)]
}

fn bind() -> impl Strategy<Value = Gen> {
    prop_oneof![
        Just(Gen::Base),
        any::<u8>().prop_map(Gen::Zones),
        Just(Gen::Mid),
        any::<u8>().prop_map(Gen::Pred),
        Just(Gen::Dst),
    ]
}

/// Weighted toward the reads Rule 3 decides: negations and aggregates.
fn guard() -> impl Strategy<Value = Guard> {
    let b = any::<u8>;
    prop_oneof![
        2 => (b(), arg()).prop_map(|(j, a)| Guard::Pred(j, a)),
        4 => (b(), arg()).prop_map(|(j, a)| Guard::NotPred(j, a)),
        2 => (b(), b()).prop_map(|(k, c)| Guard::NotMid(k, c)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::EqEp(s, a)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::NeEp(s, a)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::EqId(s, a)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::NeId(s, a)),
        1 => (b(), cmp(), b()).prop_map(|(s, c, n)| Guard::Size(s, c, n)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::In(s, a)),
        1 => (b(), arg()).prop_map(|(s, a)| Guard::NotIn(s, a)),
        1 => (b(), b()).prop_map(|(s, c)| Guard::Fmt(s, c)),
        4 => (b(), cmp(), b()).prop_map(|(k, c, n)| Guard::Agg(k, c, n)),
    ]
}

fn guards() -> impl Strategy<Value = Vec<Guard>> {
    prop::collection::vec(guard(), 0..=3)
}

fn pred_def() -> impl Strategy<Value = PredDef> {
    prop_oneof![
        4 => (bind(), guards()).prop_map(|(g, gs)| PredDef::Rule(g, gs)),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(s, id)| PredDef::Carry(s, id)),
    ]
}

/// An aggregate over an open list most often: the group Rule 3 leaves
/// undetermined.
fn agg_def() -> impl Strategy<Value = AggDef> {
    (
        prop_oneof![Just(AggKind::Count), Just(AggKind::Set)],
        prop_oneof![3 => any::<u8>().prop_map(Gen::Zones), 2 => bind()],
        guards(),
    )
        .prop_map(|(kind, bind, guards)| AggDef { kind, bind, guards })
}

fn def() -> impl Strategy<Value = Def> {
    prop_oneof![
        2 => pred_def().prop_map(Def::Pred),
        1 => agg_def().prop_map(Def::Agg),
    ]
}

fn res_def() -> impl Strategy<Value = ResDef> {
    (
        prop::option::weighted(0.6, bind()),
        guards(),
        any::<u8>(),
        any::<u8>(),
    )
        .prop_map(|(bind, guards, fields, src)| ResDef {
            bind,
            guards,
            fields,
            src,
        })
}

fn deny_def() -> impl Strategy<Value = DenyDef> {
    (prop::option::weighted(0.7, bind()), guards())
        .prop_map(|(bind, guards)| DenyDef { bind, guards })
}

fn program() -> impl Strategy<Value = Program> {
    (
        prop::collection::vec(any::<u8>(), 1..=3),
        1u8..=2,
        prop::collection::vec(def(), 0..=6),
        prop::collection::vec(res_def(), 0..=2),
        prop::collection::vec(res_def(), 0..=3),
        prop::collection::vec(deny_def(), 1..=3),
    )
        .prop_map(|(base, srcs, defs, mids, dsts, denies)| Program {
            base,
            srcs,
            defs,
            mids,
            dsts,
            denies,
        })
}

fn case() -> impl Strategy<Value = Case> {
    (program(), prop::collection::vec(any::<u8>(), 48))
        .prop_map(|(program, choices)| Case { program, choices })
}

// ---------------------------------------------------------------------------
// Planning, with the mock linked in.

/// A directory of this test process's own under the target directory.
fn scratch() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dform-proptest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("schema.df"), SCHEMA).unwrap();
        d
    })
}

/// This thread's world file (the properties may run side by side).
fn world_path() -> PathBuf {
    let t = format!("{:?}", std::thread::current().id());
    let t: String = t.chars().filter(char::is_ascii_digit).collect();
    scratch().join(format!("world-{t}.json"))
}

struct Planned {
    res: EvalResult,
    resources: Vec<Resource>,
    report: Report,
}

fn providers(world: &Path) -> Result<Providers> {
    let schema = scratch().join("schema.df");
    Providers::start(
        &Linked::direct(),
        &[schema.display().to_string()],
        &Config {
            world: world.to_path_buf(),
            inventory: scratch().join("no-inventory.json"),
            chaos: vec![],
            cache: None,
            ..Default::default()
        },
    )
}

/// The plan of `program` against the world at `world` and `state`: the
/// evaluation (refresh as facts, round 0 included), the documents, and the
/// report `dform plan` prints.
fn plan(program: &dform_core::ast::Program, world: &Path, state: &State) -> Result<Planned> {
    let backend = providers(world)?;
    let schema = backend.schema();
    let mut extra = backend.catalog(None)?;
    extra.extend(backend.world_facts(state)?);
    let (res, _violations) = engine::eval(program, &extra)?;
    let resources = ir::compile_resources(res.facts.iter().cloned(), schema)?;
    let adopts = ir::compile_adopts(res.facts.iter())?;
    let lifecycle = Lifecycle::from_facts(&res.facts, schema)?;
    let p = backend.plan(&resources, &adopts, &lifecycle, state)?;
    let docs = resources
        .iter()
        .map(|r| ((r.addr.typ.clone(), r.addr.name.clone()), r.attrs.clone()))
        .collect();
    let sections = stuck::sections(&res.stuck, &res.may_derive, &res.facts, &docs, schema);
    let report = plan_print::report(&plan_print::Input {
        plan: &p,
        res: &res,
        sections: &sections,
        program,
        schema,
        stack: "prop",
        show_noop: false,
        tick: 1,
        moved: &[],
        denies: &[],
    });
    Ok(Planned {
        res,
        resources,
        report,
    })
}

// ---------------------------------------------------------------------------
// Resolution.

/// Takes the case's choices in turn, round and round.
struct Choices<'a> {
    xs: &'a [u8],
    at: usize,
}

impl Choices<'_> {
    fn next(&mut self) -> usize {
        let v = self.xs.get(self.at % self.xs.len().max(1)).copied();
        self.at += 1;
        v.unwrap_or(0) as usize
    }
}

/// Every wanted address of `planned` created in a world: its computed
/// paths given values of their class, its identity in state. Returns the
/// world, the state, and the value per null label (secrets excepted).
fn resolve(planned: &Planned, choices: &[u8]) -> (Json, State, BTreeMap<String, Value>) {
    let mut ch = Choices { xs: choices, at: 0 };
    let mut world = serde_json::Map::new();
    let mut state = State::default();
    let mut by_label = BTreeMap::new();
    let computed: [(&str, &[&str]); 3] = [
        ("pt.src", &["id", "endpoint", "zones", "size", "token"]),
        ("pt.mid", &["id", "endpoint"]),
        ("pt.dst", &["id"]),
    ];
    let wanted =
        planned
            .res
            .facts
            .iter()
            .filter_map(|a| match (a.pred.as_str(), a.args.as_slice()) {
                ("want", [Term::Val(Value::Str(typ)), Term::Val(Value::Str(name))]) => {
                    Some(Address {
                        typ: typ.clone(),
                        name: name.clone(),
                    })
                }
                _ => None,
            });
    for (n, addr) in wanted.enumerate() {
        let Some((_, paths)) = computed.iter().find(|(t, _)| *t == addr.typ) else {
            continue;
        };
        let remote = format!("id-{n}");
        let mut doc = serde_json::Map::new();
        for &p in *paths {
            let v = match p {
                // Fresh: distinct from every other value (UNA).
                "id" => json!(remote),
                "endpoint" => json!(POOL[ch.next() % POOL.len()]),
                "zones" => {
                    let len = ch.next() % 4;
                    json!(
                        (0..len)
                            .map(|_| POOL[ch.next() % POOL.len()])
                            .collect::<Vec<_>>()
                    )
                }
                "size" => json!(ch.next() % 4),
                // Secret: the world has it; the evaluator never does.
                "token" => {
                    doc.insert(p.into(), json!("hunter2"));
                    continue;
                }
                _ => unreachable!(),
            };
            by_label.insert(null_label(&addr.typ, &addr.name, p), json_to_value(&v));
            doc.insert(p.into(), v);
        }
        world.insert(
            format!("{}::{remote}", addr.typ),
            json!({"typ": addr.typ, "name": remote, "attrs": {}, "computed": doc}),
        );
        state.set(addr, "fakecloud".into(), remote);
    }
    (json!({ "resources": world }), state, by_label)
}

/// `v` with every resolved null replaced by its value.
fn subst(v: &Value, by: &BTreeMap<String, Value>) -> Value {
    match v {
        Value::Null { label, .. } => by.get(label).cloned().unwrap_or_else(|| v.clone()),
        Value::List(xs) => Value::List(xs.iter().map(|x| subst(x, by)).collect()),
        Value::Obj(m) => Value::Obj(m.iter().map(|(k, x)| (k.clone(), subst(x, by))).collect()),
        other => other.clone(),
    }
}

fn subst_atom(a: &Atom, by: &BTreeMap<String, Value>) -> Atom {
    Atom {
        args: a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => Term::Val(subst(v, by)),
                other => other.clone(),
            })
            .collect(),
        ..a.clone()
    }
}

/// A printed pattern with every resolved null's spelling replaced.
fn subst_text(
    pattern: &str,
    nulls: &BTreeMap<String, Value>,
    by: &BTreeMap<String, Value>,
) -> String {
    let mut out = pattern.to_string();
    for (label, null) in nulls {
        if let Some(v) = by.get(label) {
            out = out.replace(&fmt_value(null), &fmt_value(v));
        }
    }
    out
}

fn collect_nulls(v: &Value, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Null { label, .. } => {
            out.insert(label.clone(), v.clone());
        }
        Value::List(xs) => xs.iter().for_each(|x| collect_nulls(x, out)),
        Value::Obj(m) => m.values().for_each(|x| collect_nulls(x, out)),
        _ => {}
    }
}

/// The program's own relations, and the core ones the properties name.
fn watched(a: &Atom) -> bool {
    let p = a.pred.as_str();
    matches!(p, "want" | "deny")
        || (p.len() >= 2
            && (p.starts_with('p') || p.starts_with('c'))
            && p[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn deny_messages(res: &EvalResult) -> BTreeSet<String> {
    res.facts
        .iter()
        .filter(|a| a.pred == "deny")
        .filter_map(|a| match a.args.first() {
            Some(Term::Val(Value::Str(m))) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn doc_of<'a>(rs: &'a [Resource], addr: &Address) -> Option<&'a Value> {
    rs.iter().find(|r| &r.addr == addr).map(|r| &r.attrs)
}

fn fail(what: impl Into<String>) -> TestCaseError {
    TestCaseError::fail(what.into())
}

/// Plan, resolve, plan again, and check the four properties.
fn check(case: &Case) -> Result<(), TestCaseError> {
    check_source(&case.program.to_string(), &case.choices)
}

fn check_source(src: &str, choices: &[u8]) -> Result<(), TestCaseError> {
    let program = dform_core::parser::parse_file("prop.df", src).map_err(|e| {
        fail(format!(
            "the grammar printed a program that does not parse: {e:#}"
        ))
    })?;
    let world = world_path();
    let _ = std::fs::remove_file(&world);
    let first = match plan(&program, &world, &State::default()) {
        Ok(p) => p,
        Err(e) => return Err(fail(format!("plan on an empty world: {e:#}"))),
    };
    let (w, state, by) = resolve(&first, choices);
    std::fs::write(&world, serde_json::to_vec(&w).unwrap()).unwrap();
    let second = plan(&program, &world, &state)
        .map_err(|e| fail(format!("plan after resolution: {e:#}")))?;
    let _ = std::fs::remove_file(&world);
    properties(&first, &second, &by).map_err(|e| {
        let resolved: Vec<String> = by
            .iter()
            .map(|(l, v)| format!("?{l} := {}", fmt_value(v)))
            .collect();
        fail(format!("{e}\nresolved: {}", resolved.join(", ")))
    })
}

fn properties(first: &Planned, second: &Planned, by: &BTreeMap<String, Value>) -> Result<()> {
    // 1. Definite deformations: the same document, nulls substituted.
    for d in &first.report.definite {
        let before = doc_of(&first.resources, &d.addr).ok_or_else(|| {
            anyhow!(
                "{}.{} is definite without a document",
                d.addr.typ,
                d.addr.name
            )
        })?;
        let after = doc_of(&second.resources, &d.addr).ok_or_else(|| {
            anyhow!(
                "property 1: {}.{} was definite and is not derived after resolution",
                d.addr.typ,
                d.addr.name
            )
        })?;
        let want = subst(before, by);
        if *after != want {
            return Err(anyhow!(
                "property 1: {}.{} was definite; its document changed beyond substitution:\n  before {}\n  expected {}\n  after {}",
                d.addr.typ,
                d.addr.name,
                fmt_value(before),
                fmt_value(&want),
                fmt_value(after)
            ));
        }
    }

    // 2. Denies: one that fires after resolution fired before or was
    // reported undetermined; one that fired still fires.
    let (fired1, fired2) = (deny_messages(&first.res), deny_messages(&second.res));
    let reported: BTreeSet<&str> = first
        .report
        .policies
        .iter()
        .map(|p| p.message.as_str())
        .collect();
    for m in fired2.difference(&fired1) {
        if !reported.contains(m.as_str()) {
            return Err(anyhow!(
                "property 2: deny {m:?} was reported satisfied (neither firing nor undetermined) and fires after resolution"
            ));
        }
    }
    if let Some(m) = fired1.difference(&fired2).next() {
        return Err(anyhow!(
            "property 2: deny {m:?} fired and no longer fires after resolution"
        ));
    }

    // 3. Wants: every new one an instance of a reported group (none is
    // lost: property 4).
    let wants = |r: &EvalResult| -> BTreeSet<Atom> {
        r.facts
            .iter()
            .filter(|a| a.pred == "want")
            .cloned()
            .collect()
    };
    let (w1, w2) = (wants(&first.res), wants(&second.res));
    let groups: Vec<&Atom> = first
        .res
        .stuck
        .iter()
        .map(|s| &s.head)
        .chain(first.res.may_derive.iter().map(|m| &m.head))
        .filter(|h| h.pred == "want")
        .collect();
    for w in w2.difference(&w1) {
        if !groups.iter().any(|g| stuck::patterns_unify(g, w)) {
            return Err(anyhow!(
                "property 3: {} is wanted after resolution and was in no pending group (groups: {})",
                fmt_atom(w),
                groups
                    .iter()
                    .map(|g| fmt_atom(g))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // 4. Negations the first plan decided do not flip; the program's own
    // facts survive.
    let printed2: BTreeSet<String> = second.res.facts.iter().map(fmt_atom).collect();
    let mut nulls = BTreeMap::new();
    for a in &first.res.facts {
        for t in &a.args {
            if let Term::Val(v) = t {
                collect_nulls(v, &mut nulls);
            }
        }
    }
    let mut decided: BTreeMap<String, String> = BTreeMap::new();
    for a in first.res.facts.iter().filter(|a| watched(a)) {
        for alt in first.res.circuit.why(&engine::circuit_fact(a)) {
            for leaf in alt {
                // The prelude's `not resolved(L)` is the null itself:
                // resolution is what flips it (Rule 4).
                if let Leaf::Absent { pattern } = leaf
                    && !pattern.starts_with("resolved(")
                {
                    decided.entry(pattern).or_insert_with(|| fmt_atom(a));
                }
            }
        }
    }
    for (pattern, used_by) in &decided {
        let now = subst_text(pattern, &nulls, by);
        if printed2.contains(&now) {
            return Err(anyhow!(
                "property 4: not {pattern} was decided true (used by {used_by}) and {now} is derived after resolution"
            ));
        }
    }
    for a in first.res.facts.iter().filter(|a| watched(a)) {
        let now = subst_atom(a, by);
        if !second.res.facts.contains(&now) {
            return Err(anyhow!(
                "property 4: {} was derived and {} is not after resolution",
                fmt_atom(a),
                fmt_atom(&now)
            ));
        }
    }
    SEEN.with_borrow_mut(|seen| {
        seen.cases += 1;
        let carries = |a: &Address| {
            doc_of(&first.resources, a).is_some_and(|d| {
                let mut n = BTreeMap::new();
                collect_nulls(d, &mut n);
                !n.is_empty()
            })
        };
        seen.definite_with_nulls += first
            .report
            .definite
            .iter()
            .filter(|d| carries(&d.addr))
            .count();
        seen.negations += decided.len();
        seen.groups += first.report.groups.len();
        seen.new_wants += w2.difference(&w1).count();
        seen.undetermined += first.report.policies.len();
        seen.denies_after += fired2.difference(&fired1).count();
    });
    Ok(())
}

/// What the checked cases exercised, so a vacuous run shows.
#[derive(Debug, Default)]
struct Seen {
    cases: usize,
    /// Definite deformations whose document carried a null.
    definite_with_nulls: usize,
    /// Negations decided true that a derived fact used.
    negations: usize,
    groups: usize,
    /// Resources wanted after resolution only.
    new_wants: usize,
    /// Denies reported undetermined or may-derive.
    undetermined: usize,
    /// Denies that fire after resolution only.
    denies_after: usize,
}

thread_local! {
    static SEEN: std::cell::RefCell<Seen> = std::cell::RefCell::new(Seen::default());
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CASES)
}

fn config(cases: u32, max_shrink_iters: u32) -> RunConfig {
    RunConfig {
        cases,
        max_shrink_iters,
        // A failure prints its program; nothing is written to the tree.
        failure_persistence: None,
        ..RunConfig::default()
    }
}

/// Run the property over `cases` cases; a failure panics with its shrunk
/// program.
fn run(cases: u32) {
    let mut runner = TestRunner::new(config(cases, 4096));
    match runner.run(&case(), |c| check(&c)) {
        Ok(()) => SEEN.with_borrow(|seen| eprintln!("{seen:?}")),
        Err(TestError::Fail(why, minimal)) => panic!("{why}\nminimal failing input: {minimal:?}"),
        Err(e) => panic!("{e}"),
    }
}

/// The whole grammar: chains of positive reads over stuck heads included,
/// whose heads may derive (F DR-2 revised's last clause) and so leave a
/// negation or an aggregate group over them undetermined.
#[test]
fn definite_facts_survive_every_resolution_of_the_unknowns() {
    run(cases());
}

/// Run the property with `r` planted, at the CI case count, from a fixed
/// seed (so whether it is caught does not depend on the run): it must fail.
fn caught(r: Rule3) -> String {
    let _planted = hooks::plant(r);
    let rng = TestRng::deterministic_rng(RngAlgorithm::ChaCha);
    let mut runner = TestRunner::new_with_rng(config(CASES, 256), rng);
    match runner.run(&case(), |c| check(&c)) {
        Err(TestError::Fail(why, minimal)) => format!("{why}\nminimal failing input: {minimal:?}"),
        Err(e) => panic!("Rule 3 with {r:?} planted: {e}"),
        Ok(()) => {
            panic!("Rule 3 with {r:?} planted: {CASES} cases passed; the property did not catch it")
        }
    }
}

/// Deciding `not p(t)` against the current `p` while a stuck head of `p`
/// unifies with it is caught.
#[test]
fn a_negation_decided_past_a_stuck_head_is_caught() {
    eprintln!("{}", caught(Rule3::Negation));
}

/// A reader of an undetermined aggregate that is not itself undetermined
/// (F §1.2 item 1) is caught.
#[test]
fn a_reader_of_an_undetermined_aggregate_is_caught() {
    eprintln!("{}", caught(Rule3::Reader));
}

// ---------------------------------------------------------------------------
// Regressions: shrunk failures of the whole grammar before Rule 3 counted
// may-derive heads, one per way the hole showed. Ticket "Rule 3 decides
// negations and aggregates over may-derive predicates too early".

/// Zeros: every open string resolves to "a", every list to [], every int
/// to 0.
const ZEROS: [u8; 1] = [0];

#[track_caller]
fn holds(src: &str, choices: &[u8]) {
    if let Err(e) = check_source(src, choices) {
        panic!("{e}\n{src}");
    }
}

/// `not p1("a")` is decided while `p1` only reads the stuck `p0`: the deny
/// fires, and after `s0.size` resolves `p1("a")` holds and it does not.
#[test]
fn regression_a_negation_over_a_predicate_that_may_derive() {
    holds(
        r#"edition 2026

b("a")

resource pt.src s0 {
  label = "s0"
}

p0(x) if b(x), s0.size >= 0
p1(x) if p0(x)
deny "d0" if b(x), not p1(x)
"#,
        &ZEROS,
    );
}

/// `c1` counts `p0`, which reads `want(pt.mid, _)` while `m1` is stuck: the
/// count is decided at 1 and is 2 after resolution.
#[test]
fn regression_an_aggregate_over_a_predicate_that_may_derive() {
    holds(
        r#"edition 2026

resource pt.src s0 {
  label = "s0"
}

resource pt.mid m0 {
  src = s0.id
}

resource pt.mid m1 {
  if s0.size >= 0
  src = s0.id
}

p0(x) if x in pt.mid
c1(count(x)) if p0(x)
"#,
        &ZEROS,
    );
}

/// `d0.mid` refers to `m0`, whose want is stuck: the reference joins no
/// `attr` row yet, so the field would be left out, and `d0` (which waits
/// on nothing else: its source is `s1`) was reported definite with a
/// document that gains `mid` after resolution. The `mid` group may derive,
/// so `d0` is pending until `m0`'s want is decided.
#[test]
fn regression_a_reference_to_a_stuck_resource_is_definite() {
    let src = r#"edition 2026

resource pt.src s0 {
  label = "s0"
}

resource pt.src s1 {
  label = "s1"
}

resource pt.mid m0 {
  if s0.size >= 0
  src = s0.id
}

resource pt.dst d0 {
  src = s1.id
  mid = m0.id
}
"#;
    holds(src, &ZEROS);
    let program = dform_core::parser::parse_file("prop.df", src).unwrap();
    let world = world_path();
    let _ = std::fs::remove_file(&world);
    let planned = plan(&program, &world, &State::default()).unwrap();
    let d0 = Address {
        typ: "pt.dst".into(),
        name: "d0".into(),
    };
    let pending: Vec<&Address> = planned
        .report
        .pending
        .iter()
        .flat_map(|b| &b.deformations)
        .map(|d| &d.addr)
        .collect();
    assert!(
        pending.contains(&&d0),
        "pt.dst.d0 is not pending: {:#}",
        planned.report.json()
    );
}
