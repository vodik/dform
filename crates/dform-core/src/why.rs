//! `dform why X` (R-122, R-150): one command for what is and what is
//! not. X decides: a resource, an attribute, a cell or a row the program
//! derives gets its chain (a value's) or its derivation tree; one it does
//! not derive gets why not (`whynot`: the rule that could have, the first
//! condition that failed, the nearest rows or name); a resource `later`
//! holds gets its chain and what it waits on; `deny "MESSAGE"` says
//! whether the deny holds, with its derivation, and if not which clause
//! failed on what. The language server's explain prints a fact the same
//! way ([`fact_text`]).

use crate::ast::{Atom, Lit, Term};
use crate::engine::{self, EvalResult};
use crate::ir;
use crate::partition;
use crate::query::{self, Redactor};
use crate::report::{self, tree};
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

/// How `why` prints: a value's chain, or the derivation `tree`, with
/// `all` its alternatives, in the `core`'s spelling; a long value
/// elided, or `whole` (`-vv`, R-176).
#[derive(Debug, Clone, Copy, Default)]
pub struct As {
    pub tree: bool,
    pub all: bool,
    pub core: bool,
    pub whole: bool,
}

/// What `why` reads besides the evaluation: the relations' signatures,
/// the stack's keys (a chain ends at one), the root sites are relative
/// to, what a resource of a type waits on before its provider plans
/// it (R-110, `deployment::Evaluator::provider_wait`), and, when a plan
/// was made, when the change or the group at an address runs
/// (`report::Report::when`, After R-156).
pub struct Context<'a> {
    pub res: &'a EvalResult,
    /// The providers' schema, for what a resource leaves unset that it
    /// requires (R-184).
    pub schema: Option<&'a crate::schema::Schema>,
    pub redact: &'a Redactor,
    pub signatures: Option<&'a crate::infer::Signatures>,
    pub stack_keys: &'a BTreeSet<String>,
    pub top: Option<&'a Path>,
    pub waits: &'a Lookup<'a>,
    pub when: Option<&'a Lookup<'a>>,
}

/// A text an address or a type maps to, when it has one.
pub type Lookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// `dform why PATTERN`, as text.
pub fn why(pattern: &str, how: As, cx: &Context) -> Result<String> {
    if let Some(message) = deny_message(pattern)? {
        return deny(&message, how, cx);
    }
    // What the program does not derive yet, a resource rule the plan
    // holds as a group: why not, and the tick it waits for.
    let why_not = || -> Result<String> {
        let mut out = crate::whynot::why_not(pattern, cx.res, cx.redact)?;
        if let Some(w) = cx.when.and_then(|when| when(pattern.trim())) {
            out.push_str(&cx.redact.text(&format!("{w}\n")));
        }
        Ok(out)
    };
    let matched = match matches(pattern, &cx.res.facts) {
        Ok(m) => m,
        // What `why` cannot read as a fact may be a row `whynot` reads;
        // else its error names every form.
        Err(_) => return why_not(),
    };
    if matched.is_empty() {
        return why_not();
    }
    let mut out = derivations(&matched, how, cx)?;
    // What a resource leaves unset that its schema requires: what the
    // plan refuses it for, before its provider is asked (R-184).
    for (a, _) in &matched {
        if a.pred != "want" {
            continue;
        }
        let Some(addr) = resource_of(a) else {
            continue;
        };
        let unset = unset_required(&addr, cx);
        if !unset.is_empty() {
            out.push_str("unset, required by the schema:\n");
            out.push_str(&unset.join(""));
        }
    }
    // A contribution of the resource that reads what nothing derives
    // answered nothing (R-183): say the read, as the plan's error does.
    let holders: BTreeSet<ir::Address> =
        matched.iter().filter_map(|(a, _)| resource_of(a)).collect();
    let mut unset: Vec<String> = Vec::new();
    for d in cx.res.facts.iter().filter(|a| a.pred == "deny") {
        let Some(d) = engine::format_policy_fact(d)
            .ok()
            .and_then(|v| report::Dangling::of(&v))
        else {
            continue;
        };
        let line = format!("no value  {}", d.unanswered());
        if d.holder.is_some_and(|h| holders.contains(&h)) && !unset.contains(&line) {
            unset.push(line);
        }
    }
    for l in unset {
        out.push_str(&format!("{l}\n"));
    }
    // A change the plan holds: the tick it runs in and what it waits on,
    // or `later` for what no tick of the plan makes (R-110, R-121, R-156).
    let mut on: Vec<String> = Vec::new();
    for (a, _) in &matched {
        let w = match cx.when {
            Some(when) => resource_of(a).and_then(|r| when(&r.to_string())),
            None => {
                crate::whynot::waiting(a, cx.res, cx.waits).map(|w| format!("later  waits on  {w}"))
            }
        };
        if let Some(w) = w
            && !on.contains(&w)
        {
            on.push(w);
        }
    }
    for w in on {
        out.push_str(&format!("{w}\n"));
    }
    Ok(cx.redact.text(&out))
}

