//! The tool's three printers (R-63); every command prints through one:
//! a result set ([`table`]: rows under a header line, `query`, `output`,
//! the listings), a derivation ([`tree`]: what `why` prints), or a report
//! (this module: `plan`, `apply` and `diff`, which compose the two).
//!
//! The plan report (proposal E §2.7, §7.4; F DR-2 revised): one report
//! built from an evaluation and the provider's plan, rendered as text for
//! `plan` and every `apply` tick, or as one JSON document for `--json`.
//!
//! Grouped by tick (R-79), the only grouping: tick 1, what applies now;
//! each later tick with the values it waits on; `later`, the rules that
//! may derive an unknown number, the denies and checks undetermined until
//! a tick, and held changes this plan does not schedule; the denies, the
//! changes held for approval, shadowed disagreements and conflicts; then
//! what apply does. Each change says where it is derived, each attribute
//! where its value was written, and why it changed since the last apply,
//! as much as [`Why`] asks. `--why=none` is the layout before R-79
//! ([`bare`]).
//!
//! Every value passes through [`shown`] or [`shown_value`], which ask the
//! one [`Redactor`] that `query`, `why`, `graph` and `show` print through: a
//! null prints as its label, a secret, a value equal to one, or a value at
//! a sensitive path as `(sensitive LABEL)`. Nothing here formats a
//! sensitive value's bytes.

use crate::ast::{Atom, Program, RuleStmt, Term};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::provider::{Action, ActionKind, Plan};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::stuck::Sections;
use crate::value::{Value, null_owner};
use chains::write_chain;
use deformation::{Refs, deformation};
use errors::{diag_lines, diags};
use fold::{element_site, folded, scalar, schema_path};
use groups::{group_address, group_copy, groups};
use layout::{Row, layout};
use mask::{confusables_in, masked_text, null_class};
use policy::{Denied, deferred, denied, policies};
use std::collections::{BTreeMap, BTreeSet};
use style::kind_paint;
use tally::{by_kind, changes_text, count};
use tree::Site;
use waits::{Follow, boundary_owners, owners, provisional, provisional_text};

mod bare;
mod chains;
mod deformation;
pub mod deployments;
mod errors;
pub mod fold;
mod groups;
mod json;
mod labels;
mod layout;
mod mask;
pub mod policy;
pub mod progress;
mod style;
pub mod table;
mod tally;
pub mod tree;
mod waits;
pub use chains::{ChainItem, chains_text};
pub use deformation::{Deformation, Line, Op};
pub use errors::{
    CONFLICT, Diag, Failure, Unanswered, Witness, is_conflict, said_of, sites, violation_conflict,
    violation_line, violations,
};
pub use groups::{Group, group_pattern};
pub use labels::{
    address, address_text, attribute, attribute_label, kind_name, label, marker_of, path,
    reference, relation_name, short_id,
};
pub use layout::WIDTH;
pub(crate) use mask::elide;
pub use mask::{Shown, masked, shown, shown_value, surface_in};
pub use policy::Policy;
pub use style::{Paint, Style};
pub use tally::Tally;
pub(crate) use waits::waited;
pub use waits::{PendingBlock, Until, waits_on};

/// A change held for an approval (`requires_approval(r, reason)`).
#[derive(Debug, Clone)]
pub struct Approval {
    /// The resource, as the plan prints its address.
    pub addr: String,
    pub reason: String,
    pub site: Option<Site>,
}

/// How much of why each change is planned the report says (R-79, R-111):
/// a ladder, `-q` to `-vv`. `None` (`-q`): the bare diff, addresses and
/// values. `Line` (the default): each change where it is derived, each
/// value written outside its own block where, and the leaf that changed
/// since the last apply. `How` (`-v`): also how, the deriving
/// statement's bindings, the expression behind a value, the writes that
/// lost with their ranks. `Full` (`-vv`): also the derivation, compressed
/// to its leaves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    None,
    #[default]
    Line,
    How,
    Full,
}

impl Why {
    /// The level `-q` and `-v`/`-vv` ask for, else `why` (`--why=LEVEL`).
    pub fn of(quiet: bool, verbose: u8, why: Why) -> Why {
        match (quiet, verbose) {
            (true, _) => Why::None,
            (_, 0) => why,
            (_, 1) => Why::How,
            _ => Why::Full,
        }
    }
}

impl std::str::FromStr for Why {
    type Err = String;
    fn from_str(s: &str) -> Result<Why, String> {
        match s {
            "none" => Ok(Why::None),
            "line" => Ok(Why::Line),
            "how" => Ok(Why::How),
            "full" => Ok(Why::Full),
            _ => Err(format!("expected none, line, how or full, got {s:?}")),
        }
    }
}

/// Everything the plan printer shows, for text or JSON.
#[derive(Debug, Clone)]
pub struct Report {
    pub stack: String,
    pub show_noop: bool,
    pub definite: Vec<Deformation>,
    pub noops: usize,
    pub pending: Vec<PendingBlock>,
    pub groups: Vec<Group>,
    pub policies: Vec<Policy>,
    pub shadowed: Vec<Diag>,
    pub conflicts: Vec<Diag>,
    /// The apply order: per tick, the addresses applied in it.
    pub ticks: Vec<(usize, Vec<String>)>,
    /// Held for a null whose resolution is not scheduled by this plan.
    pub unscheduled: Vec<String>,
    pub undeformed: bool,
    pub moved: Vec<(Address, Address)>,
    pub denies: Vec<String>,
    /// Each of `denies`, as the plan's own rows say it ([`Report::explain`]).
    pub denied: Vec<Denied>,
    /// Every null the sections name, with its class, for `--json`.
    pub classes: BTreeMap<String, String>,
    /// The tick this report's definite changes run in (1 for `plan`).
    pub tick: usize,
    /// What each change says of why it is planned ([`Report::explain`]).
    pub why: Why,
    /// The changes held for an approval.
    pub approvals: Vec<Approval>,
    /// The program's copies: a copy's deformations print under it, a
    /// composite resource (R-67).
    pub instances: crate::zset::Instances,
    /// What the plan empties since the last apply (R-80): a rule all of
    /// whose resources it deletes, a relation it leaves with no rows.
    pub warnings: Vec<crate::zset::Emptied>,
    /// The statements that derive no resource, each with why (R-120).
    pub not_planned: Vec<crate::zset::NotPlanned>,
    /// The stack's keys: a value's chain ends at one (R-122).
    pub keys: BTreeSet<String>,
    /// The apply resumes one interrupted: its tick's changes are what
    /// remained of it (R-122).
    pub resumed: bool,
    /// The deployment is being removed (`destroy`, R-149): the operation
    /// is every delete's reason, so none says one.
    pub removing: bool,
    /// Every line of a change says its site ([`Report::explain`]), though
    /// the default level prints a create folded (`plan --json` keeps
    /// them all); else only the lines the fold prints find theirs.
    pub every_site: bool,
    /// The objects the plan leaves as they are but for a value given at
    /// their creation only that differs ([`Deformation::kept`], R-198):
    /// no change, each said under its address.
    pub kept: Vec<Deformation>,
    /// The program's policies, each a tally of what it ranges over
    /// (R-200's policy block).
    pub policy: Vec<policy::Line>,
    /// The plan is one deployment's in a tree of them (R-200): the tree's
    /// headline and the deployment's header line say what its own
    /// headline and `up to date` line would.
    pub nested: bool,
}

/// The header of a used module's instance in the plan's tree, `module
/// k3s` (R-200), as `why` names the scope.
const MODULE: &str = "module";

/// One line of a tick's tree ([`Report::outline`]).
#[derive(Debug, Clone)]
pub enum Node<'r> {
    /// A copy, or a used module's instance (`module k3s`), the changes
    /// after it at a deeper level its own; `kind` gives its mark.
    Header {
        addr: Address,
        kind: ActionKind,
    },
    Change(&'r Deformation),
}

/// The scopes of `addrs` that are no copy (a used module's instance) and
/// hold two or more of them: what the plan's tree groups under a header.
fn shared_modules<'a>(
    addrs: impl Iterator<Item = &'a Address>,
    instances: &crate::zset::Instances,
) -> BTreeSet<String> {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for a in addrs {
        let mut name = a.name.as_str();
        while let Some((scope, _)) = crate::ir::scope_split(name) {
            if instances.address(scope).is_none() {
                *n.entry(scope).or_default() += 1;
            }
            name = scope;
        }
    }
    n.into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(s, _)| s.to_string())
        .collect()
}

/// A forget's note on its line (R-154).
pub const FORGOTTEN: &str = "  forgotten, kept in the world  (lifecycle retain)";

