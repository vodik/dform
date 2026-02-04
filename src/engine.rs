use crate::ast::{Atom, Constraint, Lit, Program, RuleStmt, Stmt, Term, Unique};
use crate::merge;
use crate::transform;
use crate::value::Value;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub facts: BTreeSet<Atom>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
enum DerivedFact {
    Normal(Atom),
    Agg(AggFact),
}

#[derive(Debug, Clone)]
struct AggFact {
    pred: String,
    idx: usize,
    key: Vec<Value>,
    atom: Atom,
}

pub fn eval(program: &Program, extra_facts: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    let lowered = transform::lower(program)?;
    let program = lowered.program;
    let uniques = lowered.uniques;
    let mut facts: BTreeSet<Atom> = BTreeSet::new();
    for f in extra_facts {
        facts.insert(ensure_ground(f)?);
    }

    let mut rules = Vec::new();
    let mut constraints = Vec::new();

    for stmt in &program.statements {
        match stmt {
            Stmt::Fact(a) => {
                facts.insert(ensure_ground(a)?);
            }
            Stmt::Rule(r) => rules.push(r.clone()),
            Stmt::Constraint(c) => constraints.push(c.clone()),
            _ => {
                // Lowered program should contain only facts/rules/constraints.
            }
        }
    }

    // Stratified evaluation (so defaults via `not` behave).
    let strata = compute_strata(&rules)?;
    let max_stratum = strata.values().copied().max().unwrap_or(0);
    for s in 0..=max_stratum {
        let rules_s: Vec<RuleStmt> = rules
            .iter()
            .filter(|r| strata.get(&r.head.pred).copied().unwrap_or(0) == s)
            .cloned()
            .collect();
        if rules_s.is_empty() {
            continue;
        }

        let mut changed = true;
        let mut iterations = 0usize;
        while changed {
            iterations += 1;
            if iterations > 200 {
                bail!("evaluation did not converge in stratum {s}");
            }
            changed = false;

            let snapshot: Vec<Atom> = facts.iter().cloned().collect();
            let mut pending_atoms: Vec<Atom> = Vec::new();
            let mut pending_aggs: Vec<AggFact> = Vec::new();

            for r in &rules_s {
                let derived = eval_rule(r, &snapshot)?;
                for d in derived {
                    match d {
                        DerivedFact::Normal(a) => pending_atoms.push(a),
                        DerivedFact::Agg(agg) => pending_aggs.push(agg),
                    }
                }
            }

            for agg in pending_aggs {
                if upsert_agg(&mut facts, agg)? {
                    changed = true;
                }
            }

            // Apply functional predicates (arg/output) with conflict detection.
            if apply_functional_pred(&mut facts, &pending_atoms, "arg", 3)? {
                changed = true;
            }
            if apply_functional_pred(&mut facts, &pending_atoms, "output", 2)? {
                changed = true;
            }

            // Apply all remaining derived atoms as set inserts.
            for a in pending_atoms {
                if matches!(a.pred.as_str(), "arg" | "output") {
                    continue;
                }
                if facts.insert(a) {
                    changed = true;
                }
            }

            if apply_setting_add(&mut facts)? {
                changed = true;
            }
        }
    }

    // Constraints are checked against the final fact set.
    let snapshot: Vec<Atom> = facts.iter().cloned().collect();
    let mut violations = Vec::new();
    for c in &constraints {
        if constraint_violated(c, &snapshot)? {
            violations.push(c.message.clone());
        }
    }

    // Policy facts: deny/warn.
    let mut warnings = Vec::new();
    for a in &snapshot {
        match a.pred.as_str() {
            "warn" => warnings.push(format_policy_fact(a)?),
            "deny" => violations.push(format_policy_fact(a)?),
            _ => {}
        }
    }

    enforce_uniques(&facts, &uniques)?;

    Ok((EvalResult { facts, warnings }, violations))
}

