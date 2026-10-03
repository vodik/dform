//! Stuck instances (proposal E §2.7 Rules 2 and 3, as F DR-2 revises them)
//! and the plan's phase sections.
//!
//! A rule instance whose body needs a null's *content* (a content position,
//! Rule 2) does not fire: the evaluator records it as a [`Stuck`] and derives
//! `stuck(RuleId, HeadPattern, Bindings, Nulls)`. The head pattern is the
//! rule's head with the bound key columns instantiated and everything else a
//! wildcard.
//!
//! A positive read of an ordinary predicate with stuck instances is not
//! undetermined: the facts are not there yet, which is monotone. The rule
//! may derive after the boundary that resolves the nulls ([`MayDerive`]):
//! a resource rule is a pending group, a deny may derive after tick N.
//! May-derive is transitive through positive reads. Rule 3 is decided per
//! key against the stuck heads and the may-derive heads alike:
//!
//! * `not p(t)` is undetermined iff `p(t)` unifies with such a head of `p`;
//! * an aggregate group is undetermined iff its key unifies with such a head
//!   of a body predicate, or with a stuck instance of the aggregate's own
//!   rule; `attr` is the aggregate over `arg`, per `(T, A, P)` group;
//! * a positive reader of an undetermined aggregate is undetermined.

use crate::ast::{Atom, Lit, Term};
use crate::lattice::{Truth, eq3, nulls_in};
use crate::partition::fmt_atom;
use crate::schema::Schema;
use crate::value::{NullClass, Value, null_owner};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A rule instance that did not fire because it needs a null's content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stuck {
    /// Index of the rule in the evaluated program (constraints follow the
    /// rules); `None` for an attribute group the aggregate could not decide.
    pub rule: Option<usize>,
    /// The head with bound columns instantiated, the rest wildcards.
    pub head: Atom,
    pub bindings: BTreeMap<String, Value>,
    pub nulls: BTreeSet<String>,
    pub reason: String,
    /// The rule's text, for the plan.
    pub text: String,
}