/// What a line says of a value given at creation only that differs from
/// the object's (R-198): `user_data differs (bootstrap): kept`.
pub const KEPT: &str = "(bootstrap): kept";

/// What the report is built from.
pub struct Input<'a> {
    pub plan: &'a Plan,
    pub res: &'a EvalResult,
    pub sections: &'a Sections,
    pub program: &'a Program,
    pub schema: &'a Schema,
    pub stack: &'a str,
    pub show_noop: bool,
    /// The tick this plan's definite deformations run in (1 for `plan`).
    pub tick: usize,
    /// `moved/3` renames applied to state before this plan (E §3.4).
    pub moved: &'a [(Address, Address)],
    /// Denies over the plan itself (`lifecycle prevent_destroy`).
    pub denies: &'a [String],
    /// The copies state remembers (`state::State::instances`): a removed
    /// copy's deletes print under it.
    pub kept: &'a BTreeMap<String, String>,
}

/// The resources an attribute of which conflicts: the report shows their
/// conflicts in place of any deformation of theirs, and the provider is
/// not asked to plan them (its refusal of the document the conflict left
/// incomplete would hide the conflict).
pub fn conflicted(res: &EvalResult) -> BTreeSet<Address> {
    diags(res, &Redactor::default(), "deny", CONFLICT)
        .into_iter()
        .map(|d| d.addr)
        .collect()
}