fn format_policy_fact(a: &Atom) -> Result<String> {
    if a.args.is_empty() {
        bail!("policy fact must have at least a message argument");
    }
    let msg = match &a.args[0] {
        Term::Val(Value::Str(s)) => s.clone(),
        _ => bail!("policy message must be a string"),
    };
    if a.args.len() == 1 {
        return Ok(msg);
    }
    let Term::Val(ctx) = &a.args[1] else {
        bail!("policy context must be ground");
    };
    Ok(format!("{msg} ctx={}", value_to_json_string(ctx)?))
}

fn value_to_json_string(v: &Value) -> Result<String> {
    Ok(serde_json::to_string(&value_to_json(v))?)
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Obj(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Ref { typ, name, attr } => {
            serde_json::Value::String(format!("ref({typ},{name},{attr})"))
        }
        Value::CloudRef { typ, name, attr } => {
            serde_json::Value::String(format!("cloud_ref({typ},{name},{attr})"))
        }
    }
}

fn enforce_uniques(facts: &BTreeSet<Atom>, uniques: &[Unique]) -> Result<()> {
    for u in uniques {
        if u.key_arity == 0 {
            bail!("unique {}(0) is not meaningful", u.pred);
        }
        let mut seen: BTreeMap<Vec<Value>, Vec<Value>> = BTreeMap::new();
        for a in facts.iter().filter(|a| a.pred == u.pred) {
            if a.args.len() < u.key_arity {
                bail!("unique {}({}) but fact has arity {}", u.pred, u.key_arity, a.args.len());
            }
            let mut key = Vec::new();
            let mut rest = Vec::new();
            for (i, t) in a.args.iter().enumerate() {
                let Term::Val(v) = t else {
                    bail!("internal: non-ground fact in uniqueness check");
                };
                if i < u.key_arity {
                    key.push(v.clone());
                } else {
                    rest.push(v.clone());
                }
            }
            if let Some(prev) = seen.get(&key) {
                if prev != &rest {
                    bail!("unique violation for {} on key {:?}", u.pred, key);
                }
            } else {
                seen.insert(key, rest);
            }
        }
    }
    Ok(())
}

fn compute_strata(rules: &[RuleStmt]) -> Result<BTreeMap<String, usize>> {
    let mut preds: BTreeSet<String> = BTreeSet::new();
    for r in rules {
        preds.insert(r.head.pred.clone());
        for lit in &r.body {
            match lit {
                Lit::Pos(a) | Lit::Not(a) => {
                    preds.insert(a.pred.clone());
                }
                _ => {}
            }
        }
    }

    let mut stratum: BTreeMap<String, usize> = preds.into_iter().map(|p| (p, 0usize)).collect();
    let edges: Vec<(String, String, bool)> = rules
        .iter()
        .flat_map(|r| {
            let head = r.head.pred.clone();
            r.body.iter().filter_map(move |lit| match lit {
                Lit::Pos(a) => Some((head.clone(), a.pred.clone(), false)),
                Lit::Not(a) => Some((head.clone(), a.pred.clone(), true)),
                _ => None,
            })
        })
        .collect();

    // Relax constraints until fixed point. A negation cycle will keep increasing.
    for _ in 0..10_000 {
        let mut changed = false;
        for (h, b, neg) in &edges {
            let req = stratum.get(b).copied().unwrap_or(0) + if *neg { 1 } else { 0 };
            let cur = stratum.get(h).copied().unwrap_or(0);
            if cur < req {
                stratum.insert(h.clone(), req);
                changed = true;
            }
        }
        if !changed {
            // Validate: any negative edge must be strictly lower.
            for (h, b, neg) in &edges {
                if *neg {
                    let hs = stratum.get(h).copied().unwrap_or(0);
                    let bs = stratum.get(b).copied().unwrap_or(0);
                    if hs <= bs {
                        bail!("negation cycle detected involving {h} and {b}");
                    }
                }
            }
            return Ok(stratum);
        }
    }

    bail!("failed to stratify program (possible negation cycle)")
}

