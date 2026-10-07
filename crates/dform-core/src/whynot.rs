//! `dform why X` of what the program does not derive (R-80, R-150: what
//! was `why-not`): why not, for a resource, an attribute or a row.
//! Prior art: Soufflé's explainnegation.
//!
//! The rules whose head could produce the thing are found by unifying it
//! with each head: the type and the name's shape, an interpolated name
//! read backwards (`"private-${z}"` against `"private-us-east-1c"` binds
//! `z`), a copy's scope stripped. With what the address fixes bound, each
//! such rule's body is evaluated left to right against the final fact
//! store, and the first literal no row satisfies is reported: for a
//! relation, the nearest rows that would have (the same relation,
//! differing in the fewest of the columns the literal fixes; up to
//! three); for a comparison, the values it compared; for a row a rule of
//! the program derives (a copy's guard, a relation of its own), why that
//! rule did not, one level further in.
//!
//! The limit is stated rather than papered over: it explains what one
//! rule failed to derive, and says so when no rule mentions the thing at
//! all, naming the nearest address the program does derive; it invents
//! no reason.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::engine::{self, EvalResult};
use crate::query::{self, Redactor};
use crate::report::tree::Printer;
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// The nearest rows printed under a literal no row satisfies.
const NEAREST: usize = 3;
/// How many rules deep a failing row is followed.
const DEPTH: usize = 3;
/// The most ways an address is read against one head (interpolations
/// with several holes split it more than one way).
const SPLITS: usize = 16;

type Env = BTreeMap<String, Value>;

/// What `why PATTERN` prints of what is not derived: an address
/// (`T["A"]`), an attribute (`T["A"].path`) or a relation's row with
/// constants (`zone("x", n)`).
pub fn why_not(pattern: &str, res: &EvalResult, redact: &Redactor) -> Result<String> {
    let atom = match query::address(pattern, true)? {
        Some(query::Query::Body { body, .. }) => match body.as_slice() {
            [Lit::Pos(a)] => a.clone(),
            _ => bail!("why: expected one address, got '{pattern}'"),
        },
        _ => match query::parse(pattern) {
            Ok(query::Query::Body { body, .. }) => match body.as_slice() {
                [Lit::Pos(a)] => a.clone(),
                _ => bail!("why: expected one fact pattern, got '{pattern}'"),
            },
            _ => bail!(
                "why: expected an address such as 'net.subnet[\"private-a\"]' or \
                 'net.subnet[\"private-a\"].cidr', a row such as 'zone(\"us-east-1c\", n)', \
                 or a deny such as 'deny \"MESSAGE\"', got '{pattern}'"
            ),
        },
    };
    let w = WhyNot {
        res,
        redact,
        printer: Printer {
            circuit: &res.circuit,
            redact,
            all: false,
        },
    };
    let mut out = String::new();
    let name = w.name(&atom);
    if !engine::query(&[Lit::Pos(atom.clone())], &res.facts)?.is_empty() {
        return Ok(redact.text(&format!("{name}: it is derived\n")));
    }
    // An attribute of a resource the program does not want: the resource.
    if atom.pred == "attr"
        && let [t, a, ..] = atom.args.as_slice()
    {
        let want = Atom {
            pred: "want".into(),
            args: vec![t.clone(), a.clone()],
            record: None,
            span: Default::default(),
        };
        if engine::query(&[Lit::Pos(want.clone())], &res.facts)?.is_empty() {
            out.push_str(&format!("{name}: {} is not derived\n", w.name(&want)));
            w.explain(&want, "", 0, &mut BTreeSet::new(), &mut out)?;
            return Ok(redact.text(&out));
        }
    }
    w.explain(&atom, "", 0, &mut BTreeSet::new(), &mut out)?;
    Ok(redact.text(&out))
}

/// What the resource of fact `a` (its `want`, or an attribute of it)
/// waits on before a tick plans it: its provider's settings (`waits`,
/// R-110), or a deployment it reads that has not been applied (R-121);
/// as `later` names it.
pub fn waiting(
    a: &Atom,
    res: &EvalResult,
    waits: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    match (a.pred.as_str(), a.args.first()) {
        ("want" | "attr", Some(Term::Val(Value::Str(t)))) => waits(t).or_else(|| unapplied(a, res)),
        _ => None,
    }
}