pub fn report(i: &Input) -> Report {
    let r = Redactor::new(&i.res.facts, i.schema);
    let refs = Refs::new(&i.res.facts);
    let mut conflicts = diags(i.res, &r, "deny", CONFLICT);
    conflicts.extend(diags(i.res, &r, "deny", crate::refine::VIOLATED));
    let conflicted: BTreeSet<&Address> = conflicts.iter().map(|d| &d.addr).collect();
    let (held, definite): (Vec<&Action>, Vec<&Action>) = i
        .plan
        .actions
        .iter()
        .filter(|a| !conflicted.contains(&a.addr))
        .partition(|a| waits_on(a, i.sections).is_some());
    let noops = definite
        .iter()
        .filter(|a| matches!(a.kind, ActionKind::Noop))
        .count();

    // The schedule, by the dependency graph alone (R-156): definite
    // deformations run in this tick; a held one runs after everything it
    // waits on is made, which is the tick after the last of their owners'.
    // A wait no null names (a provider's settings, a CRD) is made by what
    // `boundary_owners` says; one nothing in this plan makes is `later`'s.
    let mut tick_of: BTreeMap<(String, String), usize> = definite
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop))
        .map(|a| ((a.addr.typ.clone(), a.addr.name.clone()), i.tick))
        .collect();
    let made_by = boundary_owners(i, &held);
    let resolves = |on: &[String], tick_of: &BTreeMap<(String, String), usize>| {
        on.iter()
            .map(|n| match made_by.get(n) {
                Some(owners) => owners
                    .iter()
                    .map(|o| tick_of.get(o).copied())
                    .collect::<Option<Vec<usize>>>()
                    .and_then(|ts| ts.into_iter().max()),
                None => null_owner(n).and_then(|o| tick_of.get(&o).copied()),
            })
            .collect::<Option<Vec<usize>>>()
            .and_then(|ts| ts.into_iter().max())
    };
    loop {
        let mut changed = false;
        for a in &held {
            let key = (a.addr.typ.clone(), a.addr.name.clone());
            if tick_of.contains_key(&key) {
                continue;
            }
            let on = waits_on(a, i.sections).unwrap_or_default();
            if let Some(t) = resolves(&on, &tick_of) {
                tick_of.insert(key, t + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut by_nulls: BTreeMap<Vec<String>, Vec<Deformation>> = BTreeMap::new();
    for a in &held {
        let on = waits_on(a, i.sections).unwrap_or_default();
        by_nulls
            .entry(on)
            .or_default()
            .push(deformation(a, i.schema, &r, &refs));
    }
    let follow = Follow::new(i, &held, &tick_of);
    let pending: Vec<PendingBlock> = by_nulls
        .into_iter()
        .map(|(on, deformations)| {
            let resolves_after = resolves(&on, &tick_of);
            PendingBlock {
                until: match resolves_after {
                    Some(_) => BTreeSet::new(),
                    None => follow.until(&on),
                },
                provisional: provisional(&on, i.sections),
                resolves_after,
                on,
                deformations,
            }
        })
        .collect();

    let groups = groups(i.res, &tick_of, &resolves);
    let not_planned = crate::zset::not_planned(i.res, &r);
    let mut policies = policies(i, &tick_of, &resolves);
    policies.extend(deferred(i.res, &tick_of, &resolves));
    let policy = policy::lines(i.program, i.res, &policies, &r);
    for p in policies.iter_mut().filter(|p| p.after.is_none()) {
        p.until = follow.until(&p.on);
    }

    let mut ticks: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut unscheduled = Vec::new();
    for a in definite.iter().chain(&held) {
        if matches!(a.kind, ActionKind::Noop) {
            continue;
        }
        let name = a.addr.to_string();
        match tick_of.get(&(a.addr.typ.clone(), a.addr.name.clone())) {
            Some(t) => ticks.entry(*t).or_default().push(name),
            None => unscheduled.push(name),
        }
    }
    // create_before_destroy deposes the old object; it is deleted at the
    // next tick, once what depends on it has moved to the replacement.
    for a in &definite {
        if matches!(a.kind, ActionKind::Replace { create_first: true }) {
            ticks
                .entry(i.tick + 1)
                .or_default()
                .push(format!("{} (deposed)", a.addr));
        }
    }
    for g in &groups {
        match g.resolves_after {
            Some(t) => ticks.entry(t + 1).or_default().push(g.pattern.clone()),
            None => unscheduled.push(g.pattern.clone()),
        }
    }

    // E §2.7: undeformed is the zero Z-set, nothing stuck, no cell stuck.
    let undeformed = noops == definite.len()
        && held.is_empty()
        && conflicts.is_empty()
        && i.sections.blocking.is_empty()
        && i.sections.undetermined.is_empty()
        && i.sections.pending_groups.is_empty()
        && not_planned.is_empty();
    let classes = pending
        .iter()
        .flat_map(|b| b.on.iter())
        .chain(groups.iter().flat_map(|g| g.on.iter()))
        .chain(policies.iter().flat_map(|p| p.on.iter()))
        .map(|l| (l.clone(), null_class(l, i.schema)))
        .collect();
    Report {
        stack: i.stack.to_string(),
        show_noop: i.show_noop,
        definite: definite
            .iter()
            .filter(|a| i.show_noop || !matches!(a.kind, ActionKind::Noop))
            .map(|a| deformation(a, i.schema, &r, &refs))
            .collect(),
        noops,
        pending,
        groups,
        policies,
        shadowed: diags(
            i.res,
            &r,
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
        ),
        conflicts,
        ticks: ticks.into_iter().collect(),
        unscheduled,
        undeformed,
        moved: i.moved.to_vec(),
        denies: i.denies.to_vec(),
        denied: Vec::new(),
        classes,
        tick: i.tick,
        why: Why::None,
        keys: BTreeSet::new(),
        resumed: false,
        removing: false,
        every_site: true,
        approvals: crate::approval::needs(&i.res.facts)
            .into_iter()
            .map(|(addr, reason)| Approval {
                addr,
                reason,
                site: None,
            })
            .collect(),
        instances: crate::zset::Instances::from_facts(&i.res.facts).with(i.kept),
        warnings: Vec::new(),
        not_planned,
        // Said once, by the plan an apply shows first, not again at each
        // boundary.
        kept: definite
            .iter()
            .filter(|_| i.tick == 1 && !i.show_noop)
            .filter(|a| matches!(a.kind, ActionKind::Noop) && !a.kept().is_empty())
            .map(|a| deformation(a, i.schema, &r, &refs))
            .collect(),
        policy,
        nested: false,
    }
}

/// Where a change is derived, as its line's site column says it at `why`
/// (R-111): `FILE:LINE` (a value given on the command line, the flag);
/// from `-v`, the statement's bindings after it, `with z = "a"`.
fn place_text(s: &Site, why: Why) -> String {
    let mut out = match s.at.is_empty() {
        true => s.statement.clone(),
        false => s.at.clone(),
    };
    if why >= Why::How && !s.with.is_empty() {
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(&format!("with {}", s.with.join(", ")));
    }
    out
}

/// The writes a winning value of change `d` overrode, from `-v` (R-111):
/// `  @normal over k3s.df:9 @default`; not one in `d`'s own block (a
/// default the compiler writes there).
fn beat_text(s: &Site, d: &Deformation) -> String {
    let own = |at: &str| {
        let (Some(h), Some((file, line))) = (&d.site, at.rsplit_once(':')) else {
            return false;
        };
        let Some((hf, first)) = h.at.rsplit_once(':') else {
            return false;
        };
        let (Ok(line), Ok(first)) = (line.parse::<usize>(), first.parse::<usize>()) else {
            return false;
        };
        hf == file && (first..=h.last.max(first)).contains(&line)
    };
    match (&s.beat, &s.beat_at) {
        (Some(_), Some(at)) if own(at) => String::new(),
        (Some(b), Some(at)) => {
            let rank = s.rank.as_deref().unwrap_or("normal");
            format!("  @{rank} over {at} @{b}")
        }
        (Some(b), None) => format!("  over @{b}"),
        _ => String::new(),
    }
}

/// What wrote an attribute's value at `-v`: `STATEMENT   FILE:LINE`, then
/// `FILE:LINE` alone (a constant: its entry when it reads something, else
/// its place); the writes it won over after either.
fn written_text(s: &Site, d: &Deformation) -> Vec<String> {
    let beat = beat_text(s, d);
    if s.at.is_empty() {
        return vec![format!("{}{beat}", s.statement)];
    }
    if s.stated {
        let entry = s
            .entry
            .iter()
            .filter(|e| e.split_once(" = ").is_some_and(|(_, rhs)| reads(rhs)))
            .map(|e| format!("{e}   {}{beat}", s.at));
        return entry.chain([format!("{}{beat}", s.at)]).collect();
    }
    let mut out = vec![format!("{}   {}{beat}", s.statement, s.at)];
    out.extend(s.entry.iter().map(|e| format!("{e}   {}{beat}", s.at)));
    out.push(format!("{}{beat}", s.at));
    out
}

impl Report {
    /// When a change this plan holds runs, and what it waits on, as `why`
    /// says it (After R-156): `tick 2  waits on  main.endpoint`; a resource
    /// rule's group `tick 2+  ..`, its tick a lower bound (what it is
    /// stuck on first); `later  waits on  ..` for what no tick of this plan
    /// makes. `at`: a change's address, full or as the plan prints it, or
    /// a group's. `None` for a change this tick makes, or none.
    pub fn when(&self, at: &str) -> Option<String> {
        let line = |tick: Option<usize>, plus: &str, on: &[String]| {
            let when = match tick {
                Some(t) => format!("tick {}{plus}", t + 1),
                None => "later".into(),
            };
            let on = waited(&on.iter().cloned().collect()).join(", ");
            match on.is_empty() {
                true => when,
                false => format!("{when}  waits on  {on}"),
            }
        };
        let named = |full: &str, shown: String| full == at || shown == at;
        for b in &self.pending {
            if b.deformations
                .iter()
                .any(|d| named(&d.addr.to_string(), address(&d.addr)))
            {
                return Some(line(b.resolves_after, "", &b.on));
            }
        }
        let g = self
            .groups
            .iter()
            .find(|g| named(&g.pattern, address_text(&group_address(g))))?;
        Some(line(g.resolves_after, "+", &g.on))
    }

    /// The object's value of `path` of `addr`, given at its creation only,
    /// where the plan keeps it (R-198): `"n1"`, `(sensitive)`.
    pub fn kept_value(&self, addr: &Address, path: &str) -> Option<String> {
        let pending = self.pending.iter().flat_map(|b| b.deformations.iter());
        self.kept
            .iter()
            .chain(&self.definite)
            .chain(pending)
            .filter(|d| d.addr == *addr)
            .flat_map(|d| &d.kept)
            .find(|l| l.path == path)
            .map(|l| l.before.said(Why::Line))
    }

    /// Say why each change is planned, at level `why` (R-79), from the
    /// provenance of `res`, the evaluation the plan was made from: each
    /// entry where it is derived, with its bindings; each attribute it
    /// changes where its winning value was written; at `Full`, under
    /// each, the derivation compressed to its leaves (a create by its
    /// `want`, an update by the winning contributions to each attribute it
    /// changes, a delete by state alone). Every line passes through `r`.
    pub fn explain(&mut self, why: Why, res: &EvalResult, r: &Redactor) {
        self.why = why;
        if why == Why::None {
            return;
        }
        let p = tree::Printer {
            circuit: &res.circuit,
            redact: r,
            all: false,
        };
        let rules = &res.rules;
        let changed: BTreeSet<(String, String)> = self
            .definite
            .iter()
            .chain(self.pending.iter().flat_map(|b| b.deformations.iter()))
            .map(|d| (d.addr.typ.clone(), d.addr.name.clone()))
            .collect();
        let mut attrs: BTreeMap<(String, String), Vec<&Atom>> = BTreeMap::new();
        for f in res.facts.iter().filter(|f| f.pred == "attr") {
            if let [Term::Val(Value::Str(t)), Term::Val(Value::Str(a)), ..] = f.args.as_slice() {
                let k = (t.clone(), a.clone());
                if changed.contains(&k) {
                    attrs.entry(k).or_default().push(f);
                }
            }
        }
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        let created: Vec<(Address, BTreeSet<(String, String)>)> = self
            .definite
            .iter()
            .filter(|d| matches!(d.kind, ActionKind::Create))
            .map(|d| (d.addr.clone(), values(d, |l| &l.after)))
            .collect();
        let live = crate::zset::Instances::from_facts(&res.facts);
        let removing = self.removing;
        let instances = &self.instances;
        for d in self.definite.iter_mut().chain(pending) {
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                // A destroy's deletes need no reason: the operation is it.
                // A deposed object is the one a replace left: no rule
                // was ever to want it.
                if !removing && matches!(d.kind, ActionKind::Delete) {
                    d.gone = gone(d, &created, instances, &live, res, r);
                }
                continue;
            }
            d.site = p.want_site(rules, &d.addr);
            let facts = attrs
                .get(&(d.addr.typ.clone(), d.addr.name.clone()))
                .map(Vec::as_slice)
                .unwrap_or_default();
            // A create the default level folds prints few of its lines
            // (a document's are its row, R-131): each printed line's site
            // is found as the fold prints it, unless every line says its
            // own (`every_site`: `plan --json`).
            let folds = why < Why::Full && matches!(d.kind, ActionKind::Create | ActionKind::Adopt);
            if folds && !self.every_site {
                let mut site = |l: &Line| {
                    attr_holding(facts, &l.path)
                        .and_then(|(a, keys, whole)| {
                            p.attr_sites(rules, a, &[(keys, whole)]).pop().flatten()
                        })
                        .or_else(|| element_site(&p, rules, facts, l))
                };
                d.folded = folded(d, &p, rules, facts, &res.facts, why, &mut site);
                continue;
            }
            // Each line's site, the lines under one attribute fact asked
            // together.
            // By the fact (its address): the fact, and each line's index,
            // keys and whether they reach the leaf.
            type Asked<'a> = (&'a Atom, Vec<(usize, (Vec<String>, bool))>);
            let mut asks: BTreeMap<*const Atom, Asked> = BTreeMap::new();
            for (i, l) in d.lines.iter().enumerate() {
                if let Some((a, keys, whole)) = attr_holding(facts, &l.path) {
                    asks.entry(a as *const Atom)
                        .or_insert_with(|| (a, Vec::new()))
                        .1
                        .push((i, (keys, whole)));
                }
            }
            let mut sites: Vec<Option<tree::Site>> = vec![None; d.lines.len()];
            for (a, lines) in asks.into_values() {
                let (at, asked): (Vec<usize>, Vec<(Vec<String>, bool)>) = lines.into_iter().unzip();
                for (i, site) in at.into_iter().zip(p.attr_sites(rules, a, &asked)) {
                    sites[i] = site;
                }
            }
            for (l, site) in d.lines.iter_mut().zip(sites) {
                l.site = site.or_else(|| element_site(&p, rules, facts, l));
                if why == Why::Full {
                    l.chain = attr_chain(&p, rules, facts, &l.path, &self.keys);
                }
            }
            if folds {
                d.folded = folded(d, &p, rules, facts, &res.facts, why, &mut |l| {
                    l.site.clone()
                });
            }
        }
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            d.lines
                .iter_mut()
                .chain(d.folded.iter_mut())
                .for_each(Line::mask);
        }
        for d in &mut self.kept {
            d.site = p.want_site(rules, &d.addr);
        }
        for g in &mut self.groups {
            g.site = g
                .rule
                .as_ref()
                .and_then(|id| p.rule_site(rules, id, &g.bindings));
        }
        for x in &mut self.policies {
            x.site = x.rule.as_ref().and_then(|id| p.rule_site(rules, id, &[]));
        }
        // A check's site is its rule's.
        for l in self.policy.iter_mut().filter(|l| l.at.is_empty()) {
            let site = self
                .policies
                .iter()
                .find_map(|x| match x.message == l.text {
                    true => x.site.as_ref(),
                    false => None,
                });
            if let Some(site) = site {
                l.at = site.at.clone();
            }
        }
        for n in &mut self.not_planned {
            n.site = n.rule.as_ref().and_then(|id| p.rule_site(rules, id, &[]));
        }
        self.denied = self
            .denies
            .iter()
            .map(|text| denied(&p, res, text))
            .collect();
        for a in &mut self.approvals {
            a.site = res
                .facts
                .iter()
                .filter(|f| f.pred == "requires_approval" && f.args.len() == 2)
                .find(|f| {
                    crate::approval::needs(&BTreeSet::from([(*f).clone()]))
                        == [(a.addr.clone(), a.reason.clone())]
                })
                .and_then(|f| res.circuit.fact_id(&crate::engine::circuit_fact(f)))
                .and_then(|id| p.site(rules, id));
        }
    }

    /// Under each change, the leaf that changed since the last apply
    /// (R-79): `then` is the snapshot of the program the last apply ran,
    /// `now` this plan's, both for the plan's addresses. A delete is also
    /// said where the last apply derived it (`was FILE:LINE`).
    pub fn because(&mut self, then: &crate::diff::Snapshot, now: &crate::diff::Snapshot) {
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            let addr = d.addr.to_string();
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                d.site = then.sites.get(&addr).cloned();
            }
            let paths: Vec<String> = d.lines.iter().map(|l| l.path.clone()).collect();
            d.because = crate::diff::because_since(kind_name(&d.kind), &addr, &paths, then, now);
        }
    }

    /// Every site's place relative to `top`, the project's root (the
    /// program's directory outside a project), whatever directory the run
    /// is in and however it named the program.
    pub fn relative_to(&mut self, top: &std::path::Path) {
        let place = |at: &str| relative_place(at, top);
        let fix = |s: &mut Option<Site>| {
            if let Some(s) = s
                && let Some(at) = place(&s.at)
            {
                s.at = at;
            }
        };
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            fix(&mut d.site);
            for l in d.lines.iter_mut().chain(d.folded.iter_mut()) {
                fix(&mut l.site);
                for step in l.chain.iter_mut() {
                    if let Some(at) = place(&step.at) {
                        step.at = at;
                    }
                }
            }
        }
        self.kept.iter_mut().for_each(|d| fix(&mut d.site));
        self.groups.iter_mut().for_each(|g| fix(&mut g.site));
        self.policies.iter_mut().for_each(|p| fix(&mut p.site));
        for l in &mut self.policy {
            if let Some(at) = place(&l.at) {
                l.at = at;
            }
        }
        self.approvals.iter_mut().for_each(|a| fix(&mut a.site));
        self.denied.iter_mut().for_each(|d| fix(&mut d.site));
        for d in self.conflicts.iter_mut().chain(self.shadowed.iter_mut()) {
            let witnesses = d.witnesses.iter_mut().flat_map(|w| w.at.iter_mut());
            for at in witnesses.chain(d.at.as_mut()) {
                if let Some(p) = place(at) {
                    *at = p;
                }
            }
        }
    }

    /// The changes the plan counts: every definite one that is not a
    /// no-op, and every held one a later tick of this plan makes (one
    /// waiting on what this plan does not resolve is `later`'s).
    fn counted(&self) -> impl Iterator<Item = &Deformation> {
        self.definite
            .iter()
            .filter(|d| !matches!(d.kind, ActionKind::Noop))
            .chain(
                self.pending
                    .iter()
                    .filter(|b| b.resolves_after.is_some())
                    .flat_map(|b| b.deformations.iter())
                    // An object state holds, as state has it until its
                    // provider reads it at the boundary (R-177).
                    .filter(|d| !matches!(d.kind, ActionKind::Noop)),
            )
    }

    /// The changes by kind, in summary order.
    fn kinds(&self) -> Vec<(&'static str, usize)> {
        by_kind(self.counted())
    }

    /// How many changes the plan has, in every tick (the summary's count).
    pub fn changes(&self) -> usize {
        self.counted().count()
    }

    /// The ticks, each with its changes, what it waits on and the
    /// objects it deletes once their replacements stand; the first is
    /// this report's tick. Held changes waiting on what this plan does
    /// not schedule are not in any.
    fn sections(&self) -> BTreeMap<usize, Section<'_>> {
        let mut out: BTreeMap<usize, Section> = BTreeMap::new();
        let shown: Vec<&Deformation> = self.definite.iter().collect();
        if !shown.is_empty() {
            out.entry(self.tick).or_default().changes = shown;
        }
        for b in &self.pending {
            let Some(t) = b.resolves_after else { continue };
            let s = out.entry(t + 1).or_default();
            s.changes.extend(b.deformations.iter());
            s.waits.extend(b.on.iter().cloned());
            for k in b.provisional.iter().flatten() {
                if !s.provisional.contains(k) {
                    s.provisional.push(k.clone());
                }
            }
        }
        for d in &self.definite {
            if matches!(d.kind, ActionKind::Replace { create_first: true }) {
                out.entry(self.tick + 1).or_default().deposed.push(&d.addr);
            }
        }
        for g in &self.groups {
            let Some(t) = g.resolves_after else { continue };
            let s = out.entry(t + 1).or_default();
            s.groups.push(g);
            s.waits.extend(g.on.iter().cloned());
        }
        out
    }

    /// What `later` holds: rules that may derive an unknown number of
    /// resources, and held changes whose nulls this plan does not resolve
    /// (an undetermined deny or check is the policy block's).
    fn has_later(&self) -> bool {
        self.groups.iter().any(|g| g.resolves_after.is_none())
            || self.pending.iter().any(|b| b.resolves_after.is_none())
    }

    /// The last line, only when there is something to decide: apply
    /// refuses this plan, `apply: refused  2 conflicts, 1 deny`.
    pub fn apply_line(&self) -> Option<String> {
        let mut why = Vec::new();
        if !self.conflicts.is_empty() {
            why.push(count(self.conflicts.len(), "conflict"));
        }
        if !self.denies.is_empty() {
            why.push(match self.denies.len() {
                1 => "1 deny".to_string(),
                n => format!("{n} denies"),
            });
        }
        let verb = match self.removing {
            true => "destroy",
            false => "apply",
        };
        (!why.is_empty()).then(|| format!("{verb}: refused  {}", why.join(", ")))
    }

    /// The host labels the plan's values hold that a reader may mistake for
    /// others (R-134): each with its attribute and where it was written;
    /// a warning at every level, `-q` included.
    pub fn confusable_lines(&self) -> Vec<String> {
        fn walk(l: &Line, addr: &Address, prefix: &str, out: &mut Vec<String>) {
            let path = match prefix.is_empty() {
                true => l.path.clone(),
                false => format!("{prefix}.{}", l.path),
            };
            let at = l
                .site
                .as_ref()
                .filter(|s| !s.at.is_empty())
                .map(|s| format!("  {}", s.at))
                .unwrap_or_default();
            let attr = attribute(addr, &path);
            for v in [&l.after, &l.before] {
                if let Shown::Value(j) = v {
                    confusables_in(j, &attr, &at, out);
                }
            }
            for x in &l.leaves {
                walk(x, addr, &path, out);
            }
        }
        let mut out = Vec::new();
        let all = self
            .definite
            .iter()
            .chain(self.pending.iter().flat_map(|b| b.deformations.iter()));
        for d in all {
            for l in &d.lines {
                walk(l, &d.addr, "", &mut out);
            }
        }
        out
    }

    /// The `warning` section's lines (R-80), unindented: each rule the
    /// plan deletes every resource of, with what it deletes and the leaf
    /// that changed since the last apply; each relation it empties.
    fn warning_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for w in &self.warnings {
            match &w.statement {
                Some(statement) => {
                    out.push(format!("{}  {statement}", w.name));
                    let shown: Vec<String> =
                        w.deleted.iter().take(3).map(|a| address_text(a)).collect();
                    let more = match w.deleted.len().saturating_sub(3) {
                        0 => String::new(),
                        n => format!(" and {n} more"),
                    };
                    let n = w.deleted.len();
                    out.push(format!(
                        "    deletes all {n} it derived at the last apply: {}{more}",
                        shown.join(", ")
                    ));
                    if let Some(b) = &w.because {
                        out.push(format!("    because {b}"));
                    }
                }
                None => {
                    let rows = match w.rows {
                        1 => "1 row".to_string(),
                        n => format!("{n} rows"),
                    };
                    out.push(format!(
                        "{}  had {rows} at the last apply, has none now",
                        relation_name(&w.name)
                    ));
                }
            }
        }
        out
    }

    /// The report as text, uncoloured: what `plan` prints with no colour,
    /// every golden, and the controller's log line.
    pub fn text(&self) -> String {
        self.render(Style::PLAIN)
    }

    /// The report as text, painted with `style`: the summary, each tick
    /// with its changes, `later`, the diagnostics, and what apply does.
    pub fn render(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            let mut rows = Vec::new();
            self.write_kept(&mut rows, style);
            out.push_str(&layout(&rows, style));
            if !self.nested {
                out.push_str(&format!("stack {} is up to date\n", self.stack));
            }
            return out;
        }
        let mut rows: Vec<Row> = match self.nested {
            true => Vec::new(),
            false => vec![Row::plain(self.summary())],
        };
        for (t, s) in self.sections() {
            rows.push(Row::plain(String::new()));
            // A held object state has, unread until the boundary
            // (R-177), is listed as state has it, not counted.
            let n = s
                .changes
                .iter()
                .filter(|d| self.show_noop || !matches!(d.kind, ActionKind::Noop))
                .count();
            // A rule the tick before decides may add changes: how many
            // is not known yet (R-156).
            let head = match (self.resumed && t == self.tick, s.groups.is_empty(), n) {
                (true, _, _) => format!("tick {t}  {n} remaining, resumed"),
                (false, true, _) => format!("tick {t}  {}", count(n, "change")),
                (false, false, 0) => format!("tick {t}  ? changes"),
                (false, false, _) => format!("tick {t}  {n}+ changes"),
            };
            rows.push(Row::new(&head, bold(&head)));
            for (i, w) in waited(&s.waits).into_iter().enumerate() {
                let lead = if i == 0 {
                    "  waits on  "
                } else {
                    "            "
                };
                rows.push(Row::plain(format!("{lead}{w}")));
            }
            if !s.provisional.is_empty() {
                rows.push(Row::plain(format!(
                    "  {}",
                    provisional_text(&s.provisional)
                )));
            }
            self.write_level(&mut rows, &s.changes, "  ", style);
            for a in &s.deposed {
                let addr = address(a);
                let plain = format!("  - {addr}  (deposed)");
                let painted = format!(
                    "  {} {}  (deposed)",
                    style.paint(Paint::Delete, "-"),
                    style.address(&ActionKind::Delete, &addr)
                );
                rows.push(Row::new(&plain, painted));
            }
            self.write_groups(&mut rows, &s.groups, style);
        }
        if !self.kept.is_empty() {
            rows.push(Row::plain(String::new()));
            self.write_kept(&mut rows, style);
        }
        // The policy block (R-200): after the ticks, before what waits.
        // Its columns are its own: laid out apart, it moves no column of
        // the ticks'.
        let policy = policy::rows(&self.policy, self.why, style);
        if !policy.is_empty() {
            rows.push(Row::plain(String::new()));
            for line in layout(&policy, style).lines() {
                rows.push(Row::plain(line.to_string()));
            }
        }
        if self.has_later() {
            rows.push(Row::plain(String::new()));
            let head = "later";
            rows.push(Row::new(head, bold(head)));
            self.write_later(&mut rows, style);
        }
        // What the program states and derives nothing of (R-120): each
        // statement as a group row is, its place and why on the right.
        if !self.not_planned.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "not planned";
            rows.push(Row::new(head, style.paint(Paint::Warn, head)));
            for n in &self.not_planned {
                let addr = address(&n.addr);
                let plain = format!("  {addr}");
                let painted = format!("  {}", style.paint(Paint::Warn, &addr));
                let at = n.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                let right = match at.is_empty() {
                    true => vec![n.reason.clone()],
                    false => vec![format!("{at}  {}", n.reason), n.reason.clone()],
                };
                rows.push(Row::new(&plain, painted).with(right));
            }
        }
        let confusable = self.confusable_lines();
        if !self.warnings.is_empty() || !confusable.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "warning";
            rows.push(Row::new(head, style.paint(Paint::Warn, head)));
            for line in self.warning_lines().into_iter().chain(confusable) {
                rows.push(Row::plain(format!("  {line}")));
            }
        }
        if !self.denies.is_empty() {
            rows.push(Row::plain(String::new()));
            rows.push(Row::new("denied", style.paint(Paint::Error, "denied")));
            let wide = self
                .denied
                .iter()
                .map(|d| address_text(&d.addr).chars().count())
                .max()
                .unwrap_or(0);
            for (i, d) in self.denies.iter().enumerate() {
                let row = self.denied.get(i);
                let message = row.map(|r| r.message.as_str()).unwrap_or(d);
                let left = format!("  {message}");
                let right = row
                    .map(|r| {
                        let at = r.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                        let addr = address_text(&r.addr);
                        let pad = " ".repeat(wide - addr.chars().count());
                        format!("{addr}{pad}    {at}").trim().to_string()
                    })
                    .into_iter()
                    .collect();
                rows.push(Row::new(&left, style.paint(Paint::Error, &left)).with(right));
            }
        }
        if !self.approvals.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "held for approval";
            rows.push(Row::new(head, style.paint(Paint::Held, head)));
            let wide = self
                .approvals
                .iter()
                .map(|a| a.reason.chars().count())
                .max()
                .unwrap_or(0);
            for a in &self.approvals {
                let at = a.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                let pad = " ".repeat(wide - a.reason.chars().count());
                let right = format!("{}{pad}    {at}", a.reason).trim_end().to_string();
                rows.push(Row::plain(format!("  {}", address_text(&a.addr))).with(vec![right]));
            }
        }
        out.push_str(&layout(&rows, style));
        let header = |s: &str| format!("{}\n", bold(s));
        let diags = [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)];
        for (title, ds) in diags {
            if ds.is_empty() {
                continue;
            }
            out.push('\n');
            let conflict = title == "conflicts";
            let error = |s: &str| match conflict {
                true => style.paint(Paint::Error, s),
                false => s.to_string(),
            };
            out.push_str(&header(&error(title)));
            for d in ds {
                out.push_str(&diag_lines(d, self.why, style, conflict));
            }
        }
        if self.undeformed {
            if !self.nested {
                out.push_str(&format!("\nstack {} is up to date\n", self.stack));
            }
            return out;
        }
        if let Some(line) = self.apply_line() {
            out.push('\n');
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// The objects left as they are but for a value given at their
    /// creation only (R-198), each `=` with what it keeps.
    fn write_kept(&self, rows: &mut Vec<Row>, style: Style) {
        for d in &self.kept {
            self.write_change(rows, d, "", style);
        }
    }

    /// `later`'s rows: each group no tick of this plan decides, each held
    /// change this plan does not schedule (R-156).
    fn write_later(&self, rows: &mut Vec<Row>, style: Style) {
        let unscheduled: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| g.resolves_after.is_none())
            .collect();
        self.write_groups(rows, &unscheduled, style);
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds: Vec<&Deformation> = b.deformations.iter().collect();
            let on = waited(&b.on.iter().cloned().collect()).join(", ");
            // A header like a tick's (R-111).
            rows.push(Row::plain(format!("  waits on  {on}")));
            if let Some(keys) = &b.provisional {
                rows.push(Row::plain(format!("  {}", provisional_text(keys))));
            }
            self.write_level(rows, &ds, "  ", style);
        }
    }

    /// Group rows (R-67, R-156): each by the address its rule names (a
    /// copy that may derive once, its resources under it), with what it
    /// reads or waits on; `later`'s, or a tick's whose boundary decides it.
    fn write_groups(&self, rows: &mut Vec<Row>, groups: &[&Group], style: Style) {
        let site = |s: &Option<Site>| s.as_ref().map(|s| s.at.clone()).unwrap_or_default();
        let full = self.why >= Why::How;
        let both = |at: String, cond: String, reason: &str| both(full, at, cond, reason);
        // A copy that may derive (R-67) is said once, its resources under it.
        let copy_of = group_copy;
        let mut copies: BTreeSet<String> = BTreeSet::new();
        for g in groups {
            let copy = copy_of(g);
            if let Some(c) = &copy {
                if !copies.insert(c.clone()) {
                    continue;
                }
                let reads = g.reads.clone().unwrap_or_default();
                let shown = address_text(c);
                let plain = format!("  {shown}");
                let painted = format!("  {}", style.paint(Paint::Bold, &shown));
                let reads = reads.strip_prefix("resource ").unwrap_or(&reads);
                rows.push(Row::new(&plain, painted).with(vec![format!("if {reads}")]));
                for m in groups.iter().filter(|m| copy_of(m).as_ref() == Some(c)) {
                    let pattern = address_text(&group_address(m));
                    let plain = format!("    {pattern}");
                    let painted = format!("    {}", style.paint(Paint::Warn, &pattern));
                    rows.push(Row::new(&plain, painted));
                }
                continue;
            }
            let full_pattern = group_address(g);
            let unknown = full_pattern.ends_with("[?]") || full_pattern.contains("${");
            let pattern = address_text(&full_pattern);
            let on = waited(&g.on.iter().cloned().collect()).join(", ");
            let cond = match (&g.reads, unknown) {
                (Some(r), true) => format!("one per {r}"),
                (Some(r), false) => format!("if {r}"),
                (None, _) => format!("waits on {on}"),
            };
            let mut right = both(site(&g.site), cond, &g.reason);
            // Too many values for the column: the resources they are of.
            if g.reads.is_none() {
                let owners = owners(&g.on.iter().cloned().collect()).join(", ");
                if owners != on {
                    right.extend(both(site(&g.site), format!("waits on {owners}"), &g.reason));
                }
            }
            let plain = format!("  {pattern}");
            let painted = format!("  {}", style.paint(Paint::Warn, &pattern));
            rows.push(Row::new(&plain, painted).with(right));
        }
    }

    /// The scopes `addr` is in, innermost first, as the plan's tree
    /// nests it: each copy, and each used module's instance of `shared`
    /// (`module k3s`).
    fn enclosing(&self, addr: &Address, shared: &BTreeSet<String>) -> Vec<Address> {
        let mut out = Vec::new();
        let mut name = addr.name.as_str();
        while let Some((scope, _)) = crate::ir::scope_split(name) {
            match self.instances.address(scope) {
                Some(copy) => out.push(copy),
                None if shared.contains(scope) => out.push(Address {
                    typ: MODULE.to_string(),
                    name: scope.to_string(),
                }),
                None => {}
            }
            name = scope;
        }
        out
    }

    /// Changes in order, a copy's under it (R-67): `+ network blue` at
    /// the place of its first resource, the resources indented beneath, a
    /// copy inside it nested again.
    fn write_level(&self, rows: &mut Vec<Row>, ds: &[&Deformation], indent: &str, style: Style) {
        self.walk_level(ds, None, 0, &mut |depth, node| {
            let indent = format!("{indent}{}", "  ".repeat(depth));
            match node {
                Node::Header { addr, kind } => {
                    let addr = address(&addr);
                    let plain = format!("{indent}{} {addr}", marker_of(&kind));
                    let painted = format!(
                        "{indent}{} {}",
                        style.marker(&kind),
                        style.paint(Paint::Bold, &addr)
                    );
                    rows.push(Row::new(&plain, painted));
                }
                Node::Change(d) => self.write_change(rows, d, &indent, style),
            }
        });
    }

    /// The tree of this report's tick (R-200, the path tree): each change
    /// in order, each copy and each used module's instance two or more of
    /// them share a header at the place of its first, what is in it one
    /// level deeper. The plan prints it with sites ([`Report::render`]),
    /// the apply with each call's status (R-206, `progress::Block`).
    pub fn outline(&self) -> Vec<(usize, Node<'_>)> {
        let ds: Vec<&Deformation> = self.definite.iter().collect();
        let mut out = Vec::new();
        self.walk_level(&ds, None, 0, &mut |depth, node| out.push((depth, node)));
        out
    }

    /// The changes `ds` under `outer` at `depth`, each copy's header with
    /// its kind: its `deformation` row's (`zset::Instances::row_kind`),
    /// `-` when the program wants none of its resources, `+` when one is
    /// created, else `~`.
    fn walk_level<'r>(
        &'r self,
        ds: &[&'r Deformation],
        outer: Option<&Address>,
        depth: usize,
        f: &mut dyn FnMut(usize, Node<'r>),
    ) {
        // The scopes the changes here share: each copy, and a used
        // module's instance two or more of them are in (R-200: the plan
        // is the path tree printed).
        let shared = shared_modules(ds.iter().map(|d| &d.addr), &self.instances);
        let enclosing = |a: &Address| self.enclosing(a, &shared);
        // The copy or module directly under `outer` a change is in, if any.
        let under = |d: &Deformation| -> Option<Address> {
            let chain = enclosing(&d.addr);
            let at = match outer {
                None => chain.len(),
                Some(o) => chain.iter().position(|a| a == o)?,
            };
            at.checked_sub(1).map(|i| chain[i].clone())
        };
        let mut done: BTreeSet<Address> = BTreeSet::new();
        for d in ds {
            let Some(copy) = under(d) else {
                f(depth, Node::Change(d));
                continue;
            };
            if !done.insert(copy.clone()) {
                continue;
            }
            let members: Vec<&Deformation> = ds
                .iter()
                .filter(|m| enclosing(&m.addr).contains(&copy))
                .copied()
                .collect();
            let kinds: Vec<&str> = members
                .iter()
                .filter_map(|m| crate::zset::deformation_kind(&m.kind, false))
                .collect();
            let kind = match self.instances.row_kind(&copy, &kinds) {
                "delete" => ActionKind::Delete,
                "create" => ActionKind::Create,
                _ => ActionKind::Update,
            };
            f(
                depth,
                Node::Header {
                    addr: copy.clone(),
                    kind,
                },
            );
            self.walk_level(&members, Some(&copy), depth + 1, f);
        }
    }

    /// One change (R-111): its marker and address with where it is
    /// derived, its attributes each with where its value was written when
    /// that is outside its block, at `-vv` what it rests on, and why it
    /// changed since the last apply.
    fn write_change(&self, rows: &mut Vec<Row>, d: &Deformation, indent: &str, style: Style) {
        let note = match d.kind {
            ActionKind::Drift => {
                "  (drift: a fresh null where the world has a value; its identity is stale)"
            }
            ActionKind::DeleteDeposed => "  (deposed)",
            ActionKind::Replace { create_first: true } => "  (the new one first)",
            ActionKind::Forget => FORGOTTEN,
            _ => "",
        };
        let addr = address(&d.addr);
        let plain = format!("{indent}{} {addr}{note}", marker_of(&d.kind));
        let painted = format!(
            "{indent}{} {}{note}",
            style.marker(&d.kind),
            style.address(&d.kind, &addr)
        );
        let at = d.site.as_ref().map(|s| place_text(s, self.why));
        let right: Vec<String> = match (&d.kind, at) {
            (ActionKind::Replace { .. }, at) => {
                let forces: Vec<String> = d
                    .forces
                    .iter()
                    .map(|p| format!("{p} forces replace"))
                    .collect();
                let both = at
                    .iter()
                    .flat_map(|at| forces.iter().map(move |f| format!("{at}  {f}")));
                both.chain(forces.iter().cloned()).collect()
            }
            (ActionKind::Delete, _) if !self.removing => gone_column(d, self.why),
            (ActionKind::Delete | ActionKind::DeleteDeposed, _) => vec![],
            // A create: the bindings that made this one, those its address
            // does not show (After R-149 amendment 5).
            (ActionKind::Create | ActionKind::Adopt, Some(at)) if self.why == Why::Line => {
                let s = d.site.as_ref().expect("a site");
                // A value the body prints (an attribute, a document by
                // its row) is said there.
                let shown: BTreeSet<String> = values(d, |l| &l.after)
                    .into_iter()
                    .map(|(_, v)| v.trim_matches('"').to_string())
                    .chain(
                        d.folded
                            .iter()
                            .chain(&d.lines)
                            .filter_map(|l| l.row.clone()),
                    )
                    .collect();
                let with: Vec<String> = s
                    .with
                    .iter()
                    .filter(|b| {
                        !b.split_once(" = ")
                            .is_some_and(|(_, v)| shown.contains(v.trim_matches('"')))
                    })
                    .cloned()
                    .collect();
                match terse(&with, &addr) {
                    Some(w) => vec![format!("{at}  {w}"), at],
                    None => vec![at],
                }
            }
            (_, Some(at)) => match d.site.as_ref() {
                // A line too long for its bindings keeps its place.
                Some(s) if at != s.at && !s.at.is_empty() => vec![at, s.at.clone()],
                _ => vec![at],
            },
            (_, None) => vec![],
        };
        let right = match &d.custody {
            Some(c) if right.is_empty() => vec![c.clone()],
            Some(c) => right.into_iter().map(|r| format!("{r}  {c}")).collect(),
            None => right,
        };
        rows.push(Row::new(&plain, painted).with(right));
        // Keep plan output readable.
        let max = 40usize;
        let inner = format!("{indent}    ");
        let lines = match d.folded.is_empty() {
            true => &d.lines,
            false => &d.folded,
        };
        // A replace: the attribute that forces it first (After R-149
        // amendment 5).
        let forced = |l: &Line| {
            d.forces.iter().any(|p| {
                l.path == *p
                    || l.path
                        .strip_prefix(p.as_str())
                        .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
            })
        };
        let mut lines: Vec<&Line> = lines.iter().collect();
        lines.sort_by_key(|l| !forced(l));
        for (i, l) in lines.iter().copied().enumerate() {
            if i == max {
                rows.push(Row::plain(format!(
                    "{inner}... ({} more changes)",
                    lines.len() - max
                )));
                break;
            }
            let right = match &l.site {
                None => vec![],
                Some(s) => attr_text(d, l, s, self.why),
            };
            write_line(rows, &d.kind, l, &inner, style, self.why, right);
            if self.why == Why::Full {
                write_chain(rows, l, &format!("{inner}  "), self.why);
            }
        }
        for l in &d.kept {
            write_kept(rows, l, &inner, style, self.why);
        }
        // A delete's reason is in its change line's site column (After
        // R-149), a destroy's none: no line under its attributes.
        if let Some(b) = d.because.as_ref().filter(|_| !is_delete(&d.kind)) {
            let plain = format!("{inner}because {b}");
            let painted = format!("{inner}{} {b}", style.paint(Paint::Because, "because"));
            rows.push(Row::new(&plain, painted));
        }
    }
}