fn ensure_ground(a: &Atom) -> Result<Atom> {
    let mut args = Vec::with_capacity(a.args.len());
    for t in &a.args {
        let v = eval_term(t, &HashMap::new())
            .with_context(|| format!("fact is not ground: {}(...)", a.pred))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: a.pred.clone(),
        args,
        record: None,
    })
}

fn eval_rule(rule: &RuleStmt, facts: &[Atom]) -> Result<Vec<DerivedFact>> {
    let collect = find_collect(&rule.head);
    if let Some((idx, kind)) = collect {
        return eval_rule_collect(rule, facts, idx, kind);
    }

    let mut out = Vec::new();
    let bindings = eval_body(&rule.body, facts)?;
    for b in bindings {
        let head = instantiate_atom(&rule.head, &b)
            .with_context(|| format!("instantiate head {}", rule.head.pred))?;
        out.push(DerivedFact::Normal(head));
    }
    Ok(out)
}

#[derive(Debug, Copy, Clone)]
enum CollectKind {
    Set,
    List,
}

fn eval_rule_collect(
    rule: &RuleStmt,
    facts: &[Atom],
    idx: usize,
    kind: CollectKind,
) -> Result<Vec<DerivedFact>> {
    let Term::Func { name: _, args } = &rule.head.args[idx] else {
        bail!("internal: collect idx not func");
    };
    if args.len() != 1 {
        bail!("collect*(...) must have exactly one argument");
    }
    let item_term = args[0].clone();

    let bindings = eval_body(&rule.body, facts)?;
    let mut groups_set: BTreeMap<Vec<Value>, BTreeSet<Value>> = BTreeMap::new();
    let mut groups_list: BTreeMap<Vec<Value>, Vec<Value>> = BTreeMap::new();
    for b in bindings {
        let mut key = Vec::new();
        for (i, t) in rule.head.args.iter().enumerate() {
            if i == idx {
                continue;
            }
            let v = eval_term(t, &b).ok_or_else(|| anyhow!("non-ground head term"))?;
            key.push(v);
        }
        let item = eval_term(&item_term, &b).ok_or_else(|| anyhow!("non-ground collect item"))?;
        match kind {
            CollectKind::Set => {
                groups_set.entry(key).or_default().insert(item);
            }
            CollectKind::List => {
                groups_list.entry(key).or_default().push(item);
            }
        }
    }

    let mut out = Vec::new();

    let mut emit_group = |key: Vec<Value>, mut items: Vec<Value>| {
        // Deterministic output: Datalog doesn't define an order, so we sort.
        items.sort();

        let mut args_out = Vec::with_capacity(rule.head.args.len());
        let mut k = 0usize;
        for i in 0..rule.head.args.len() {
            if i == idx {
                args_out.push(Term::Val(Value::List(items.clone())));
            } else {
                args_out.push(Term::Val(key[k].clone()));
                k += 1;
            }
        }
        let atom = Atom {
            pred: rule.head.pred.clone(),
            args: args_out,
            record: None,
        };
        out.push(DerivedFact::Agg(AggFact {
            pred: rule.head.pred.clone(),
            idx,
            key,
            atom,
        }));
    };

    for (key, items) in groups_set {
        emit_group(key, items.into_iter().collect());
    }
    for (key, items) in groups_list {
        emit_group(key, items);
    }

    Ok(out)
}

fn upsert_agg(facts: &mut BTreeSet<Atom>, agg: AggFact) -> Result<bool> {
    // Ensure we only keep one aggregate value per (pred, key).
    // This avoids accumulating stale aggregate results across fixpoint iterations.
    let mut to_remove = Vec::new();
    for a in facts.iter() {
        if a.pred != agg.pred {
            continue;
        }
        let key = key_from_atom(a, agg.idx)?;
        if key == agg.key {
            // Same group.
            if a.args == agg.atom.args {
                return Ok(false);
            }
            to_remove.push(a.clone());
        }
    }
    for r in to_remove {
        facts.remove(&r);
    }
    Ok(facts.insert(agg.atom))
}