/// Each attribute the resource at `addr` leaves unset that its schema
/// requires, a line `  PATH  (WHAT IT IS)`.
fn unset_required(addr: &ir::Address, cx: &Context) -> Vec<String> {
    let Some(schema) = cx.schema else {
        return Vec::new();
    };
    let Ok(resources) = ir::compile_resources(cx.res.facts.iter().cloned(), schema) else {
        return Vec::new();
    };
    let Some(r) = resources.iter().find(|r| r.addr == *addr) else {
        return Vec::new();
    };
    let doc = engine::value_to_json(&r.attrs);
    schema
        .unset_required(&addr.typ, &doc)
        .into_iter()
        .map(|p| match schema.required_doc(&addr.typ, &p) {
            Some(w) => format!("  {p}  ({w})\n"),
            None => format!("  {p}\n"),
        })
        .collect()
}

/// The resource a `want` or an `attr` fact is of.
fn resource_of(a: &Atom) -> Option<ir::Address> {
    match (a.pred.as_str(), a.args.as_slice()) {
        ("want" | "attr", [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), ..]) => {
            Some(ir::Address {
                typ: t.clone(),
                name: n.clone(),
            })
        }
        _ => None,
    }
}

/// `dform why --json PATTERN` (R-176): one object per fact the pattern
/// names, `fact` as `why` heads it, `value` whole for a value (an
/// attribute, a cell; a secret as `query --json` says it), and `text`,
/// what `why -vv` prints of it; for what the program does not derive,
/// `why_not`, and for a deny, `text`.
pub fn why_json(pattern: &str, how: As, cx: &Context) -> Result<serde_json::Value> {
    let how = As { whole: true, ..how };
    if deny_message(pattern)?.is_some() {
        return Ok(serde_json::json!({ "text": why(pattern, how, cx)? }));
    }
    let matched = match matches(pattern, &cx.res.facts) {
        Ok(m) if !m.is_empty() => m,
        _ => {
            let text = crate::whynot::why_not(pattern, cx.res, cx.redact)?;
            return Ok(serde_json::json!({ "why_not": text }));
        }
    };
    let mut out = Vec::new();
    for m in &matched {
        let (a, focus) = m;
        let keys = focus.as_ref().map(tree::Focus::keys).unwrap_or_default();
        let fact = match head_name(a).filter(|_| a.pred == "attr") {
            Some(n) => keys.iter().fold(n, |p, k| crate::ir::path_join(&p, k)),
            None => cx.redact.surface_atom(a),
        };
        let mut o = serde_json::json!({ "fact": fact });
        if a.pred == "attr"
            && let Some(Term::Val(v)) = a.args.get(3)
        {
            let v = keys.iter().try_fold(v, |v, k| match v {
                Value::Obj(m) => m.get(k),
                _ => None,
            });
            if let Some(v) = v {
                o["value"] = cx.redact.json(v);
            }
        }
        let text = derivations(std::slice::from_ref(m), how, cx)?;
        o["text"] = serde_json::Value::String(cx.redact.text(&text));
        out.push(o);
    }
    Ok(serde_json::Value::Array(out))
}