/// A row's right column, the longest that fits first: the place, the
/// condition, and (at `full`) the reason; the condition alone last.
fn both(full: bool, at: String, cond: String, reason: &str) -> Vec<String> {
    let mut out = Vec::new();
    for cond in [full.then(|| format!("{cond}  ({reason})")), Some(cond)]
        .into_iter()
        .flatten()
    {
        if !at.is_empty() {
            out.push(format!("{at}  {cond}"));
        }
        out.push(cond);
    }
    out
}

/// A change's bindings as the default level says them (After R-149
/// amendment 5): only those whose value its address does not show as a
/// whole segment (`k3s.agent-3` shows `n = 3`; `private-us-east-1b` shows
/// `zone = "us-east-1b"`, not `n = 1`), each value elided as any long one,
/// at most two and then `…`: `with zone = "us-test-1a", n = 3, …`.
pub(crate) fn terse(with: &[String], addr: &str) -> Option<String> {
    let shown: Vec<String> = with
        .iter()
        .filter(|b| match b.split_once(" = ") {
            Some((_, v)) => !shows(addr, v.trim_matches('"')),
            None => true,
        })
        .map(|b| match b.split_once(" = ") {
            Some((k, v)) => format!("{k} = {}", elide(v)),
            None => b.clone(),
        })
        .collect();
    let mut out = shown.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
    if shown.len() > 2 {
        out.push_str(", …");
    }
    (!out.is_empty()).then(|| format!("with {out}"))
}