fn key_from_atom(a: &Atom, idx: usize) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (i, t) in a.args.iter().enumerate() {
        if i == idx {
            continue;
        }
        let Term::Val(v) = t else {
            bail!("internal: expected ground atom in agg key");
        };
        out.push(v.clone());
    }
    Ok(out)
}

fn apply_functional_pred(
    facts: &mut BTreeSet<Atom>,
    derived: &[Atom],
    pred: &str,
    key_arity: usize,
) -> Result<bool> {
    let mut grouped: BTreeMap<Vec<Value>, Atom> = BTreeMap::new();
    for a in derived.iter().filter(|a| a.pred == pred) {
        let key = first_n_vals(a, key_arity)?;
        if let Some(prev) = grouped.get(&key) {
            if prev.args != a.args {
                bail!("conflicting derived {pred} for key {key:?}");
            }
        } else {
            grouped.insert(key, a.clone());
        }
    }
    if grouped.is_empty() {
        return Ok(false);
    }

    let mut changed = false;
    for (key, atom) in grouped {
        // Remove stale existing values for that key.
        let mut existing: Vec<Atom> = facts
            .iter()
            .filter(|a| a.pred == pred)
            .filter_map(|a| (first_n_vals(a, key_arity).ok()? == key).then_some(a.clone()))
            .collect();

        if existing.len() == 1 && existing[0].args == atom.args {
            continue;
        }

        for r in existing.drain(..) {
            facts.remove(&r);
        }
        facts.insert(atom);
        changed = true;
    }
    Ok(changed)
}

fn first_n_vals(a: &Atom, n: usize) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for t in a.args.iter().take(n) {
        let Term::Val(v) = t else {
            bail!("internal: expected ground atom");
        };
        out.push(v.clone());
    }
    Ok(out)
}

fn apply_setting_add(facts: &mut BTreeSet<Atom>) -> Result<bool> {
    // Merge `setting_add(Env, Key, Value)` into effective `setting(Env, Key, Value)`.
    let rules = merge::parse_merge_rules(facts.iter())?;

    #[derive(Default)]
    struct Group {
        base: Option<Value>,
        adds: Vec<Value>,
    }

    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();

    for a in facts.iter() {
        if a.pred != "setting" && a.pred != "setting_add" {
            continue;
        }
        if a.args.len() != 3 {
            bail!("{}/3 expected", a.pred);
        }
        let env = term_str(&a.args[0])?;
        let key = term_str(&a.args[1])?;
        let Term::Val(v) = &a.args[2] else {
            bail!("setting values must be ground");
        };
        let g = groups.entry((env.to_string(), key.to_string())).or_default();
        if a.pred == "setting" {
            match &g.base {
                None => g.base = Some(v.clone()),
                Some(prev) if prev == v => {}
                Some(prev) => bail!(
                    "conflicting base setting({}, {}) values: {prev:?} vs {v:?}",
                    env,
                    key
                ),
            }
        } else {
            g.adds.push(v.clone());
        }
    }

    let mut changed = false;
    for ((env, key), g) in groups {
        if g.adds.is_empty() {
            continue;
        }

        let mut merged = g.base;
        for add in g.adds {
            merged = Some(match merged {
                None => add,
                Some(cur) => merge::merge_value(cur, add, &rules, "setting", &key)?,
            });
        }
        let Some(merged) = merged else {
            continue;
        };

        // Replace any existing `setting(env,key,...)` with the merged value.
        let mut existing: Vec<Atom> = facts
            .iter()
            .filter(|a| a.pred == "setting")
            .filter(|a| {
                a.args.len() == 3
                    && term_str(&a.args[0]).ok() == Some(env.as_str())
                    && term_str(&a.args[1]).ok() == Some(key.as_str())
            })
            .cloned()
            .collect();

        if existing.len() == 1 {
            if let Term::Val(v) = &existing[0].args[2] {
                if v == &merged {
                    continue;
                }
            }
        }

        for e in existing.drain(..) {
            facts.remove(&e);
        }
        facts.insert(Atom {
            pred: "setting".to_string(),
            args: vec![
                Term::Val(Value::Str(env.clone())),
                Term::Val(Value::Str(key.clone())),
                Term::Val(merged),
            ],
            record: None,
        });
        changed = true;
    }

    Ok(changed)
}