impl Stuck {
    pub fn nulls_text(&self) -> String {
        self.nulls
            .iter()
            .map(|n| format!("?{}", crate::ir::label(n)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `stuck(RuleId, HeadPattern, Bindings, Nulls)`: the derived relation.
    pub fn fact(&self) -> Atom {
        let v = |v: Value| Term::Val(v);
        Atom {
            pred: "stuck".into(),
            args: vec![
                v(self
                    .rule
                    .map(|r| Value::Int(r as i64))
                    .unwrap_or(Value::Int(-1))),
                v(Value::Str(fmt_atom(&self.head))),
                v(Value::Obj(self.bindings.clone().into_iter().collect())),
                v(Value::List(
                    self.nulls.iter().cloned().map(Value::Str).collect(),
                )),
            ],
            record: None,
            span: Default::default(),
        }
    }
}

/// `may_derive(RuleId, HeadPattern, Nulls)`: the [`MayDerive`] instances of
/// resource rules (pending groups) in the plan's evaluation, given to its
/// policy pass.
pub const MAY_DERIVE: &str = "may_derive";

/// A rule instance that may derive after a boundary (F DR-2 revised, last
/// clause, for every rule): its body positively reads a predicate with a
/// stuck instance, or with one that may derive, that unifies with the read
/// under the bindings of the literals before it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MayDerive {
    /// Index of the rule in the evaluated program (constraints follow).
    pub rule: usize,
    /// The head with the bound columns instantiated, the rest wildcards.
    pub head: Atom,
    pub nulls: BTreeSet<String>,
    /// The stuck head read, as the body reads it.
    pub reads: Atom,
    /// Whether `reads` is itself only a may-derive head (transitively).
    pub transitive: bool,
}

impl MayDerive {
    pub fn reason(&self) -> String {
        let what = if self.transitive {
            "which may derive after a boundary"
        } else {
            "which is stuck"
        };
        let reads = crate::modules::private_text(&self.reads, &fmt_atom, " ")
            .unwrap_or_else(|| fmt_atom(&self.reads));
        format!("reads {reads}, {what}")
    }

    /// `may_derive(RuleId, HeadPattern, Nulls)`, as the policy pass reads it.
    pub fn fact(&self) -> Atom {
        let v = |v: Value| Term::Val(v);
        Atom {
            pred: MAY_DERIVE.into(),
            args: vec![
                v(Value::Int(self.rule as i64)),
                v(Value::Str(fmt_atom(&self.head))),
                v(Value::List(
                    self.nulls.iter().cloned().map(Value::Str).collect(),
                )),
            ],
            record: None,
            span: Default::default(),
        }
    }

    pub fn nulls_text(&self) -> String {
        self.nulls
            .iter()
            .map(|n| format!("?{}", crate::ir::label(n)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Can an instance of this rule (or constraint body under a `deny` head)
/// be stuck? Only when it has a content position (Rule 2) or reads
/// something Rule 3 can leave undetermined: a literal other than a
/// relation read, a relation read that compares a column (a constant, a
/// variable bound before or earlier in it, a function, list or object
/// pattern), a read of an aggregate (`aggregates`), an aggregate head, a
/// resource address in the head, or a builtin in the head. A rule that
/// only reads relations into fresh variables and copies them into its
/// head cannot: `stuck/4`'s readers must be such rules.
pub fn can_stick(head: &Atom, body: &[Lit], aggregates: &BTreeSet<String>) -> bool {
    fn has_func(t: &Term) -> bool {
        match t {
            Term::Func { .. } | Term::ListComp { .. } => true,
            Term::List(xs) => xs.iter().any(has_func),
            Term::Obj(m) => m.values().any(has_func),
            _ => false,
        }
    }
    if crate::zset::address_arg(head).is_some() {
        return true;
    }
    if head.args.iter().any(has_func) {
        return true;
    }
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    for lit in body {
        let Lit::Pos(a) = lit else {
            return true;
        };
        if aggregates.contains(&a.pred)
            || matches!(a.pred.as_str(), "member" | "enumerate")
            || crate::ir::ops::is_builtin_pred(&a.pred)
        {
            return true;
        }
        for t in &a.args {
            match t {
                Term::Var(x) if !bound.insert(x) => return true,
                Term::Var(_) | Term::Wildcard => {}
                _ => return true,
            }
        }
    }
    false
}

/// Any null at all.
pub fn has_null(v: &Value) -> bool {
    !nulls_in(v).is_empty()
}

/// An open or secret null: content unknown even under the Unique Name
/// Assumption.
pub fn has_open_or_secret(v: &Value) -> bool {
    match v {
        Value::Null { class, .. } => *class != NullClass::Fresh,
        Value::List(xs) => xs.iter().any(has_open_or_secret),
        Value::Obj(m) => m.values().any(has_open_or_secret),
        _ => false,
    }
}

/// The head with its bindings applied: a bound, null-free term becomes its
/// value; anything else (an unbound variable, a null, an expression that
/// cannot be evaluated yet) a wildcard. `eval` evaluates a ground term.
pub fn head_pattern(
    head: &Atom,
    bindings: &HashMap<String, Value>,
    eval: impl Fn(&Term, &HashMap<String, Value>) -> Option<Value>,
) -> Atom {
    let args = head
        .args
        .iter()
        .map(|t| match eval(t, bindings) {
            Some(v) if !has_null(&v) => Term::Val(v),
            _ => Term::Wildcard,
        })
        .collect();
    let pat = Atom {
        pred: head.pred.clone(),
        args,
        record: None,
        span: Default::default(),
    };
    normalize_contribution(pat)
}

/// A contribution pattern `arg(T, A, P, V, R)` with a constant dotted path
/// is keyed by its top-level attribute, as the aggregate groups it.
fn normalize_contribution(mut pat: Atom) -> Atom {
    if pat.pred == "arg"
        && pat.args.len() == 5
        && let (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) = (&pat.args[0], &pat.args[2])
    {
        let (p, _) = crate::transform::normalize_contribution(t, p, Term::Wildcard);
        pat.args[2] = Term::Val(Value::Str(p));
        pat.args[3] = Term::Wildcard;
    }
    pat
}

/// How a body reads a stuck head: bodies read `attr/4`, never `arg/5`.
pub fn as_read(p: &Atom) -> Atom {
    if p.pred == "arg" && p.args.len() == 5 {
        return Atom {
            pred: "attr".into(),
            args: p.args[..4].to_vec(),
            record: None,
            span: Default::default(),
        };
    }
    p.clone()
}

/// Do two patterns (wildcards on either side) unify? Unknown equality
/// counts as unifying: the conservative answer.
pub fn patterns_unify(a: &Atom, b: &Atom) -> bool {
    if a.pred != b.pred || a.args.len() != b.args.len() {
        return false;
    }
    a.args.iter().zip(&b.args).all(|(p, q)| match (p, q) {
        (Term::Val(x), Term::Val(y)) => eq3(x, y) != Truth::False,
        _ => true,
    })
}

/// Stuck and may-derive head patterns in the form bodies read them, by
/// predicate.
#[derive(Debug, Default, Clone)]
pub struct Known {
    by_pred: BTreeMap<String, Heads>,
}

/// One predicate's stuck heads, in the order found, with those whose first
/// column is a null-free constant indexed by it.
#[derive(Debug, Default, Clone)]
struct Heads {
    all: Vec<(Atom, BTreeSet<String>)>,
    by_first: HashMap<Value, Vec<usize>>,
    /// Heads whose first column is not a null-free constant.
    open_first: Vec<usize>,
}

/// A pattern's first column, when it is a constant without a null.
fn first_const(a: &Atom) -> Option<&Value> {
    match a.args.first() {
        Some(Term::Val(v)) if !has_null(v) => Some(v),
        _ => None,
    }
}

impl Heads {
    /// The heads that can unify with `pat`, in the order found: a head
    /// with another constant first column cannot.
    fn candidates<'a>(
        &'a self,
        pat: &Atom,
    ) -> Box<dyn Iterator<Item = &'a (Atom, BTreeSet<String>)> + 'a> {
        match first_const(pat) {
            Some(v) => {
                let mut ids: Vec<usize> = self.by_first.get(v).cloned().unwrap_or_default();
                ids.extend(&self.open_first);
                ids.sort_unstable();
                Box::new(ids.into_iter().map(|i| &self.all[i]))
            }
            None => Box::new(self.all.iter()),
        }
    }
}

impl Known {
    pub fn add(&mut self, s: &Stuck) {
        self.add_head(&s.head, &s.nulls);
    }

    /// A head (in its derived form) with the nulls it waits on.
    pub fn add_head(&mut self, head: &Atom, nulls: &BTreeSet<String>) {
        let read = as_read(head);
        let heads = self.by_pred.entry(read.pred.clone()).or_default();
        let i = heads.all.len();
        match first_const(&read) {
            Some(v) => heads.by_first.entry(v.clone()).or_default().push(i),
            None => heads.open_first.push(i),
        }
        heads.all.push((read, nulls.clone()));
    }

    /// No head at all?
    pub fn is_empty(&self) -> bool {
        self.by_pred.is_empty()
    }

    /// Has `pred` any head at all?
    pub fn has_pred(&self, pred: &str) -> bool {
        self.by_pred.contains_key(pred)
    }

    /// The nulls of every stuck head of `pat.pred` that unifies with `pat`;
    /// empty when none does.
    pub fn blocking(&self, pat: &Atom) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        if let Some(heads) = self.by_pred.get(&pat.pred) {
            for (p, nulls) in heads.candidates(pat) {
                if patterns_unify(p, pat) {
                    out.extend(nulls.iter().cloned());
                    if nulls.is_empty() {
                        out.insert(String::new());
                    }
                }
            }
        }
        out.remove("");
        out
    }

    /// Every stuck head of `pat.pred` that unifies with `pat`, with its nulls.
    pub fn matching(&self, pat: &Atom) -> Vec<(Atom, BTreeSet<String>)> {
        self.by_pred
            .get(&pat.pred)
            .into_iter()
            .flat_map(|h| h.candidates(pat))
            .filter(|(p, _)| patterns_unify(p, pat))
            .cloned()
            .collect()
    }

    pub fn any(&self, pat: &Atom) -> bool {
        self.by_pred
            .get(&pat.pred)
            .is_some_and(|h| h.candidates(pat).any(|(p, _)| patterns_unify(p, pat)))
    }
}

/// The phase sections of E §2.7 over one evaluation: which resources are
/// definite, which wait on a boundary, the stuck resource rules (pending
/// groups), and the undetermined policies.
#[derive(Debug, Clone, Default)]
pub struct Sections {
    /// Nulls named by stuck instances and stuck attribute cells: resolving
    /// one needs a re-evaluation boundary.
    pub blocking: BTreeSet<String>,
    /// Resources held until a boundary, with the nulls they wait on.
    pub pending: BTreeMap<(String, String), BTreeSet<String>>,
    pub pending_groups: Vec<String>,
    pub undetermined: Vec<String>,
}

/// E §2.7 phase assignment. A resource is held when its document carries a
/// null owned by a resource whose Apply resolves a blocking null (or by a
/// held resource), when its provider's configuration does, or when one of
/// its attribute cells is stuck. A document's own computed nulls are not
/// edges (F5): `docs` is assembled without them. A stuck resource rule is a
/// pending group, and so is one that may derive after a boundary.
pub fn sections(
    stuck: &[Stuck],
    may_derive: &[MayDerive],
    facts: &BTreeSet<Atom>,
    docs: &BTreeMap<(String, String), Value>,
    schema: &Schema,
) -> Sections {
    let mut blocking: BTreeSet<String> = stuck.iter().flat_map(|s| s.nulls.clone()).collect();
    let mut stuck_cells: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "attr_stuck") {
        let (Term::Val(Value::Str(t)), Term::Val(n), Term::Val(Value::List(ls))) =
            (&a.args[0], &a.args[1], &a.args[3])
        else {
            continue;
        };
        let nulls: BTreeSet<String> = ls
            .iter()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect();
        blocking.extend(nulls.iter().cloned());
        stuck_cells
            .entry((t.clone(), crate::partition::fmt_bare(n)))
            .or_default()
            .extend(nulls);
    }
    // A stuck contribution or undecided cell for an address: the resource
    // has no complete document yet.
    for st in stuck {
        let read = as_read(&st.head);
        if read.pred != "attr" || read.args.len() != 4 {
            continue;
        }
        // The documents the pattern can name: one when its type and
        // address are constants, the type's when only the type is.
        let named: Box<dyn Iterator<Item = &(String, String)>> =
            match (&read.args[0], &read.args[1]) {
                (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) => Box::new(
                    docs.get_key_value(&(t.clone(), a.clone()))
                        .map(|(k, _)| k)
                        .into_iter(),
                ),
                (Term::Val(Value::Str(t)), _) => Box::new(
                    docs.range((t.clone(), String::new())..)
                        .map(|(k, _)| k)
                        .take_while(move |(tt, _)| tt == t),
                ),
                _ => Box::new(docs.keys()),
            };
        for (t, a) in named {
            let group = Atom {
                pred: "attr".into(),
                args: vec![
                    Term::Val(Value::Str(t.clone())),
                    Term::Val(Value::Str(a.clone())),
                    Term::Wildcard,
                    Term::Wildcard,
                ],
                record: None,
                span: Default::default(),
            };
            if patterns_unify(&read, &group) {
                stuck_cells
                    .entry((t.clone(), a.clone()))
                    .or_default()
                    .extend(st.nulls.iter().cloned());
            }
        }
    }
    let boundary: BTreeSet<(String, String)> =
        blocking.iter().filter_map(|l| null_owner(l)).collect();

    let mut provider_nulls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "provider_config") {
        if let [Term::Val(Value::Str(p)), Term::Val(v)] = a.args.as_slice() {
            provider_nulls
                .entry(p.clone())
                .or_default()
                .extend(nulls_in(v));
        }
    }

    let mut pending: BTreeMap<(String, String), BTreeSet<String>> = stuck_cells;
    loop {
        let mut changed = false;
        for (addr, doc) in docs {
            if pending.contains_key(addr) {
                continue;
            }
            let prov = schema
                .provider_of
                .get(&addr.0)
                .and_then(|p| provider_nulls.get(p))
                .cloned()
                .unwrap_or_default();
            let waits: BTreeSet<String> = nulls_in(doc)
                .into_iter()
                .chain(prov)
                .filter(|l| {
                    null_owner(l).is_some_and(|o| {
                        &o != addr && (boundary.contains(&o) || pending.contains_key(&o))
                    })
                })
                .chain(other_stacks(doc))
                .collect();
            if !waits.is_empty() {
                pending.insert(addr.clone(), waits);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut pending_groups = Vec::new();
    let mut undetermined = Vec::new();
    for s in stuck {
        match s.head.pred.as_str() {
            "want" => pending_groups.push(format!(
                "{} x unknown, on {}  ({})",
                fmt_atom(&s.head),
                s.nulls_text(),
                s.reason
            )),
            "deny" => {
                let msg = match s.head.args.first() {
                    Some(Term::Val(Value::Str(m))) => m.clone(),
                    _ => fmt_atom(&s.head),
                };
                undetermined.push(format!(
                    "deny \"{msg}\" on {}  ({})",
                    s.nulls_text(),
                    s.reason
                ));
            }
            _ => {}
        }
    }
    for m in may_derive.iter().filter(|m| m.head.pred == "want") {
        pending_groups.push(format!(
            "{} x unknown, on {}  ({})",
            fmt_atom(&m.head),
            m.nulls_text(),
            m.reason()
        ));
    }
    pending_groups.dedup();
    undetermined.sort();
    undetermined.dedup();
    Sections {
        blocking,
        pending,
        pending_groups,
        undetermined,
    }
}

/// The nulls in `v` of another stack's outputs that it has not published
/// yet (`output/NAME#K`, not a secret, which its reader's provider reads
/// where it is held): nothing in this run resolves one.
fn other_stacks(v: &Value) -> BTreeSet<String> {
    match v {
        Value::Null { label, class, .. }
            if *class != crate::value::NullClass::Secret
                && crate::stack::deployment_output(label).is_some() =>
        {
            BTreeSet::from([label.clone()])
        }
        Value::List(xs) => xs.iter().flat_map(other_stacks).collect(),
        Value::Obj(m) => m.values().flat_map(other_stacks).collect(),
        _ => BTreeSet::new(),
    }
}