/// Whether `addr` shows `v` whole: `v` in it with no letter or digit
/// either side, so a segment (`3` of `agent-3`, `us-east-1b` of
/// `private-us-east-1b`) and not a part of one (`1` of `1b`).
fn shows(addr: &str, v: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    !v.is_empty()
        && addr.match_indices(v).any(|(i, _)| {
            !word(addr[..i].chars().next_back()) && !word(addr[i + v.len()..].chars().next())
        })
}

/// A delete, of the object or of a deposed one.
fn is_delete(k: &ActionKind) -> bool {
    matches!(k, ActionKind::Delete | ActionKind::DeleteDeposed)
}

/// The leaves of `d` with the values `side` gives, as the plan prints
/// them: what a rename guess compares.
fn values(d: &Deformation, side: impl Fn(&Line) -> &Shown) -> BTreeSet<(String, String)> {
    fn walk(ls: &[Line], side: &dyn Fn(&Line) -> &Shown, out: &mut BTreeSet<(String, String)>) {
        for l in ls {
            match l.leaves.is_empty() {
                true => {
                    out.insert((l.path.clone(), side(l).said(Why::Line)));
                }
                false => walk(&l.leaves, side, out),
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&d.lines, &side, &mut out);
    out
}

/// Why a plan deletes `d`, on one line (After R-149, R-150): a create of
/// its type in the same plan with the same values is a rename guess
/// (`renamed?  T b is created with the same values`, `state mv` the fix);
/// a copy whose `use` the program no longer has (`use synapse removed`);
/// else why the program no longer derives it ([`crate::why::not::gone`]),
/// `not in the program` with where the last apply derived it when that is
/// known.
fn gone(
    d: &Deformation,
    created: &[(Address, BTreeSet<(String, String)>)],
    instances: &crate::zset::Instances,
    live: &crate::zset::Instances,
    res: &EvalResult,
    r: &Redactor,
) -> Option<(Option<String>, String)> {
    let mine = values(d, |l| &l.before);
    if !mine.is_empty()
        && let Some((a, _)) = created
            .iter()
            .find(|(a, vs)| a.typ == d.addr.typ && *vs == mine)
    {
        return Some((
            None,
            format!(
                "renamed?  {} is created with the same values",
                reference(a, "")
            ),
        ));
    }
    let wanted = live.enclosing(&d.addr);
    if let Some(copy) = instances
        .enclosing(&d.addr)
        .into_iter()
        .find(|c| !wanted.contains(c))
    {
        return Some((None, format!("use {} removed", copy.name)));
    }
    crate::why::not::gone(&d.addr.typ, &d.addr.name, res, r)
}

/// A delete's site column (After R-149): where the last apply derived
/// it (else where the rule that would is), then why it is gone, the
/// leaf the last apply rests on that is false now when the plan knows it
/// (`because`), else the program's why-not; `not in the program  (was
/// SITE)`; a rename guess alone. The reason alone when that is too
/// wide, else the site;
/// from `-v`, the site with the bindings the last apply derived it with.
fn gone_column(d: &Deformation, how: Why) -> Vec<String> {
    let Some((rule_at, why)) = &d.gone else {
        return d
            .site
            .iter()
            .map(|s| s.at.clone())
            .filter(|a| !a.is_empty())
            .collect();
    };
    // From `-v`, the bindings it was derived with.
    let at = d
        .site
        .as_ref()
        .map(|s| place_text(s, how))
        .filter(|a| !a.is_empty())
        .or_else(|| rule_at.clone());
    if why.starts_with("renamed?") {
        return vec![why.clone()];
    }
    let why = d.because.clone().unwrap_or_else(|| why.clone());
    match at {
        Some(at) if why == crate::why::not::NOT_IN_PROGRAM => {
            vec![format!("{why}  (was {at})"), why]
        }
        Some(at) => vec![format!("{at}  {why}"), why, at],
        None => vec![why],
    }
}

/// The site column of attribute line `l` of change `d`, written at `s`,
/// at level `why` (R-111). By default a value written in its own block
/// says nothing, any other its place. From `-v`, a create's value
/// written in its own block is the entry's expression when it reads
/// something the value does not show; anything else is the statement
/// that wrote it; either with the writes it won over.
fn attr_text(d: &Deformation, l: &Line, s: &Site, why: Why) -> Vec<String> {
    if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
        return vec![];
    }
    let texts = attr_texts(d, l, s, why);
    // An expression written into a secret says no literal (R-124
    // amendment 2).
    match (&l.after, &l.before) {
        (Shown::Sensitive(_), _) | (_, Shown::Sensitive(_)) => {
            texts.iter().map(|t| masked_text(t)).collect()
        }
        _ => texts,
    }
}

fn attr_texts(d: &Deformation, l: &Line, s: &Site, why: Why) -> Vec<String> {
    let own = d.site.as_ref().is_some_and(|h| match (&h.stmt, &s.stmt) {
        (Some((hf, first)), Some((sf, line))) => {
            hf == sf && (first == line || (first..=&h.last).contains(&line))
        }
        _ => false,
    });
    if why == Why::Line {
        return match own {
            true => vec![],
            false => vec![place_text(s, why)],
        };
    }
    if own && matches!(d.kind, ActionKind::Create | ActionKind::Adopt) {
        let after = l.after.said(why);
        let rhs = s
            .entry
            .as_deref()
            .and_then(|e| e.split_once(" = "))
            .map(|(_, rhs)| rhs.to_string())
            // A fold is the value the entry wrote, laid out.
            .filter(|_| l.value.is_none())
            .filter(|rhs| {
                // A variable is the entry's binding, on the line above; a
                // literal is the value itself; a secret says so already.
                !rhs.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !rhs.starts_with("(sensitive")
                    && reads(rhs)
                    && !after.contains(rhs.as_str())
            });
        let beat = beat_text(s, d);
        return match rhs {
            Some(rhs) if !beat.is_empty() => vec![format!("{rhs}{beat}"), rhs],
            Some(rhs) => vec![rhs],
            None if !beat.is_empty() => vec![beat.trim_start().to_string()],
            None => vec![],
        };
    }
    written_text(s, d)
}

/// Whether expression `e` reads anything: a name that is not an object's
/// key (a variable, a function), or an interpolation. `{ team: "a" }`
/// reads nothing; `db.name`, `io.read("f.json")` and `"shop-${env}"` do.
fn reads(e: &str) -> bool {
    let mut cs = e.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '"' {
            let mut esc = false;
            let mut prev = ' ';
            for c in cs.by_ref() {
                if prev == '$' && c == '{' && !esc {
                    return true;
                }
                if c == '"' && !esc {
                    break;
                }
                esc = c == '\\' && !esc;
                prev = c;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut word = String::from(c);
            while let Some(&n) = cs.peek()
                && (n.is_alphanumeric() || "_.".contains(n))
            {
                word.push(n);
                cs.next();
            }
            while cs.peek().is_some_and(|n| n.is_whitespace()) {
                cs.next();
            }
            let key = cs.peek() == Some(&':');
            if !key && !matches!(word.as_str(), "true" | "false" | "null") {
                return true;
            }
        }
    }
    false
}

/// The chain of change path `path`'s value ([`attr_site`]'s fact).
fn attr_chain(
    p: &tree::Printer,
    rules: &[RuleStmt],
    facts: &[&Atom],
    path: &str,
    stack_keys: &BTreeSet<String>,
) -> Vec<tree::Step> {
    match attr_holding(facts, path) {
        Some((a, keys, true)) => p.attr_chain(rules, a, &keys, stack_keys),
        _ => Vec::new(),
    }
}

/// The attribute fact of `facts` that holds change path `path`, the keys
/// below it, and whether they reach the leaf.
fn attr_holding<'a>(facts: &[&'a Atom], path: &str) -> Option<(&'a Atom, Vec<String>, bool)> {
    let segs = crate::ir::path_segments(path);
    for k in (1..=segs.len()).rev() {
        let last = segs[k - 1];
        let index = crate::ir::segment_parts(last).1;
        // The segments are `path`'s own, joined by dots: the prefix to
        // `last`, its index left out, is a slice of it.
        let start = last.as_ptr() as usize - path.as_ptr() as usize;
        let prefix = &path[..start + last.len() - index.len()];
        let found = facts
            .iter()
            .find(|a| matches!(a.args.get(2), Some(Term::Val(Value::Str(p))) if p == prefix));
        if let Some(a) = found {
            let keys: Vec<String> = match index.is_empty() {
                true => segs[k..]
                    .iter()
                    .take_while(|s| crate::ir::segment_parts(s).1.is_empty())
                    .map(|s| crate::ir::segment_key(s).into_owned())
                    .collect(),
                false => vec![],
            };
            let whole = index.is_empty() && keys.len() == segs.len() - k;
            return Some((*a, keys, whole));
        }
    }
    None
}