/// The facts `pattern` names: an input's or a `let`'s cell by its name,
/// an address as the plan prints it, an address or a fact pattern, a
/// copy's output.
fn matches(pattern: &str, facts: &BTreeSet<Atom>) -> Result<Vec<Matched>> {
    let printed = query::printed(pattern, facts);
    let cells = [crate::modules::INPUT, crate::modules::LET];
    let matched = match input_cell(pattern, &cells, facts)? {
        Some(m) => m,
        // An address as the plan prints it, `ovh.ssh_key k3s.admin`, or
        // its path, `k3s.admin` (R-111).
        None if !printed.is_empty() => {
            let mut out = Vec::new();
            for (addr, path) in printed {
                let query::Query::Body { body, .. } = query::pattern(&addr, path, true) else {
                    continue;
                };
                if let [Lit::Pos(pat)] = body.as_slice() {
                    out.extend(tree::find(pat, facts)?);
                }
            }
            out
        }
        None => {
            let parsed = match query::address(pattern, true)? {
                Some(q) => q,
                None => query::parse(pattern)?,
            };
            let query::Query::Body { body, .. } = parsed else {
                bail!(
                    "why: expected an address such as 'net.vpc main' or \
                     'net.vpc[\"main\"].cidr', an input such as 'nodes.count', a fact \
                     pattern such as 'want(net.vpc, N)', or a deny such as 'deny \"MESSAGE\"', \
                     got '{pattern}'"
                );
            };
            let [Lit::Pos(pat)] = body.as_slice() else {
                bail!("why: expected one fact pattern, got '{pattern}'");
            };
            tree::find(pat, facts)?
        }
    };
    // A copy's output, `synapse_db.conn` (R-120), when no resource is so
    // named.
    Ok(match matched.is_empty() {
        true => input_cell(pattern, &[crate::transform::OUTPUT], facts)?.unwrap_or_default(),
        false => matched,
    })
}

/// Each fact of `matched` as `why` prints it: its chain, else its
/// derivation tree, a relation's facts after its signature.
fn derivations(
    matched: &[Matched],
    As {
        tree,
        all,
        core,
        whole,
    }: As,
    cx: &Context,
) -> Result<String> {
    let res = cx.res;
    let printer = tree::Printer {
        circuit: &res.circuit,
        redact: cx.redact,
        all,
    };
    let mut out = String::new();
    // A relation's facts follow its signature, its columns as declared or
    // inferred (R-34), as `decl` writes them: once, before the first, when
    // any column has a type.
    let mut typed = BTreeSet::new();
    for (i, (a, focus)) in matched.iter().enumerate() {
        let Some(id) = res.circuit.fact_id(&engine::circuit_fact(a)) else {
            bail!("internal: no provenance for {}", partition::fmt_atom(a));
        };
        if i > 0 {
            out.push('\n');
        }
        if !core
            && let Some(sig) = cx
                .signatures
                .and_then(|s| s.get(&(a.pred.clone(), a.args.len())))
            && sig
                .columns
                .iter()
                .any(|c| c.ty.as_ref().is_some_and(|t| *t != crate::types::Ty::Any))
            && typed.insert(&a.pred)
        {
            out.push_str(&format!("decl {sig}\n"));
        }
        if core {
            out.push_str(&printer.tree(id, focus.as_ref()));
        } else if let (false, Some(text)) = (
            tree,
            chain(
                &printer,
                res,
                a,
                focus.as_ref(),
                cx.stack_keys,
                cx.top,
                whole,
            ),
        ) {
            out.push_str(&text);
        } else {
            out.push_str(&printer.source_tree(&res.rules, id, focus.as_ref()));
        }
    }
    Ok(out)
}

/// What `why` prints of the fact at circuit node `id` (the language
/// server's explain): its chain, else its derivation tree.
pub fn fact_text(
    res: &EvalResult,
    redact: &Redactor,
    id: crate::circuit::NodeId,
    stack_keys: &BTreeSet<String>,
) -> String {
    let printer = tree::Printer {
        circuit: &res.circuit,
        redact,
        all: false,
    };
    let crate::circuit::View::Fact { fact, .. } = res.circuit.view(id) else {
        return printer.source_tree(&res.rules, id, None);
    };
    chain(&printer, res, &fact.atom(), None, stack_keys, None, false)
        .unwrap_or_else(|| printer.source_tree(&res.rules, id, None))
}

/// `deny "MESSAGE"` (R-150): the message, unquoted.
fn deny_message(pattern: &str) -> Result<Option<String>> {
    let Some(rest) = pattern.trim().strip_prefix("deny") else {
        return Ok(None);
    };
    let rest = rest.trim();
    if !rest.starts_with('"') {
        return Ok(None);
    }
    match crate::parser::parse_program(&format!("m({rest})")) {
        Ok(p) => match p.statements.as_slice() {
            [crate::ast::Stmt::Fact(Atom { args, .. })] => match args.as_slice() {
                [Term::Val(Value::Str(m))] => Ok(Some(m.clone())),
                _ => bail!("why: expected 'deny \"MESSAGE\"', got '{pattern}'"),
            },
            _ => bail!("why: expected 'deny \"MESSAGE\"', got '{pattern}'"),
        },
        Err(_) => bail!("why: expected 'deny \"MESSAGE\"', got '{pattern}'"),
    }
}