fn term_str(t: &Term) -> Result<&str> {
    let Term::Val(Value::Str(s)) = t else {
        bail!("expected string")
    };
    Ok(s)
}

fn find_collect(head: &Atom) -> Option<(usize, CollectKind)> {
    for (i, t) in head.args.iter().enumerate() {
        let Term::Func { name, args } = t else {
            continue;
        };
        if args.len() != 1 {
            continue;
        }
        match name.as_str() {
            // Back-compat: `collect(X)` is set-like.
            "collect" | "collect_set" => return Some((i, CollectKind::Set)),
            "collect_list" => return Some((i, CollectKind::List)),
            _ => {}
        }
    }
    None
}

fn constraint_violated(c: &Constraint, facts: &[Atom]) -> Result<bool> {
    let bindings = eval_body(&c.body, facts)?;
    Ok(!bindings.is_empty())
}

fn eval_body(body: &[Lit], facts: &[Atom]) -> Result<Vec<HashMap<String, Value>>> {
    let mut states: Vec<HashMap<String, Value>> = vec![HashMap::new()];
    for lit in body {
        let mut next = Vec::new();
        match lit {
            Lit::Pos(atom) => {
                if atom.pred == "member" {
                    if atom.args.len() != 2 {
                        bail!("member/2 expected");
                    }
                    for s in &states {
                        let list_v = eval_term(&atom.args[0], s)
                            .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
                        let Value::List(items) = list_v else {
                            continue;
                        };
                        for item in &items {
                            let mut s2 = s.clone();
                            if unify_term(&atom.args[1], item, &mut s2)? {
                                next.push(s2);
                            }
                        }
                    }
                    states = next;
                    if states.is_empty() {
                        break;
                    }
                    continue;
                }
                for s in &states {
                    for f in facts.iter().filter(|x| x.pred == atom.pred) {
                        if let Some(s2) = unify_atom(atom, f, s)? {
                            next.push(s2);
                        }
                    }
                }
            }
            Lit::Not(atom) => {
                for s in &states {
                    let grounded = ground_atom(atom, s)
                        .with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
                    let any = facts.iter().any(|f| f.pred == grounded.pred && f.args == grounded.args);
                    if !any {
                        next.push(s.clone());
                    }
                }
            }
            Lit::Eq(a, b) => {
                for s in &states {
                    if let Some(s2) = eval_eq(a, b, s)? {
                        next.push(s2);
                    }
                }
            }
            Lit::Neq(a, b) => {
                for s in &states {
                    if let Some(s2) = eval_neq(a, b, s)? {
                        next.push(s2);
                    }
                }
            }
            Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                for s in &states {
                    if eval_cmp(lit, a, b, s)? {
                        next.push(s.clone());
                    }
                }
            }
        }
        states = next;
        if states.is_empty() {
            break;
        }
    }
    Ok(states)
}

