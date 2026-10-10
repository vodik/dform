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
//!
//! This file is the report, what it is built from and its building
//! (`report`). The rest, by idea: `deformation` (a change and its lines),
//! `render` and `lines` (the plan as text, one change's text), `json`,
//! `explain` (why, at each level), `fold` (a create's lines folded),
//! `chains` (a value's chain), `waits` (what a held change waits on),
//! `groups` (pending groups), `policy` (the policy block), `errors` (what
//! went wrong), `tally` (the headline), `labels` (how things are named),
//! `mask` (a value as shown), `style`, `layout` (the page), `progress` (the
//! apply's block), `table` and `tree` (the other two printers), `bare`
//! (`--why=none`), `deployments`.

use crate::address::Address;
use crate::ast::Program;
use crate::engine::EvalResult;
use crate::provider::{Action, ActionKind, Plan};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::stuck::Sections;
use crate::value::null_owner;
use deformation::{Refs, deformation};
use errors::diags;
use groups::groups;
use mask::null_class;
use policy::{Denied, deferred, policies};
use std::collections::{BTreeMap, BTreeSet};
use tree::Site;
use waits::{Follow, Resolves, boundary_owners, provisional};

mod bare;
mod chains;
mod deformation;
pub mod deployments;
mod errors;
mod explain;
pub mod fold;
mod groups;
mod json;
mod labels;
mod layout;
mod lines;
mod mask;
pub mod policy;
pub mod progress;
mod render;
mod style;
pub mod table;
mod tally;
pub mod tree;
mod waits;
pub use chains::{ChainItem, chains_text};
pub use deformation::{Deformation, Kept, Line, Op};
pub use errors::{
    CONFLICT, Diag, Failure, Unanswered, Witness, is_conflict, said_of, sites, violation_conflict,
    violation_line, violations,
};
pub use explain::relative_place;
pub use groups::{Group, group_pattern};
pub use labels::{
    address, address_text, attribute, attribute_label, kind_name, label, marker_of, path,
    reference, relation_name, short_id,
};
pub use layout::WIDTH;
pub(crate) use mask::elide;
pub use mask::{Shown, masked, shown, shown_value, surface_in};
pub use policy::Policy;
pub use render::{Node, moved_text};
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

/// A forget's note on its line (R-154).
pub const FORGOTTEN: &str = "  forgotten, kept in the world  (lifecycle retain)";

/// What a line says of a value given at creation only that differs from
/// the object's (R-198): `user_data differs (bootstrap): kept`.
pub const KEPT: &str = "(bootstrap): kept";

/// The note after a value the program did not write and the schema
/// gives (R-217): `protocol: "TCP" (schema default)`, inside the value
/// the program wrote around it or on its own line.
pub const SCHEMA_DEFAULT: &str = "(schema default)";

/// The notes a laid-out value says inside it, each a
/// [`Style::note`](style::Style::note) as every note is, [`KEPT`] too
/// (R-217): one where the value would be, `(sensitive)` and `(sensitive
/// LABEL)`; one after it, [`SCHEMA_DEFAULT`].
pub(crate) const NOTES: [&str; 2] = ["(sensitive", SCHEMA_DEFAULT];

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

    // A wait no null names (a provider's settings, a CRD) is made by what
    // `boundary_owners` says; one nothing in this plan makes is `later`'s.
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
    let tick_of = schedule(i, &definite, &held, &resolves);
    let follow = Follow::new(i, &held, &tick_of);
    let pending = pending_blocks(i, &held, (&r, &refs), &tick_of, &resolves, &follow);

    let groups = groups(i.res, &tick_of, &resolves);
    let not_planned = crate::zset::not_planned(i.res, &r);
    let mut policies = policies(i, &tick_of, &resolves);
    policies.extend(deferred(i.res, &tick_of, &resolves));
    for p in policies.iter_mut().filter(|p| p.after.is_none()) {
        p.until = follow.until(&p.on);
    }
    let policy = policy::lines(i.program, i.res, &policies, &r);

    let (ticks, unscheduled) = ticks(i, &definite, &held, &groups, &tick_of);

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

/// The schedule, by the dependency graph alone (R-156): definite
/// deformations run in this tick; a held one runs after everything it
/// waits on is made, which is the tick after the last of their owners'.
fn schedule(
    i: &Input,
    definite: &[&Action],
    held: &[&Action],
    resolves: &Resolves,
) -> BTreeMap<(String, String), usize> {
    let mut tick_of: BTreeMap<(String, String), usize> = definite
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop))
        .map(|a| ((a.addr.typ.clone(), a.addr.name.clone()), i.tick))
        .collect();
    loop {
        let mut changed = false;
        for a in held {
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
            return tick_of;
        }
    }
}

/// The held deformations by what they wait on, each block with the tick
/// it resolves after, or what it waits on outside this plan.
fn pending_blocks(
    i: &Input,
    held: &[&Action],
    (r, refs): (&Redactor, &Refs),
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
    follow: &Follow,
) -> Vec<PendingBlock> {
    let mut by_nulls: BTreeMap<Vec<String>, Vec<Deformation>> = BTreeMap::new();
    for a in held {
        let on = waits_on(a, i.sections).unwrap_or_default();
        by_nulls
            .entry(on)
            .or_default()
            .push(deformation(a, i.schema, r, refs));
    }
    by_nulls
        .into_iter()
        .map(|(on, deformations)| {
            let resolves_after = resolves(&on, tick_of);
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
        .collect()
}

/// What each tick makes, by address, and what no tick of this plan makes.
fn ticks(
    i: &Input,
    definite: &[&Action],
    held: &[&Action],
    groups: &[Group],
    tick_of: &BTreeMap<(String, String), usize>,
) -> (BTreeMap<usize, Vec<String>>, Vec<String>) {
    let mut ticks: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut unscheduled = Vec::new();
    for a in definite.iter().chain(held) {
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
    for a in definite {
        if matches!(a.kind, ActionKind::Replace { create_first: true }) {
            ticks
                .entry(i.tick + 1)
                .or_default()
                .push(format!("{} (deposed)", a.addr));
        }
    }
    for g in groups {
        match g.resolves_after {
            Some(t) => ticks.entry(t + 1).or_default().push(g.pattern.clone()),
            None => unscheduled.push(g.pattern.clone()),
        }
    }
    (ticks, unscheduled)
}

#[cfg(test)]
mod tests;