/// Site `at` (`FILE:LINE`) relative to `top`, the project's root; `None`
/// when it is not under it, or not a file's.
pub fn relative_place(at: &str, top: &std::path::Path) -> Option<String> {
    let prefix = format!("{}/", top.display());
    let (file, line) = at.rsplit_once(':')?;
    if file.starts_with('<') {
        return None;
    }
    let abs = std::path::absolute(file).ok()?;
    let rest = abs.to_str()?.strip_prefix(&prefix)?;
    Some(format!("{rest}:{line}"))
}

/// One tick of the report.
#[derive(Default)]
struct Section<'a> {
    changes: Vec<&'a Deformation>,
    waits: BTreeSet<String>,
    deposed: Vec<&'a Address>,
    /// The rules that may derive an unknown number of resources once
    /// the tick before has run (R-156).
    groups: Vec<&'a Group>,
    /// The settings of the providers whose connection an earlier tick
    /// makes, which planned its changes against their offline schemas
    /// (R-193), as `later`'s provisional block says them.
    provisional: Vec<String>,
}

/// `moved OLD -> NEW`, one line per rename `moved/3` applied to state.
pub fn moved_text(moves: &[(Address, Address)]) -> String {
    moves
        .iter()
        .map(|(old, new)| format!("moved {old} -> {new}\n"))
        .collect()
}