fn unify_atom(pattern: &Atom, fact: &Atom, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    if pattern.args.len() != fact.args.len() {
        return Ok(None);
    }
    let mut out = state.clone();
    for (p, f) in pattern.args.iter().zip(&fact.args) {
        let Term::Val(fv) = f else {
            bail!("internal: non-ground fact");
        };
        if !unify_term(p, fv, &mut out)? {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

fn unify_term(pat: &Term, fv: &Value, out: &mut HashMap<String, Value>) -> Result<bool> {
    match pat {
        Term::Val(v) => Ok(v == fv),
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(bound == fv)
            } else {
                out.insert(name.clone(), fv.clone());
                Ok(true)
            }
        }
        Term::Wildcard => Ok(true),
        Term::List(items) => {
            let Value::List(vs) = fv else {
                return Ok(false);
            };
            if items.len() != vs.len() {
                return Ok(false);
            }
            let mut tmp = out.clone();
            for (t, v) in items.iter().zip(vs) {
                if !unify_term(t, v, &mut tmp)? {
                    return Ok(false);
                }
            }
            *out = tmp;
            Ok(true)
        }
        Term::Obj(m) => {
            let Value::Obj(vm) = fv else {
                return Ok(false);
            };
            if m.len() != vm.len() {
                return Ok(false);
            }
            let mut tmp = out.clone();
            for (k, t) in m {
                let Some(v) = vm.get(k) else {
                    return Ok(false);
                };
                if !unify_term(t, v, &mut tmp)? {
                    return Ok(false);
                }
            }
            *out = tmp;
            Ok(true)
        }
        Term::Func { name, args } => {
            // Special pattern unification for scoped(Scope, LocalName).
            // This allows rules to join on component-scoped resources while still
            // binding LocalName variables.
            if name == "scoped" && args.len() == 2 {
                let Some(Value::Str(scope)) = eval_term(&args[0], out) else {
                    return Ok(false);
                };
                let Value::Str(full) = fv else {
                    return Ok(false);
                };
                let prefix = format!("{scope}::");
                let Some(suffix) = full.strip_prefix(&prefix) else {
                    return Ok(false);
                };
                return unify_term(&args[1], &Value::Str(suffix.to_string()), out);
            }

            let pv = match eval_term(pat, out) {
                Some(v) => v,
                None => return Ok(false),
            };
            Ok(&pv == fv)
        }
        Term::ListComp { .. } => {
            // Comprehensions must be lowered before evaluation.
            Ok(false)
        }
    }
}

fn ground_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        let v = eval_term(t, state).ok_or_else(|| anyhow!("unbound var in negation"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
    })
}

fn instantiate_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        if matches!(t, Term::Func { name, .. } if name == "collect") {
            bail!("internal: collect must be handled separately");
        }
        let v = eval_term(t, state).ok_or_else(|| anyhow!("non-ground head"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
    })
}

fn eval_eq(a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    let mut out = state.clone();
    match (eval_term(a, &out), eval_term(b, &out)) {
        (Some(av), Some(bv)) => Ok((av == bv).then_some(out)),
        (Some(av), None) => {
            if bind_term(b, av, &mut out)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, Some(bv)) => {
            if bind_term(a, bv, &mut out)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, None) => bail!("unsafe equality: both sides unbound"),
    }
}

fn eval_neq(a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    match (eval_term(a, state), eval_term(b, state)) {
        (Some(av), Some(bv)) => Ok((av != bv).then_some(state.clone())),
        _ => bail!("unsafe !=: both sides must be ground"),
    }
}