/// Why the program derives no resource `typ` `name`, on one line (R-120):
/// the deepest condition [`why_not`] names (`input one.namespace is not
/// set`), for the plan's `not planned` rows.
pub fn reason(typ: &str, name: &str, res: &EvalResult, redact: &Redactor) -> Option<String> {
    let want = Atom {
        pred: "want".into(),
        args: vec![
            Term::Val(Value::Str(typ.into())),
            Term::Val(Value::Str(name.into())),
        ],
        record: None,
        span: Default::default(),
    };
    let w = WhyNot {
        res,
        redact,
        printer: Printer {
            circuit: &res.circuit,
            redact,
            all: false,
        },
    };
    let mut out = String::new();
    w.explain(&want, "", 0, &mut BTreeSet::new(), &mut out)
        .ok()?;
    let line = out
        .lines()
        .map(str::trim)
        .rfind(|l| !l.starts_with("nearest: ") && !l.ends_with(" has no rows"))?;
    Some(redact.text(line))
}

/// The edit distance between `a` and `b`, by characters (Levenshtein).
pub fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The deployments not applied yet (R-121) whose outputs the attributes
/// of the resource `atom` names hold, as `later` names them: `stack
/// platform[env=lab]`.
fn unapplied(atom: &Atom, res: &EvalResult) -> Option<String> {
    let [t, a, ..] = atom.args.as_slice() else {
        return None;
    };
    let mut on = BTreeSet::new();
    for f in res.facts.iter().filter(|f| f.pred == "attr") {
        if let [ft, fa, _, Term::Val(v)] = f.args.as_slice()
            && ft == t
            && fa == a
        {
            for l in crate::lattice::nulls_in(v) {
                if let Some((u, n, _)) = crate::value::null_parts(&l)
                    && u == crate::stack::UNAPPLIED
                {
                    on.insert(n);
                }
            }
        }
    }
    let on: Vec<String> = on.into_iter().map(|n| format!("stack {n}")).collect();
    (!on.is_empty()).then(|| on.join(", "))
}

struct WhyNot<'a> {
    res: &'a EvalResult,
    redact: &'a Redactor,
    printer: Printer<'a>,
}

/// One rule's attempt: the seed the head bound, the index of the first
/// literal that failed (`None`: every literal held), and the bindings
/// every answer of the literals before it agrees on.
struct Attempt {
    seed: Env,
    failed: Option<usize>,
    known: Env,
}