/// A value given at creation only that differs from the object's (R-198):
/// `user_data differs (bootstrap): kept`; from `-v` the two values, the
/// object's first.
fn write_kept(rows: &mut Vec<Row>, l: &Line, indent: &str, style: Style, why: Why) {
    let (plain, painted) = match why >= Why::How {
        true => (
            format!(
                "{indent}{} = {} → {}  {KEPT}",
                l.path,
                l.before.said(why),
                l.after.said(why)
            ),
            format!(
                "{indent}{} = {} → {}  {}",
                l.path,
                style.said(&l.before, why),
                style.said(&l.after, why),
                style.note(KEPT)
            ),
        ),
        false => (
            format!("{indent}{} differs {KEPT}", l.path),
            format!("{indent}{} differs {}", l.path, style.note(KEPT)),
        ),
    };
    rows.push(Row::new(&plain, painted));
}

fn write_line(
    rows: &mut Vec<Row>,
    kind: &ActionKind,
    l: &Line,
    indent: &str,
    style: Style,
    why: Why,
    right: Vec<String>,
) {
    let mut push = |plain: String, painted: String, right: Vec<String>| {
        rows.push(Row::new(&plain, painted).with(right))
    };
    let plain = |v: &Shown| v.said(why);
    let painted = |v: &Shown| style.said(v, why);
    match l.op {
        Op::Add | Op::Remove => {
            let (sign, paint, v) = if l.op == Op::Add {
                ("+", Paint::Create, &l.after)
            } else {
                ("-", Paint::Delete, &l.before)
            };
            let painted_sign = style.paint(paint, sign);
            // A scalar element of a keyless set is named by itself
            // (R-158): `- policies[app_policy]`.
            if l.leaves.is_empty()
                && let Some(list) = l.path.strip_suffix("[]")
            {
                let (plain, painted) = (plain(v), painted(v));
                push(
                    format!("{indent}{sign} {list}[{plain}]"),
                    format!("{indent}{painted_sign} {list}[{painted}]"),
                    right,
                );
                return;
            }
            if l.leaves.is_empty() {
                push(
                    format!("{indent}{sign} {} = {}", l.path, plain(v)),
                    format!("{indent}{painted_sign} {} = {}", l.path, painted(v)),
                    right,
                );
                return;
            }
            push(
                format!("{indent}{sign} {}", l.path),
                format!("{indent}{painted_sign} {}", l.path),
                right,
            );
            let inner = if l.op == Op::Add {
                ActionKind::Create
            } else {
                ActionKind::Delete
            };
            for x in &l.leaves {
                write_line(
                    rows,
                    &inner,
                    x,
                    &format!("{indent}    "),
                    style,
                    why,
                    vec![],
                );
            }
        }
        // A document value (R-131): its row; a value body's, the
        // resource's (`= vendor/crds.yml:412  (24.0 KB)`).
        Op::Leaf if let Some(row) = &l.row => {
            let text = match l.path.is_empty() {
                true => format!("{indent}= {row}"),
                false => format!("{indent}{} = {row}", l.path),
            };
            push(text.clone(), text, right);
        }
        Op::Leaf if l.value.is_some() => {
            let head = format!("{} = ", l.path);
            let width = WIDTH.saturating_sub(indent.chars().count());
            let tree = l.value.as_ref().expect("a fold has its value");
            for (i, text) in crate::fmt::value::layout(&head, tree, width)
                .into_iter()
                .enumerate()
            {
                let row = format!("{indent}{text}");
                let painted = style.notes_in(&row);
                match i {
                    0 => push(row, painted, right.clone()),
                    _ => push(row, painted, vec![]),
                }
            }
        }
        // An element named by itself says no value (R-158).
        Op::Leaf
            if matches!(kind, ActionKind::Create | ActionKind::Adopt)
                && scalar(&l.after)
                && l.path.ends_with(&format!("[{}]", plain(&l.after))) =>
        {
            let text = format!("{indent}{}", l.path);
            push(text.clone(), text, right);
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => push(
                format!("{indent}{} = {}", l.path, plain(&l.after)),
                format!("{indent}{} = {}", l.path, painted(&l.after)),
                right,
            ),
            ActionKind::Delete | ActionKind::DeleteDeposed => push(
                format!("{indent}{} = {}", l.path, plain(&l.before)),
                format!("{indent}{} = {}", l.path, painted(&l.before)),
                right,
            ),
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => push(
                format!(
                    "{indent}{} = {} → {}",
                    l.path,
                    plain(&l.before),
                    plain(&l.after)
                ),
                format!(
                    "{indent}{} = {} → {}",
                    l.path,
                    painted(&l.before),
                    painted(&l.after)
                ),
                right,
            ),
            ActionKind::Noop | ActionKind::Forget => {}
        },
    }
}

#[cfg(test)]
mod tests;