/// `why deny "MESSAGE"`: whether a deny of that message holds, each
/// firing with its derivation; if none does, why not: which clause of
/// each deny so written failed, on what.
fn deny(message: &str, how: As, cx: &Context) -> Result<String> {
    let res = cx.res;
    let held: Vec<Matched> = res
        .facts
        .iter()
        .filter(|a| a.pred == "deny")
        .filter(|a| matches!(a.args.first(), Some(Term::Val(Value::Str(m))) if m == message))
        .map(|a| (a.clone(), None))
        .collect();
    let quoted = crate::partition::fmt_value(&Value::Str(message.to_string()));
    if !held.is_empty() {
        let mut out = format!("deny {quoted}: holds\n");
        out.push_str(&derivations(&held, how, cx)?);
        return Ok(cx.redact.text(&out));
    }
    let messages: Vec<&Term> = res
        .rules
        .iter()
        .filter(|r| r.head.pred == "deny")
        .filter_map(|r| r.head.args.first())
        .collect();
    let written = |m: &Term| matches!(m, Term::Val(Value::Str(m)) if m == message);
    // A deny with a context (`deny "m" { a, b }`) is `deny(m, ctx)`.
    let arity = res
        .rules
        .iter()
        .filter(|r| r.head.pred == "deny")
        .find(|r| r.head.args.first().is_some_and(written))
        .map_or(1, |r| r.head.args.len());
    let pattern = match arity {
        1 => format!("deny({quoted})"),
        _ => format!("deny({quoted}, ctx)"),
    };
    // Said as the deny is written, then why not, as for any row; a
    // message no deny can say, the nearest one written.
    let text = crate::whynot::why_not(&pattern, res, cx.redact)?;
    if let Some((first, rest)) = text.split_once('\n')
        && first.ends_with(": no rule derives it")
    {
        return Ok(format!("deny {quoted}: does not hold\n{rest}"));
    }
    let mut out = format!("deny {quoted}: no deny says it\n");
    let near = messages
        .iter()
        .filter_map(|m| match m {
            Term::Val(Value::Str(m)) => Some((crate::whynot::edits(message, m), m)),
            _ => None,
        })
        .min();
    if let Some((_, m)) = near {
        let m = crate::partition::fmt_value(&Value::Str(m.clone()));
        out.push_str(&format!("  nearest: deny {m}\n"));
    }
    Ok(cx.redact.text(&out))
}