fn eval_cmp(op_lit: &Lit, a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<bool> {
    let Some(av) = eval_term(a, state) else {
        bail!("unsafe comparison: left not ground");
    };
    let Some(bv) = eval_term(b, state) else {
        bail!("unsafe comparison: right not ground");
    };
    let (ai, bi) = match (&av, &bv) {
        (Value::Int(x), Value::Int(y)) => (*x, *y),
        _ => bail!("comparison only supports ints"),
    };
    Ok(match op_lit {
        Lit::Gt(_, _) => ai > bi,
        Lit::Ge(_, _) => ai >= bi,
        Lit::Lt(_, _) => ai < bi,
        Lit::Le(_, _) => ai <= bi,
        _ => unreachable!(),
    })
}

fn bind_term(t: &Term, v: Value, out: &mut HashMap<String, Value>) -> Result<bool> {
    match t {
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(bound == &v)
            } else {
                out.insert(name.clone(), v);
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

fn eval_term(term: &Term, state: &HashMap<String, Value>) -> Option<Value> {
    match term {
        Term::Val(v) => Some(v.clone()),
        Term::Var(name) => state.get(name).cloned(),
        Term::Wildcard => None,
        Term::Func { name, args } => eval_func(name, args, state),
        Term::List(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(eval_term(x, state)?);
            }
            Some(Value::List(out))
        }
        Term::Obj(m) => {
            let mut out = BTreeMap::new();
            for (k, v) in m {
                out.insert(k.clone(), eval_term(v, state)?);
            }
            Some(Value::Obj(out))
        }
        Term::ListComp { .. } => None,
    }
}

fn eval_func(name: &str, args: &[Term], state: &HashMap<String, Value>) -> Option<Value> {
    match name {
        "scoped" => {
            if args.len() != 2 {
                return None;
            }
            let scope = value_to_string(&eval_term(&args[0], state)?);
            let name = value_to_string(&eval_term(&args[1], state)?);
            Some(Value::Str(format!("{scope}::{name}")))
        }
        "format" => {
            let mut ev = Vec::new();
            for a in args {
                ev.push(eval_term(a, state)?);
            }
            let fmt = ev.first()?.as_str()?.to_string();
            let mut out = String::new();
            let mut parts = fmt.split("%s");
            out.push_str(parts.next().unwrap_or(""));
            for (i, p) in parts.enumerate() {
                let v = ev.get(i + 1)?;
                out.push_str(&value_to_string(v));
                out.push_str(p);
            }
            Some(Value::Str(out))
        }
        "concat" => {
            let mut out = String::new();
            for a in args {
                out.push_str(&value_to_string(&eval_term(a, state)?));
            }
            Some(Value::Str(out))
        }
        "ref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::Ref {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "cloud_ref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::CloudRef {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "gref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::Ref {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "cidrsubnet" => {
            if args.len() != 3 {
                return None;
            }
            let cidr = eval_term(&args[0], state)?.as_str()?.to_string();
            let newbits = match eval_term(&args[1], state)? {
                Value::Int(i) => i,
                _ => return None,
            };
            let netnum = match eval_term(&args[2], state)? {
                Value::Int(i) => i,
                _ => return None,
            };
            Some(Value::Str(cidrsubnet(&cidr, newbits as u32, netnum as u32)?))
        }
        "collect" => None,
        _ => None,
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(_) => "<list>".to_string(),
        Value::Obj(_) => "<obj>".to_string(),
        Value::Ref { typ, name, attr } => format!("ref({typ},{name},{attr})"),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ},{name},{attr})"),
    }
}

fn cidrsubnet(cidr: &str, newbits: u32, netnum: u32) -> Option<String> {
    // Supports only IPv4 CIDRs like "10.0.0.0/16".
    let (ip, prefix) = cidr.split_once('/')?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let new_prefix = prefix.checked_add(newbits)?;
    if new_prefix > 32 {
        return None;
    }
    let base = ipv4_to_u32(ip)?;
    // netnum selects the subnet within the expanded prefix.
    let shift = 32 - new_prefix;
    let subnet_base = base + (netnum << shift);
    Some(format!("{}/{}", u32_to_ipv4(subnet_base), new_prefix))
}

fn ipv4_to_u32(ip: &str) -> Option<u32> {
    let mut out = 0u32;
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    for p in parts {
        let b: u32 = p.parse().ok()?;
        if b > 255 {
            return None;
        }
        out = (out << 8) | b;
    }
    Some(out)
}

fn u32_to_ipv4(v: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (v >> 24) & 255,
        (v >> 16) & 255,
        (v >> 8) & 255,
        v & 255
    )
}