impl WhyNot<'_> {
    /// Explain why `atom` (ground where the pattern fixed it) is not
    /// derived, into `out` at `pad`.
    fn explain(
        &self,
        atom: &Atom,
        pad: &str,
        depth: usize,
        seen: &mut BTreeSet<String>,
        out: &mut String,
    ) -> Result<()> {
        let rules = &self.res.rules;
        let candidates: Vec<(usize, &RuleStmt, Vec<Env>)> = rules
            .iter()
            .enumerate()
            .filter(|(_, r)| head_matches(&r.head, atom))
            .filter_map(|(i, r)| {
                let seeds = unify_head(&r.head, atom, &r.body);
                (!seeds.is_empty()).then_some((i, r, seeds))
            })
            .collect();
        if depth == 0 && !candidates.is_empty() {
            out.push_str(&format!("{}: no rule derives it\n", self.name(atom)));
        }
        if candidates.is_empty() {
            let line = match atom.pred.as_str() {
                "want" => match atom.args.first() {
                    Some(Term::Val(Value::Str(t))) => format!(
                        "no rule derives {}: no resource {t} is named like it",
                        self.name(atom)
                    ),
                    _ => format!("no rule derives {}", self.name(atom)),
                },
                "attr" => format!("no rule derives {}: no statement sets it", self.name(atom)),
                p => match self.rows(p, atom.args.len()).is_empty() {
                    true => format!("no rule derives {}: nothing states {p}", self.name(atom)),
                    false => format!("no rule derives {}: no row of {p} is it", self.name(atom)),
                },
            };
            out.push_str(&format!("{pad}{line}\n"));
            if depth == 0 && !matches!(atom.pred.as_str(), "want" | "attr") {
                self.nearest(atom, atom, &format!("{pad}  "), out);
            }
            if depth == 0
                && atom.pred == "want"
                && let Some(near) = self.nearest_address(atom)
            {
                out.push_str(&format!("{pad}  nearest: {near}\n"));
            }
            return Ok(());
        }
        for (i, rule, seeds) in candidates {
            let id = format!("r{i}");
            if !seen.insert(format!("{id} {}", crate::partition::fmt_atom(atom))) {
                continue;
            }
            let mut best: Option<Attempt> = None;
            for seed in seeds {
                let a = attempt(rule, seed, &self.res.facts);
                let further = |a: &Attempt| a.failed.unwrap_or(usize::MAX);
                if best.as_ref().is_none_or(|b| further(&a) > further(b)) {
                    best = Some(a);
                }
            }
            let Some(a) = best else { continue };
            let bindings: Vec<(String, Value)> =
                a.seed.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let site = match self.printer.rule_site(rules, &id, &bindings) {
                Some(s) => {
                    let origin = s
                        .origin
                        .as_ref()
                        .map(|o| format!("   ({o})"))
                        .unwrap_or_default();
                    format!("{}  {}{origin}", s.at, s.statement)
                }
                None => self
                    .res
                    .circuit
                    .rule_text(&id)
                    .unwrap_or(id.as_str())
                    .to_string(),
            };
            out.push_str(&format!("{pad}  {site}\n"));
            let inner = format!("{pad}    ");
            let Some(k) = a.failed else {
                let with = match a.seed.is_empty() || atom.pred == "attr" {
                    true => String::new(),
                    false => format!(" with {}", self.with(&a.seed)),
                };
                let what = match (atom.args.get(2), rule.head.args.get(2)) {
                    (Some(Term::Val(Value::Str(p))), Some(Term::Val(Value::Str(h))))
                        if atom.pred == "attr" && p != h =>
                    {
                        format!("what it writes to {h} has no {}", &p[h.len() + 1..])
                    }
                    _ if atom.pred == "attr" => "it writes another value".to_string(),
                    _ => "it derives another one".to_string(),
                };
                out.push_str(&format!("{inner}every condition holds{with}: {what}\n"));
                continue;
            };
            let lit = &rule.body[k];
            let bound = subst_lit(lit, &a.known);
            match &bound {
                Lit::Pos(b) => {
                    let Lit::Pos(written) = lit else { continue };
                    let derived = rules
                        .iter()
                        .any(|r| r.head.pred == b.pred && r.head.args.len() == b.args.len());
                    let none = match (derived, b.pred.ends_with("::__instance")) {
                        (true, true) => "not made",
                        (true, false) => "not derived",
                        (false, _) => "no row",
                    };
                    out.push_str(&format!("{inner}{}: {none}\n", self.atom_text(b)));
                    let rows = self.rows(&b.pred, b.args.len());
                    if !rows.is_empty() || !derived {
                        self.nearest(b, written, &inner, out);
                    }
                    if derived && depth + 1 < DEPTH {
                        self.explain(b, &inner, depth + 1, seen, out)?;
                    }
                }
                Lit::Not(b) => {
                    let held = engine::query(&[Lit::Pos(b.clone())], &self.res.facts)?;
                    let row = held
                        .first()
                        .and_then(|(_, used)| used.first())
                        .map(|r| format!(": {}", self.atom_text(r)))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "{inner}not {}: the row exists{row}\n",
                        self.atom_text(b)
                    ));
                }
                _ => {
                    let mut vars = BTreeSet::new();
                    lit_vars(lit, &mut vars);
                    let shown: Env = a
                        .known
                        .iter()
                        .filter(|(k, _)| vars.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let with = match shown.is_empty() {
                        true => String::new(),
                        false => format!(", with {}", self.with(&shown)),
                    };
                    out.push_str(&format!("{inner}{}: false{with}\n", self.lit_text(lit)));
                }
            }
        }
        Ok(())
    }

    /// The rows of relation `pred/arity`, in the store's order.
    fn rows(&self, pred: &str, arity: usize) -> Vec<&Atom> {
        self.res
            .facts
            .iter()
            .filter(|f| f.pred == pred && f.args.len() == arity)
            .collect()
    }

    /// The address the program derives whose printed name is nearest
    /// `want`'s (a typo, a type's namespace), when one is near: within a
    /// third of its length in edits.
    fn nearest_address(&self, want: &Atom) -> Option<String> {
        let name = self.name(want);
        self.res
            .facts
            .iter()
            .filter(|f| f.pred == "want")
            .map(|f| self.name(f))
            .map(|n| (edits(&name, &n), n))
                // A `not has` waiting on a value: its helper is stuck, so
                // the negation is undetermined though no row holds.
                if let Some(t) = self.undetermined(rule, &a.known) {
                    out.push_str(&format!("{inner}{}\n", self.redact.text(&t)));
                    continue;
                }
            .filter(|(d, _)| *d > 0 && *d <= name.chars().count() / 3)
            .min()
            .map(|(_, n)| n)
    }

    /// `nearest: ROW, ..`: the rows of `bound`'s relation that differ
    /// from it in the fewest columns it fixes; the columns the literal as
    /// `written` fixes must match, and are not printed. With no such row,
    /// the nearest of all rows, whole.
    fn nearest(&self, bound: &Atom, written: &Atom, pad: &str, out: &mut String) {
        let rows = self.rows(&bound.pred, bound.args.len());
        if rows.is_empty() {
            out.push_str(&format!(
                "{pad}{} has no rows\n",
                self.pred_text(&bound.pred)
            ));
            return;
        }
        let fixed: Vec<Option<&Value>> = bound
                Lit::Pos(b) if b.pred == "__known" => {
                    let line = match known_text(rule, lit, &a.known) {
                        Some(t) => t,
                        None => format!("{}: not known yet", self.atom_text(b)),
                    };
                    out.push_str(&format!("{inner}{}\n", self.redact.text(&line)));
                }
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => Some(v),
                _ => None,
            })
            .collect();
        // An attribute is its resource's and path's: only its value may
        // differ, and it prints as the address it is.
        let attr = bound.pred == "attr" && bound.args.len() == 4;
        let stated: Vec<bool> = written
            .args
            .iter()
            .enumerate()
            .map(|(j, t)| match attr {
                true => j < 3,
                false => matches!(t, Term::Val(_)),
            })
            .collect();
        let value = |t: &Term| match t {
            Term::Val(v) => Some(v.clone()),
            _ => None,
        };
        let distance = |row: &Atom, only: &[bool]| {
            row.args
                .iter()
                .zip(&fixed)
                .enumerate()
                .filter(|(j, (t, f))| {
                    !only.get(*j).copied().unwrap_or(false)
                        && f.is_some_and(|f| value(t).as_ref() != Some(f))
                })
                .count()
        };
        // Rows that agree with every column the literal states.
        let agree: Vec<&Atom> = rows
            .iter()
            .copied()
            .filter(|row| {
                row.args
                    .iter()
                    .zip(&fixed)
                    .zip(&stated)
                    .all(|((t, f), s)| !*s || f.is_none_or(|f| value(t).as_ref() == Some(f)))
            })
            .collect();
        if attr && agree.is_empty() {
            out.push_str(&format!("{pad}{} is not set\n", self.name(bound)));
            return;
        }
        let (pool, hide) = match agree.is_empty() {
    /// The first `not h(..)` of a stuck `rule` whose helper `h` tests a
    /// `has` (`partition::answer_has`), as the
    /// program wrote it: `not has b.status.ready: b.status.ready is not
    /// known yet`.
    fn undetermined(&self, rule: &RuleStmt, known: &Env) -> Option<String> {
        rule.body.iter().find_map(|l| {
            let Lit::Not(b) = l else { return None };
            // The negation is undetermined: the rule is stuck on it.
            if !self
                .res
                .stuck
                .iter()
                .any(|s| s.head.pred == b.pred || s.head.pred == rule.head.pred)
            {
                return None;
            }
            let helper = self.res.rules.iter().find(|r| r.head.pred == b.pred)?;
            let mut env = Env::new();
            for (h, t) in helper.head.args.iter().zip(&b.args) {
                if let (Term::Var(h), Some(v)) = (h, subst(t, known).ground()) {
                    env.insert(h.clone(), v);
                }
            }
            let lit = helper
                .body
                .iter()
                .find(|l| matches!(l, Lit::Pos(a) if a.pred == "__known"))?;
            known_text(helper, lit, &env).map(|t| format!("not {t}"))
        })
    }

            false => (agree, stated.clone()),
            true => (rows, vec![false; stated.len()]),
        };
        let mut ranked: Vec<(usize, usize, &Atom)> = pool
            .into_iter()
            .enumerate()
            .map(|(i, row)| (distance(row, &hide), i, row))
            .collect();
        ranked.sort_by_key(|(d, i, _)| (*d, *i));
        let shown: Vec<String> = ranked
            .iter()
            .take(NEAREST)
            .map(|(_, _, row)| {
                if attr {
                    return self.atom_text(row);
                }
                let cols: Vec<String> = row
                    .args
                    .iter()
                    .zip(&hide)
                    .filter(|(_, h)| !**h)
                    .map(|(t, _)| self.term_text(t))
                    .collect();
                format!("({})", cols.join(", "))
            })
            .collect();
        let more = ranked.len().saturating_sub(NEAREST);
        let more = match more {
            0 => String::new(),
            n => format!(" (and {n} more)"),
        };
        out.push_str(&format!("{pad}nearest: {}{more}\n", shown.join(", ")));
    }

    /// A pattern as the program names it: a resource by its address, an
    /// attribute by the address and its path, a row as written.
    fn name(&self, a: &Atom) -> String {
        let s = |t: &Term| match t {
            Term::Val(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        match (a.pred.as_str(), a.args.as_slice()) {
            ("want", [t, n]) => match (s(t), s(n)) {
                (Some(t), Some(n)) => {
                    crate::report::address(&crate::ir::Address { typ: t, name: n })
                }
                _ => self.atom_text(a),
            },
            ("attr", [t, n, p, _]) => match (s(t), s(n), s(p)) {
                (Some(t), Some(n), Some(p)) => attribute_text(t, n, &p),
                _ => self.atom_text(a),
            },
            _ => self.atom_text(a),
        }
    }

    fn pred_text(&self, pred: &str) -> String {
        pred.to_string()
    }

    /// A body atom as the program would write it: a copy's guard as the
    /// copy, an attribute read by its address, a row with its variables by
    /// the source's names.
    fn atom_text(&self, a: &Atom) -> String {
        if let Some(scope) = a.pred.strip_suffix("::__instance")
            && let [Term::Val(Value::Str(c))] = a.args.as_slice()
        {
            return format!("resource {c} {}", scope.replace("::", "."));
        }
        if let (
            "attr",
            [
                Term::Val(Value::Str(t)),
                Term::Val(Value::Str(n)),
                Term::Val(Value::Str(p)),
                v,
            ],
        ) = (a.pred.as_str(), a.args.as_slice())
        {
            let addr = attribute_text(t.clone(), n.clone(), p);
            return match v {
                Term::Val(v) => format!("{addr} = {}", self.redact.surface(v)),
                _ => addr.to_string(),
            };
        }
        let args: Vec<String> = a.args.iter().map(|t| self.term_text(t)).collect();
        format!("{}({})", self.pred_text(&a.pred), args.join(", "))
    }

    fn term_text(&self, t: &Term) -> String {
        match t {
            Term::Val(v) => self.redact.surface(v),
            Term::Var(v) => source_name(v),
            Term::Wildcard => "_".into(),
            // An interpolated string as written.
            Term::Func { name, args } if name == "format" => match args.split_first() {
                Some((Term::Val(Value::Str(f)), rest)) => {
                    let mut out = String::from("\"");
                    let mut parts = f.split("%s");
                    out.push_str(parts.next().unwrap_or(""));
                    for (p, a) in parts.zip(rest) {
                        out.push_str(&format!("${{{}}}", self.term_text(a)));
                        out.push_str(p);
                    }
                    out.push('"');
                    out
                }
                _ => crate::partition::fmt_term(t),
            },
            Term::Func { name, args } => format!(
                "{name}({})",
                args.iter()
                    .map(|a| self.term_text(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Term::List(xs) => format!(
                "[{}]",
                xs.iter()
                    .map(|a| self.term_text(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            t => crate::partition::fmt_term(t),
        }
    }

    fn lit_text(&self, l: &Lit) -> String {
        let bin = |a: &Term, op: &str, b: &Term| {
            format!("{} {op} {}", self.term_text(a), self.term_text(b))
        };
        match l {
            Lit::Pos(a) => self.atom_text(a),
            Lit::Not(a) => format!("not {}", self.atom_text(a)),
            Lit::Eq(a, b) => bin(a, "==", b),
            Lit::Neq(a, b) => bin(a, "!=", b),
            Lit::Gt(a, b) => bin(a, ">", b),
            Lit::Ge(a, b) => bin(a, ">=", b),
            Lit::Lt(a, b) => bin(a, "<", b),
            Lit::Le(a, b) => bin(a, "<=", b),
        }
    }

    /// `x = v, y = w`, by the source's names.
    fn with(&self, env: &Env) -> String {
        env.iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| format!("{} = {}", source_name(k), self.redact.surface(v)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// An attribute by its address and path; an input's cell as the program
/// names the input, `input one.namespace` of the copy `one` (R-120).
fn attribute_text(typ: String, name: String, p: &str) -> String {
    match typ == crate::modules::INPUT {
        true if name.is_empty() => format!("input {p}"),
        true => format!("input {name}.{p}"),
        false => crate::report::attribute(&crate::ir::Address { typ, name }, p),
    }
}

/// A core variable by the name the source gave it: `AvailabilityZone` is
/// `availability_zone` (`resolve::capitalise`, read backwards).
pub(crate) fn source_name(v: &str) -> String {
    let lead = v.len() - v.trim_start_matches('_').len();
    let mut out = v[..lead].to_string();
    for (i, c) in v[lead..].chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A head that may produce `atom`: the same relation and arity, and for a
/// contribution (`arg/5`) against an attribute (`attr/4`), the same
/// resource and attribute.
fn head_matches(head: &Atom, atom: &Atom) -> bool {
    match (head.pred.as_str(), atom.pred.as_str()) {
        ("arg", "attr") => head.args.len() == 5 && atom.args.len() == 4,
        (h, a) => h == a && head.args.len() == atom.args.len(),
    }
}

/// Every way `head` produces `atom`'s ground columns, each the bindings it
/// takes; none when it cannot. An attribute's path matches a head's that
/// is it or the attribute it is under (`tags` for `tags.team`).
fn unify_head(head: &Atom, atom: &Atom, body: &[Lit]) -> Vec<Env> {
    let mut envs = vec![Env::new()];
    let attr = head.pred == "arg" && atom.pred == "attr";
    for (j, t) in atom.args.iter().enumerate() {
        let Term::Val(v) = t else { continue };
        let h = &head.args[j];
        let wanted: Vec<Value> = match (attr, j, v) {
            // The path: the attribute, or the one it is under.
            (true, 2, Value::Str(p)) => {
                let mut ps = vec![v.clone()];
                let top = crate::ir::path_segments(p)[0].to_string();
                if top != *p {
                    ps.push(Value::Str(top));
                }
                ps
            }
            (true, 3, _) => continue,
            _ => vec![v.clone()],
        };
        let mut next = Vec::new();
        for env in &envs {
            for w in &wanted {
                for e in unify(h, w, body) {
                    if let Some(m) = merge(env, &e) {
                        next.push(m);
                    }
                }
            }
        }
        next.truncate(SPLITS);
        envs = next;
        if envs.is_empty() {
            break;
        }
    }
    envs
}

fn merge(a: &Env, b: &Env) -> Option<Env> {
    let mut out = a.clone();
    for (k, v) in b {
        match out.get(k) {
            Some(w) if w != v => return None,
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Some(out)
/// A `has` the compiler made a `__known` over the value it reads
/// (`partition::answer_identity`, `answer_has`), as the program wrote it,
/// with why it fails: `has warm_cache: warm_cache does not exist yet` of
/// a resource's identity, `has cache.endpoint: cache.endpoint is not
/// known yet` of a computed attribute.
fn known_text(rule: &RuleStmt, lit: &Lit, known: &Env) -> Option<String> {
    let Lit::Pos(k) = lit else { return None };
    let Some(Term::Var(mut v)) = k.args.first().cloned() else {
        return None;
    };
    let identity = v.starts_with("__Identity");
    let mut below: Vec<String> = Vec::new();
    // Back through the walk to the attribute read that binds the value.
    loop {
        let step = rule.body.iter().find_map(|l| match l {
            Lit::Eq(Term::Var(w), Term::Func { name, args }) if *w == v && name == "__path" => {
                match args.as_slice() {
                    [Term::Var(from), p] => Some(Err((from.clone(), p.as_str()?.to_string()))),
                    _ => None,
                }
            }
            Lit::Eq(Term::Var(w), Term::Var(from)) if *w == v => {
                Some(Err((from.clone(), String::new())))
            }
            Lit::Pos(a)
                if a.pred == "attr" && matches!(a.args.get(3), Some(Term::Var(w)) if *w == v) =>
            {
                Some(Ok(a.clone()))
            }
            _ => None,
        })?;
        match step {
            Ok(read) => {
                let val = |t: &Term| match t {
                    Term::Val(Value::Str(s)) => Some(s.clone()),
                    Term::Var(x) => known.get(x).and_then(|v| v.as_str().map(str::to_string)),
                    _ => None,
                };
                let a = crate::ir::Address {
                    typ: val(&read.args[0])?,
                    name: val(&read.args[1])?,
                };
                if identity {
                    let r = crate::report::reference(&a, "");
                    return Some(format!("has {r}: {r} does not exist yet"));
                }
                let mut path = val(&read.args[2])?;
                for p in below.iter().rev().filter(|p| !p.is_empty()) {
                    path.push('.');
                    path.push_str(p);
                }
                let r = crate::report::reference(&a, &path);
                return Some(format!("has {r}: {r} is not known yet"));
            }
            Err((from, p)) => {
                below.push(p);
                v = from;
            }
        }
    }
}

}

/// The bindings under which term `t` (of a head) is `v`: a variable the
/// body defines as a call (`Addr = format("private-%s", Z)`) is read
/// through it; an interpolation backwards, each way it splits; a copy's
/// scope stripped. A call that cannot be read backwards binds nothing
/// (the body says).
fn unify(t: &Term, v: &Value, body: &[Lit]) -> Vec<Env> {
    match t {
        Term::Val(w) => match w == v {
            true => vec![Env::new()],
            false => vec![],
        },
        Term::Wildcard => vec![Env::new()],
        Term::Var(x) => {
            let def = body.iter().find_map(|l| match l {
                Lit::Eq(Term::Var(y), f @ Term::Func { .. })
                | Lit::Eq(f @ Term::Func { .. }, Term::Var(y))
                    if y == x =>
                {
                    Some(f)
                }
                _ => None,
            });
            let own = Env::from([(x.clone(), v.clone())]);
            match def {
                None => vec![own],
                Some(f) => unify(f, v, body)
                    .into_iter()
                    .filter_map(|e| merge(&own, &e))
                    .collect(),
            }
        }
        Term::Func { name, args } if name == "scoped" => {
            let [Term::Val(scope), inner] = args.as_slice() else {
                return vec![Env::new()];
            };
            let Value::Str(s) = v else { return vec![] };
            let prefix = crate::ir::scoped(&crate::functions::value_to_string(scope), "");
            match s.strip_prefix(&prefix) {
                Some(rest) => unify(inner, &Value::Str(rest.to_string()), body),
                None => vec![],
            }
        }
        // A header name's segment (R-112) is its name, quoted or not.
        Term::Func { name, args } if name == crate::ir::NAME_SEGMENT && args.len() == 1 => {
            let Value::Str(s) = v else { return vec![] };
            unify(
                &args[0],
                &Value::Str(crate::ir::segment_key(s).into_owned()),
                body,
            )
        }
        Term::Func { name, args } if name == "format" => {
            let (Some(Term::Val(Value::Str(f))), Value::Str(s)) = (args.first(), v) else {
                return vec![Env::new()];
            };
            let pieces: Vec<&str> = f.split("%s").collect();
            let mut out = Vec::new();
            for parts in splits(s, &pieces) {
                let mut envs = vec![Env::new()];
                for (a, part) in args[1..].iter().zip(&parts) {
                    let mut vals = vec![Value::Str(part.clone())];
                    if let Ok(n) = part.parse::<i64>() {
                        vals.push(Value::Int(n));
                    }
                    let mut next = Vec::new();
                    for env in &envs {
                        for val in &vals {
                            for e in unify(a, val, body) {
                                if let Some(m) = merge(env, &e) {
                                    next.push(m);
                                }
                            }
                        }
                    }
                    envs = next;
                }
                out.extend(envs);
                if out.len() >= SPLITS {
                    break;
                }
            }
            out
        }
        _ => vec![Env::new()],
    }
}

/// Every way `s` reads as the literal `pieces` with a hole between each
/// two: the holes' contents.
fn splits(s: &str, pieces: &[&str]) -> Vec<Vec<String>> {
    let Some((first, rest)) = pieces.split_first() else {
        return vec![];
    };
    let Some(s) = s.strip_prefix(first) else {
        return vec![];
    };
    if rest.is_empty() {
        return match s.is_empty() {
            true => vec![vec![]],
            false => vec![],
        };
    }
    let mut out = Vec::new();
    for (i, _) in s.char_indices().chain([(s.len(), ' ')]) {
        let (hole, tail) = s.split_at(i);
        let mut tails = splits(tail, rest);
        for t in &mut tails {
            t.insert(0, hole.to_string());
        }
        out.extend(tails);
        if out.len() >= SPLITS {
            break;
        }
    }
    out
}

/// Evaluate `rule`'s body with `seed` bound, literal by literal: the
/// first prefix no row satisfies.
fn attempt(rule: &RuleStmt, seed: Env, facts: &BTreeSet<Atom>) -> Attempt {
    let body: Vec<Lit> = rule.body.iter().map(|l| subst_lit(l, &seed)).collect();
    let mut known = seed.clone();
    for k in 0..body.len() {
        let answers = match engine::query(&body[..=k], facts) {
            Ok(a) => a,
            // A literal the engine cannot evaluate this early: the next
            // prefix says.
            Err(_) => continue,
        };
        if answers.is_empty() {
            return Attempt {
                seed,
                failed: Some(k),
                known,
            };
        }
        // What every answer so far agrees on.
        let mut agreed = answers[0].0.clone();
        for (a, _) in &answers[1..] {
            agreed.retain(|k, v| a.get(k) == Some(v));
        }
        known = seed.clone();
        known.extend(agreed);
    }
    Attempt {
        seed,
        failed: None,
        known,
    }
}

pub(crate) fn subst_lit(l: &Lit, env: &Env) -> Lit {
    let t = |x: &Term| subst(x, env);
    let a = |x: &Atom| Atom {
        args: x.args.iter().map(t).collect(),
        ..x.clone()
    };
    match l {
        Lit::Pos(x) => Lit::Pos(a(x)),
        Lit::Not(x) => Lit::Not(a(x)),
        Lit::Eq(x, y) => Lit::Eq(t(x), t(y)),
        Lit::Neq(x, y) => Lit::Neq(t(x), t(y)),
        Lit::Gt(x, y) => Lit::Gt(t(x), t(y)),
        Lit::Ge(x, y) => Lit::Ge(t(x), t(y)),
        Lit::Lt(x, y) => Lit::Lt(t(x), t(y)),
        Lit::Le(x, y) => Lit::Le(t(x), t(y)),
    }
}

pub(crate) fn subst(t: &Term, env: &Env) -> Term {
    match t {
        Term::Var(x) => match env.get(x) {
            Some(v) => Term::Val(v.clone()),
            None => t.clone(),
        },
        Term::Func { name, args } => Term::Func {
            name: name.clone(),
            args: args.iter().map(|a| subst(a, env)).collect(),
        },
        Term::List(xs) => Term::List(xs.iter().map(|a| subst(a, env)).collect()),
        Term::Obj(m) => Term::Obj(m.iter().map(|(k, a)| (k.clone(), subst(a, env))).collect()),
        t => t.clone(),
    }
}

fn lit_vars(l: &Lit, out: &mut BTreeSet<String>) {
    fn term(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(x) => {
                out.insert(x.clone());
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| term(a, out)),
            Term::Obj(m) => m.values().for_each(|a| term(a, out)),
            _ => {}
        }
    }
    match l {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| term(t, out)),
        Lit::Eq(x, y)
        | Lit::Neq(x, y)
        | Lit::Gt(x, y)
        | Lit::Ge(x, y)
        | Lit::Lt(x, y)
        | Lit::Le(x, y) => {
            term(x, out);
            term(y, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interpolation_reads_backwards_every_way_it_splits() {
        assert_eq!(splits("private-a", &["private-", ""]), vec![vec!["a"]]);
        assert_eq!(
            splits("a-to-b-to-c", &["", "-to-", ""]),
            vec![vec!["a", "b-to-c"], vec!["a-to-b", "c"]]
        );
        assert!(splits("public-a", &["private-", ""]).is_empty());
    }

    #[test]
    fn a_core_variable_reads_as_the_source_name() {
        assert_eq!(source_name("AvailabilityZone"), "availability_zone");
        assert_eq!(source_name("Z"), "z");
        assert_eq!(source_name("__v1"), "__v1");
    }
}