/// What `why` prints of a value (R-122): an attribute's or a cell's head
/// line and its chain, an object by its leaves; a resource's header,
/// `ADDR  SITE`, and each attribute's. `None` for any other fact, which
/// prints its tree.
pub fn chain(
    printer: &tree::Printer,
    res: &engine::EvalResult,
    a: &Atom,
    focus: Option<&tree::Focus>,
    stack_keys: &BTreeSet<String>,
    top: Option<&Path>,
    whole: bool,
) -> Option<String> {
    let style = report::Style::PLAIN;
    let relative = |at: &str| {
        top.and_then(|t| report::relative_place(at, t))
            .unwrap_or_else(|| at.to_string())
    };
    // `T NAME.path = value` per leaf of attribute fact `f` below `keys`,
    // each with its chain; a cell's by its own name. The leaves one
    // contribution wrote fold to one value where the writers diverge
    // (R-124), with that contribution's chain.
    let leaves = |f: &Atom, head: &str, keys: &[String]| {
        let mut items = Vec::new();
        let (Some(Term::Val(v)), Some(Term::Val(Value::Str(top)))) = (f.args.get(3), f.args.get(2))
        else {
            return items;
        };
        let Some(v) = keys.iter().try_fold(v, |v, k| match v {
            Value::Obj(m) => m.get(k),
            _ => None,
        }) else {
            return items;
        };
        let mut found = Vec::new();
        object_leaves(v, &mut keys.to_vec(), &mut found);
        // Each leaf's path from the resource (`spec.replicas`), and the
        // contribution that wrote it.
        let paths: Vec<String> = found
            .iter()
            .map(|(keys, _)| {
                keys.iter()
                    .fold(top.clone(), |p, k| crate::ir::path_join(&p, k))
            })
            .collect();
        let resource = matches!(f.args.first(), Some(Term::Val(Value::Str(t)))
            if ![
                crate::modules::INPUT,
                crate::modules::LET,
                crate::transform::OUTPUT,
            ]
            .contains(&t.as_str()));
        let writers = match resource {
            true => printer.writers(f, &paths),
            false => vec![None; paths.len()],
        };
        // A list several writers add to (a set, R-158) is said element by
        // element, each with its own writer's chain.
        let mut elem = vec![false; found.len()];
        let (mut found, mut paths, mut writers) = (found, paths, writers);
        if resource {
            let mut i = 0;
            while i < found.len() {
                let Value::List(xs) = &found[i].1 else {
                    i += 1;
                    continue;
                };
                if xs.is_empty() || printer.redact.is_secret(&found[i].1) {
                    i += 1;
                    continue;
                }
                let at: Vec<String> = (0..xs.len())
                    .map(|j| format!("{}[{j}]", paths[i]))
                    .collect();
                let ws = printer.writers(f, &at);
                if ws.iter().all(|w| *w == ws[0]) {
                    i += 1;
                    continue;
                }
                let keys = found[i].0.clone();
                let items: Vec<(Vec<String>, Value)> =
                    xs.iter().map(|x| (keys.clone(), x.clone())).collect();
                let n = items.len();
                found.splice(i..=i, items);
                paths.splice(i..=i, at);
                writers.splice(i..=i, ws);
                elem.splice(i..=i, vec![true; n]);
                i += n;
            }
        }
        let printed = |p: &str| match head.strip_suffix(top.as_str()) {
            Some(addr) => format!("{addr}{p}"),
            None => p.to_string(),
        };
        let relative_chain = |mut chain: Vec<tree::Step>| {
            for step in chain.iter_mut() {
                step.at = relative(&step.at);
            }
            chain
        };
        let surface = |v: &Value| printer.redact.surface(v);
        // A plain leaf of a secret object is `(sensitive)` (R-124
        // amendment 2): its value is no secret elsewhere.
        let whole = match f.args.get(3) {
            Some(Term::Val(w)) => w,
            _ => v,
        };
        let hidden = |keys: &[String], leaf: &Value| {
            !printer.redact.is_secret(leaf)
                && report::surface_in(printer.redact, whole, keys, leaf) == "(sensitive)"
        };
        for g in report::fold::fold(&paths, &writers) {
            let laid = |v: &Value| {
                crate::fmt::value::Tree::of(v, &|v| {
                    let open =
                        matches!(v, Value::Obj(_) | Value::List(_)) && !printer.redact.is_secret(v);
                    (!open).then(|| surface(v))
                })
            };
            if let [i] = g.leaves.as_slice() {
                let (keys, leaf) = &found[*i];
                let chain = match (elem[*i], writers[*i]) {
                    (true, Some(w)) => relative_chain(
                        printer.contribution_chain(&res.rules, w, &paths[*i], stack_keys),
                    ),
                    _ => relative_chain(printer.attr_chain(&res.rules, f, keys, stack_keys)),
                };
                // A list is laid out as a fold is.
                if matches!(leaf, Value::List(xs) if !xs.is_empty())
                    && !printer.redact.is_secret(leaf)
                    && !hidden(keys, leaf)
                {
                    items.push(report::ChainItem {
                        head: format!("{} = ", printed(&paths[*i])),
                        shown: surface(leaf),
                        chain,
                        value: Some(laid(leaf)),
                    });
                    continue;
                }
                let shown = match hidden(keys, leaf) {
                    true => "(sensitive)".to_string(),
                    false => surface(leaf),
                };
                items.push(report::ChainItem {
                    head: format!("{} = {shown}", printed(&paths[*i])),
                    shown,
                    chain,
                    value: None,
                });
                continue;
            }
            let values: Vec<crate::fmt::value::Tree> = found
                .iter()
                .map(|(keys, leaf)| match hidden(keys, leaf) {
                    true => crate::fmt::value::Tree::Leaf("(sensitive)".into()),
                    false => laid(leaf),
                })
                .collect();
            let w = writers[g.leaves[0]].expect("a fold has its writer");
            // The part of the value the fold prints, as a chain's step
            // that is the literal itself says it.
            let mut part = Value::Obj(Default::default());
            for &i in &g.leaves {
                let (keys, leaf) = &found[i];
                let below = &keys[(g.depth - report::fold::tokens(top).len()).min(keys.len())..];
                nest(&mut part, below, leaf.clone());
            }
            items.push(report::ChainItem {
                head: format!("{} = ", printed(&g.path)),
                shown: surface(&part),
                chain: relative_chain(
                    printer.contribution_chain(&res.rules, w, &g.path, stack_keys),
                ),
                value: Some(report::fold::assemble(&g, &paths, &values)),
            });
        }
        items
    };
    match a.pred.as_str() {
        "attr" => {
            let keys = focus.map(tree::Focus::keys).unwrap_or_default();
            let items = leaves(a, &head_name(a)?, keys);
            Some(report::chains_text(&items, "", style, whole))
        }
        "want" => {
            let [Term::Val(Value::Str(t)), Term::Val(Value::Str(n))] = a.args.as_slice() else {
                return None;
            };
            let addr = ir::Address {
                typ: t.clone(),
                name: n.clone(),
            };
            let mut out = report::address(&addr);
            if let Some(site) = printer.want_site(&res.rules, &addr) {
                out.push_str(&format!("  {}", relative(&site.at)));
                if !site.with.is_empty() {
                    out.push_str(&format!("  with {}", site.with.join(", ")));
                }
            }
            out.push('\n');
            let mut items = Vec::new();
            for f in res.facts.iter().filter(|f| f.pred == "attr") {
                let [
                    Term::Val(Value::Str(ft)),
                    Term::Val(Value::Str(fname)),
                    Term::Val(Value::Str(path)),
                    _,
                ] = f.args.as_slice()
                else {
                    continue;
                };
                if (ft, fname) == (t, n) {
                    items.extend(leaves(f, path, &[]));
                }
            }
            out.push_str(&report::chains_text(&items, "  ", style, whole));
            Some(out)
        }
        _ => None,
    }
}

/// The name `why` heads an attribute fact with: `let agent_init`,
/// `input nodes`, `T NAME.path`.
fn head_name(f: &Atom) -> Option<String> {
    let [
        Term::Val(Value::Str(t)),
        Term::Val(Value::Str(n)),
        Term::Val(Value::Str(p)),
        ..,
    ] = f.args.as_slice()
    else {
        return None;
    };
    Some(match t.as_str() {
        crate::modules::INPUT | crate::modules::LET | crate::transform::OUTPUT => {
            let scoped = match n.is_empty() {
                true => p.clone(),
                false => format!("{n}.{p}"),
            };
            format!(
                "{} {scoped}",
                if t == crate::modules::LET {
                    "let"
                } else {
                    t.as_str()
                }
            )
        }
        _ => report::attribute(
            &ir::Address {
                typ: t.clone(),
                name: n.clone(),
            },
            p,
        ),
    })
}

/// `leaf` put into object `v` at `keys`.
fn nest(v: &mut Value, keys: &[String], leaf: Value) {
    let Some((k, rest)) = keys.split_first() else {
        *v = leaf;
        return;
    };
    if let Value::Obj(m) = v {
        let at = m
            .entry(k.clone())
            .or_insert_with(|| Value::Obj(Default::default()));
        nest(at, rest, leaf);
    }
}

/// The leaves of value `v` below `keys`: an object's by its keys, any
/// other value itself.
fn object_leaves(v: &Value, keys: &mut Vec<String>, out: &mut Vec<(Vec<String>, Value)>) {
    match v {
        Value::Obj(m) if !m.is_empty() => {
            for (k, x) in m {
                keys.push(k.clone());
                object_leaves(x, keys, out);
                keys.pop();
            }
        }
        v => out.push((keys.clone(), v.clone())),
    }
}

/// A fact `why` explains, and the part of it the pattern named.
type Matched = (Atom, Option<tree::Focus>);

/// `why NAME`: the cell of one of `kinds` (an input, a `let`, an output)
/// by the name the stack reads it by (R-54, R-55): `replicas`, a leaf of
/// an object input `nodes.count`, a used module's `synapse.replicas`, a
/// copy's output `synapse_db.conn` (R-120). The stack's own name first,
/// then a used module's or a copy's, its scope the name's first segments.
fn input_cell(
    pattern: &str,
    kinds: &[&str],
    facts: &BTreeSet<Atom>,
) -> Result<Option<Vec<Matched>>> {
    let plain = !pattern.is_empty()
        && pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !plain {
        return Ok(None);
    }
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let mut splits = vec![("", pattern)];
    splits.extend(
        pattern
            .match_indices('.')
            .map(|(i, _)| (&pattern[..i], &pattern[i + 1..])),
    );
    for kind in kinds {
        for (scope, path) in &splits {
            let pat = Atom {
                pred: "attr".into(),
                args: vec![s(kind), s(scope), s(path), Term::Var("value".into())],
                record: None,
                span: Default::default(),
            };
            let found = tree::find(&pat, facts)?;
            if !found.is_empty() {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}
