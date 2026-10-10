//! The hover. What the code under the cursor derives, and why: the facts
//! of the innermost rule or stated fact written there, read up to the
//! attributes they contribute to, printed as `dform why` prints them (a
//! cursor on an `attr` read in a rule's body names the attributes that
//! rule read), with the schema's description of each attribute. A
//! declared name shows its declaration's first line and doc comment (an
//! alias its definition; a module or an instance the module's inputs and
//! outputs with theirs); a builtin or a keyword its reference entry. Point
//! on anything else (whitespace, a comment, a literal, a variable) has no
//! hover.

use crate::analysis::Evaluated;
use crate::{cells, refs};
use dform_core::ast::{Atom, Lit, Span, Stmt, Term};
use dform_core::circuit::{Leaf, NodeId, View};
use dform_core::engine;
use dform_core::lattice::Rank;
use dform_core::names::{self, Parsed, Symbol, What};
use dform_core::reference::{self, Reference};
use dform_core::report::tree;
use dform_core::syntax::doc;
use dform_core::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use dform_core::value::Value;
use std::path::Path;

/// What is written at a place: a rule (its index), an `attr` read in a
/// rule's body, or a stated fact.
enum Anchor<'a> {
    Rule(usize),
    Read(usize, &'a Atom),
    Fact(&'a Atom),
}

fn covers(s: Span, in_file: &dyn Fn(u32) -> bool, at: usize) -> bool {
    !s.is_none() && in_file(s.file) && (s.start as usize) <= at && at <= (s.end as usize)
}

fn len(s: Span) -> u32 {
    s.end - s.start
}

/// The fact nodes the code at byte `at` of the file (`in_file` says which
/// sources are it) is about: attributes where it contributes to or reads
/// them, else what it derives.
pub fn targets(e: &Evaluated, in_file: &dyn Fn(u32) -> bool, at: usize) -> Vec<NodeId> {
    let mut anchors: Vec<(u32, Anchor)> = Vec::new();
    for (i, r) in e.res.rules.iter().enumerate() {
        if !covers(r.head.span, in_file, at) {
            continue;
        }
        anchors.push((len(r.head.span), Anchor::Rule(i)));
        for l in &r.body {
            if let Lit::Pos(a) = l
                && a.pred == "attr"
                // Spans compare equal whatever they are: by their bytes.
                && (a.span.start, a.span.end) != (r.head.span.start, r.head.span.end)
                && covers(a.span, in_file, at)
            {
                anchors.push((len(a.span), Anchor::Read(i, a)));
            }
        }
    }
    let stated = e
        .program
        .statements
        .iter()
        .chain(e.lowered.iter().flat_map(|p| &p.statements));
    for s in stated {
        if let Stmt::Fact(a) = s
            && covers(a.span, in_file, at)
        {
            anchors.push((len(a.span), Anchor::Fact(a)));
        }
    }
    let Some(least) = anchors.iter().map(|(n, _)| *n).min() else {
        return Vec::new();
    };
    let c = &e.res.circuit;
    let mut found: Vec<NodeId> = Vec::new();
    for (_, anchor) in anchors.iter().filter(|(n, _)| *n == least) {
        match anchor {
            Anchor::Rule(i) => {
                for &n in e.derived_by(&format!("r{i}")) {
                    for t in up(e, n) {
                        push(&mut found, t);
                    }
                }
            }
            Anchor::Read(i, atom) => {
                let path = match atom.args.get(2) {
                    Some(Term::Val(Value::Str(p))) => Some(p.clone()),
                    _ => None,
                };
                let id = format!("r{i}");
                let before = found.len();
                for &n in e.derived_by(&id) {
                    for ch in read_by(e, n, &id) {
                        let View::Fact { fact, .. } = c.view(ch) else {
                            continue;
                        };
                        let same_path = match (&path, fact.args.get(2)) {
                            (Some(p), Some(Value::Str(q))) => p == q,
                            (None, _) => true,
                            _ => false,
                        };
                        if fact.pred == "attr" && same_path {
                            push(&mut found, ch);
                        }
                    }
                }
                // A rule that never fired read nothing: the attributes its
                // literal matches.
                if found.len() == before
                    && let Ok(matched) = tree::find(atom, &e.res.facts)
                {
                    for (a, _) in matched {
                        if let Some(n) = c.fact_id(&engine::circuit_fact(&a)) {
                            push(&mut found, n);
                        }
                    }
                }
            }
            Anchor::Fact(atom) => {
                if let Some(n) = c.fact_id(&engine::circuit_fact(atom)) {
                    for t in up(e, n) {
                        push(&mut found, t);
                    }
                }
            }
        }
    }
    found
}

fn push(found: &mut Vec<NodeId>, n: NodeId) {
    if !found.contains(&n) {
        found.push(n);
    }
}

/// The fact nodes the firings of `n` by rule `id` read.
fn read_by(e: &Evaluated, n: NodeId, id: &str) -> Vec<NodeId> {
    let c = &e.res.circuit;
    let View::Fact { alts, .. } = c.view(n) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        if !children
            .iter()
            .any(|ch| matches!(c.view(*ch), View::Leaf(Leaf::Rule { id: r }) if r == id))
        {
            continue;
        }
        out.extend(
            children
                .iter()
                .copied()
                .filter(|ch| matches!(c.view(*ch), View::Fact { .. })),
        );
    }
    out
}

/// A contribution read up to the attribute it is part of; a resource's
/// `want` to the resource's attributes; anything else is itself.
fn up(e: &Evaluated, n: NodeId) -> Vec<NodeId> {
    let c = &e.res.circuit;
    let View::Fact { fact, .. } = c.view(n) else {
        return Vec::new();
    };
    match fact.pred.as_str() {
        "arg" => {
            let attrs: Vec<NodeId> = e
                .parents(n)
                .iter()
                .copied()
                .filter(|p| matches!(c.view(*p), View::Fact { fact, .. } if fact.pred == "attr"))
                .collect();
            if attrs.is_empty() { vec![n] } else { attrs }
        }
        "want" => {
            let (Some(t), Some(name)) = (fact.args.first(), fact.args.get(1)) else {
                return vec![n];
            };
            let attrs: Vec<NodeId> = e
                .res
                .facts
                .iter()
                .filter(|a| a.pred == "attr")
                .filter(|a| {
                    matches!((a.args.first(), a.args.get(1)),
                        (Some(Term::Val(x)), Some(Term::Val(y))) if x == t && y == name)
                })
                .filter_map(|a| c.fact_id(&engine::circuit_fact(a)))
                .collect();
            if attrs.is_empty() { vec![n] } else { attrs }
        }
        _ => vec![n],
    }
}

/// `dform why`'s text for each fact node, one after another: the same
/// entry (`why::fact_text`), a value's chain, else its derivation.
pub fn why_text(e: &Evaluated, facts: &[NodeId]) -> String {
    facts
        .iter()
        .map(|n| dform_core::why::fact_text(&e.res, &e.redact, *n, &e.keys))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The contributors hover: per attribute its collapsed value, the schema's
/// description of its path, the winning rank and every contribution with
/// its rank and owner; then the derivations. The deployment is not said:
/// the client has it from `dform/environment`.
pub fn hover(e: &Evaluated, facts: &[NodeId]) -> String {
    let c = &e.res.circuit;
    let docs = e.schema.docs();
    let mut out = String::new();
    for n in facts.iter().take(8) {
        let View::Fact { fact, alts, .. } = c.view(*n) else {
            continue;
        };
        if fact.pred != "attr" || fact.args.len() < 4 {
            out.push_str(&format!("`{}`\n\n", e.redact.fmt_atom(&fact.atom())));
            continue;
        }
        let s = |v: &Value| match v {
            Value::Str(s) => s.clone(),
            v => e.redact.fmt(v),
        };
        let (typ, name, key) = (s(&fact.args[0]), s(&fact.args[1]), s(&fact.args[2]));
        // A cell of an input, a let or an output as `why` names it:
        // `input main.vpc_net`.
        let cell = match (typ.as_str(), name.as_str()) {
            (dform_core::modules::INPUT | dform_core::modules::LET | "output", "") => {
                format!("{typ} {key}")
            }
            (dform_core::modules::INPUT | dform_core::modules::LET | "output", n) => {
                format!("{typ} {n}.{key}")
            }
            _ => dform_core::address::Address { typ, name }.attr(&key),
        };
        // The value in the formatter's layout, as `plan`, `why` and
        // `query` print one (R-124).
        let tree = dform_core::fmt::value::Tree::of(&fact.args[3], &|v| {
            let open = matches!(v, Value::Obj(_) | Value::List(_)) && !e.redact.is_secret(v);
            (!open).then(|| e.redact.fmt(v))
        });
        match dform_core::fmt::value::layout("", &tree, 80).as_slice() {
            [one] => out.push_str(&format!("**{cell}** = `{one}`\n\n")),
            lines => out.push_str(&format!(
                "**{cell}** =\n```text\n{}\n```\n\n",
                lines.join("\n")
            )),
        }
        if let (Value::Str(t), Value::Str(p)) = (&fact.args[0], &fact.args[2])
            && let Some(d) = docs.get(&(t.as_str(), p.as_str()))
        {
            out.push_str(&format!("{d}\n\n"));
        }
        let mut contributions = Vec::new();
        for a in alts.iter().take(1) {
            let View::Times { children, .. } = c.view(*a) else {
                continue;
            };
            for ch in children {
                let View::Fact { fact: f, .. } = c.view(*ch) else {
                    continue;
                };
                if f.pred != "arg" {
                    continue;
                }
                let rank = match f.args.get(4) {
                    Some(Value::Str(r)) => r.clone(),
                    _ => continue,
                };
                let value = f.args.get(3).map(|v| e.redact.fmt(v)).unwrap_or_default();
                contributions.push((rank, value, owner(e, *ch)));
            }
        }
        if let Some(best) = contributions
            .iter()
            .filter_map(|(r, _, _)| Rank::parse(r))
            .max()
        {
            out.push_str(&format!("winning rank: {}\n\n", best.name()));
        }
        for (rank, value, owner) in &contributions {
            out.push_str(&format!("- rank {rank}: `{value}` by {owner}\n"));
        }
        out.push('\n');
    }
    if facts.len() > 8 {
        out.push_str(&format!("... and {} more\n\n", facts.len() - 8));
    }
    out.push_str("```text\n");
    out.push_str(&why_text(e, &facts[..facts.len().min(8)]));
    out.push_str("```\n");
    out
}

/// Who wrote contribution `n`: its rule and where it is written, with the
/// pack or module instance, or the place of a stated fact.
fn owner(e: &Evaluated, n: NodeId) -> String {
    let c = &e.res.circuit;
    let View::Fact { alts, .. } = c.view(n) else {
        return "?".into();
    };
    let mut owners = Vec::new();
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        for ch in children {
            let o = match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => match c.rule_at(id) {
                    Some(at) => format!("{id} ({at})"),
                    None => id.clone(),
                },
                View::Leaf(Leaf::Base { span }) => span.clone(),
                View::Leaf(Leaf::Input { source }) => source.clone(),
                _ => continue,
            };
            if !owners.contains(&o) {
                owners.push(o);
            }
        }
    }
    if owners.is_empty() {
        "?".into()
    } else {
        owners.join(", ")
    }
}

/// The hover at byte `at` of `path`, from the project's files and
/// evaluations; `None` where point is on nothing with content.
pub fn hover_at(p: &refs::Project, path: &Path, at: usize) -> Option<String> {
    let files = p.parse();
    let d = p.decls(&files);
    // The token at point, a name in an interpolation hole included.
    let named = d.at(&files, path, at)?;
    let t = named.token.clone();
    // `check` is a word only where it opens a refinement.
    let refinement = t.kind() == SyntaxKind::IDENT
        && t.parent().is_some_and(|n| {
            n.kind() == SyntaxKind::REFINEMENT && n.first_token().as_ref() == Some(&t)
        });
    if t.kind().is_keyword() || refinement {
        // A keyword where a name is expected is that name.
        let as_name = t
            .parent()
            .is_some_and(|n| matches!(n.kind(), SyntaxKind::CHAIN | SyntaxKind::BLOCK_PATH));
        return (!as_name)
            .then(|| reference::reference(t.text(), false))
            .flatten()
            .map(reference_md);
    }
    if t.kind() == SyntaxKind::QUANTITY {
        return Some(quantity_md(t.text()));
    }
    if t.kind() != SyntaxKind::IDENT {
        return None;
    }
    let contributors = || -> Option<String> {
        p.evaluated.iter().find_map(|e| {
            let in_file = |id: u32| e.files.get(&id).is_some_and(|f| f == path);
            let facts = targets(e, &in_file, at);
            (!facts.is_empty()).then(|| hover(e, &facts))
        })
    };
    let joined = |a: Option<String>, b: Option<String>| match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}\n---\n\n{b}")),
        (a, b) => a.or(b),
    };
    // The values the selected deployment gives what is read here (R-20):
    // the cell's value, its winning rank, every contribution with its
    // owner and the derivation, per evaluation.
    let in_hole = named.range != t.text_range();
    let declaration = named.is_declaration();
    let values = |w: &What| -> Option<String> {
        let (sym, path) = match t.parent() {
            _ if in_hole => return None,
            Some(c) if c.kind() == SyntaxKind::CHAIN => cells::read_at(&d, &c, &t)?,
            _ => match w {
                What::Name(s, true) => (s.clone(), String::new()),
                _ => return None,
            },
        };
        let found = cells::cells(&p.evaluated, &d, &files, &sym, &path);
        let label = cells::label(&found)?;
        let read: String = t
            .parent()
            .filter(|c| c.kind() == SyntaxKind::CHAIN)
            .map(|c| {
                let end = usize::from(t.text_range().end() - c.text_range().start());
                c.text().to_string()[..end].to_string()
            })
            .unwrap_or_else(|| t.text().to_string());
        let mut out = format!("**{read}** = `{label}`\n\n");
        let mut by: Vec<(&Evaluated, Vec<NodeId>)> = Vec::new();
        for c in &found {
            match by.iter_mut().find(|(e, _)| std::ptr::eq(*e, c.e)) {
                Some((_, ns)) => ns.push(c.node),
                None => by.push((c.e, vec![c.node])),
            }
        }
        for (e, nodes) in by {
            out.push_str(&format!("in {}:\n\n", e.deployment));
            out.push_str(&hover(e, &nodes));
        }
        Some(out)
    };
    // A name read bare in a component's body: what it denotes in each
    // copy the deployment makes, as `why COPY.NAME` says it (R-184).
    let component = t
        .parent()
        .filter(|c| c.kind() == SyntaxKind::CHAIN && c.first_token().as_ref() == Some(&t))
        .filter(|_| !declaration && !in_hole)
        .and_then(|c| c.ancestors().find(|a| a.kind() == SyntaxKind::COMPONENT))
        .and_then(|c| names::declared_name(&c));
    let in_copies = || -> Option<String> {
        let c = component.as_ref()?;
        let lines: Vec<String> = p
            .evaluated
            .iter()
            .flat_map(|e| {
                dform_core::why::in_component(
                    c.text(),
                    t.text(),
                    &e.res,
                    &e.redact,
                    &e.keys,
                    Some(&p.dir),
                )
            })
            .map(|l| format!("`{l}`\n\n"))
            .collect();
        (!lines.is_empty()).then(|| lines.concat())
    };
    let what = match named.what {
        // Of a name several resources share, the first's.
        What::Names(syms) => syms
            .into_iter()
            .next()
            .map_or(What::Other, |s| What::Name(s, false)),
        w => w,
    };
    let out = match what {
        What::Name(sym, _) => {
            let decls: Vec<SyntaxNode> = d
                .occurrences(&files, &sym)
                .into_iter()
                .filter(names::Named::is_declaration)
                .filter_map(|n| statement(&n.token))
                .collect();
            if decls.is_empty()
                && let Some(r) = builtin(&t)
            {
                return Some(reference_md(r));
            }
            let documented = decls
                .iter()
                .find(|n| doc::comment(n).is_some())
                .or(decls.first());
            let own = match &sym {
                // A relation a copy exports, read through it (`n.p(..)`):
                // the output's declaration too.
                Symbol::Predicate(_, name) => joined(
                    output_md(&files, &t),
                    joined(signature_md(p, name), documented.map(item_md)),
                ),
                // A let with parameters (R-187): its columns as inferred.
                Symbol::Let(_, name)
                    if documented
                        .is_some_and(|n| n.children().any(|c| c.kind() == SyntaxKind::PARAMS)) =>
                {
                    joined(documented.map(item_md), signature_md(p, name))
                }
                Symbol::Module(m) => module_md(&files, m),
                Symbol::Instance(m, _) => joined(documented.map(item_md), module_md(&files, m)),
                _ => documented.map(item_md),
            };
            let w = What::Name(sym.clone(), declaration);
            match sym {
                Symbol::Value(..) | Symbol::Let(..) | Symbol::Field(..) | Symbol::Output(..) => {
                    joined(own, values(&w).or_else(contributors))
                }
                Symbol::Predicate(..) | Symbol::Resource(..) => joined(own, contributors()),
                _ => own,
            }
        }
        What::Path => values(&What::Path)
            .or_else(contributors)
            .or_else(|| field_doc(p, &t)),
        What::Type(typ) => type_md(p, &t).or_else(|| run_time_type(&files, &typ)),
        What::Names(_) | What::Provider | What::Variable | What::Key | What::Other => builtin(&t)
            .map(reference_md)
            .or_else(|| output_md(&files, &t)),
    };
    joined(in_copies(), out)
}

/// A relation's columns as declared or inferred (R-34): `az(name:
/// string, index: int)`; a component's relation by its copies'.
fn signature_md(p: &refs::Project, name: &str) -> Option<String> {
    let sigs: std::collections::BTreeSet<String> = p
        .evaluated
        .iter()
        .flat_map(|e| e.signatures.values())
        .filter(|s| {
            s.pred == name
                || s.pred
                    .rsplit_once("::")
                    .is_some_and(|(_, last)| last == name)
        })
        .map(|s| {
            let mut s = s.clone();
            s.pred = name.to_string();
            s.to_string()
        })
        .collect();
    (!sigs.is_empty()).then(|| {
        format!(
            "```dform\n{}\n```\n",
            sigs.into_iter().collect::<Vec<_>>().join("\n")
        )
    })
}

/// A quantity literal (R-66): its type, canonical form and base value;
/// `500m` both of its readings.
fn quantity_md(text: &str) -> String {
    use dform_core::quantity::{self, Dim, Literal, Quantity};
    let line = |q: &Quantity| format!("`{}`: {q} = {}", q.dim().name(), q.base());
    let body = match quantity::literal(text) {
        Ok(Literal::Known(q)) => line(&q),
        Ok(Literal::Ambiguous) => {
            let readings: Vec<String> = [Dim::Cpu, Dim::Duration]
                .into_iter()
                .filter_map(|d| quantity::read(d, text).ok())
                .map(|q| format!("- in a {} position, {}", q.dim().name(), line(&q)))
                .collect();
            format!("read by its position's type:\n\n{}", readings.join("\n"))
        }
        Err(e) => e,
    };
    format!("```dform\n{text}\n```\n\n{body}\n")
}

/// A reference entry: its signature, summary and example.
pub fn reference_md(r: &Reference) -> String {
    format!(
        "```dform\n{}\n```\n\n{}\n\n```dform\n{}\n```\n",
        r.signature, r.summary, r.example
    )
}

/// The builtin a call's name is (`inet.subnet(..)`, `int.round(..)`), its
/// whole dotted name.
fn builtin(t: &SyntaxToken) -> Option<&'static Reference> {
    let chain = t.parent().filter(|c| c.kind() == SyntaxKind::CHAIN)?;
    chain.parent().filter(|c| c.kind() == SyntaxKind::CALL)?;
    let name: String = chain
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|x| !x.kind().is_trivia())
        .map(|x| x.text().to_string())
        .collect();
    reference::reference(&name, true)
}

/// The statement a declaration's name token stands in.
fn statement(t: &SyntaxToken) -> Option<SyntaxNode> {
    use SyntaxKind::*;
    t.parent()?
        .ancestors()
        .find(|n| doc::item(n).is_some() || n.kind() == LET)
}

/// A doc comment's description and pairs.
fn pairs_md(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if k == "description" {
            out.push_str(&format!("\n{v}\n"));
        }
    }
    let rest: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k != "description")
        .map(|(k, v)| format!("- **{k}**: {v}\n"))
        .collect();
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(&rest.concat());
    }
    out
}

/// A statement's first line (an alias: its definition), and its doc
/// comment.
fn item_md(n: &SyntaxNode) -> String {
    let mut out = format!("```dform\n{}\n```\n", doc::header(n));
    if let Some((_, pairs)) = doc::comment(n) {
        out.push_str(&pairs_md(&pairs));
    }
    out
}

/// The component `m` (a path's last segment) some file declares.
fn module_node(files: &[Parsed], m: &str) -> Option<SyntaxNode> {
    let m = m.rsplit('.').next().unwrap_or(m);
    files.iter().flat_map(|f| f.tree.descendants()).find(|n| {
        n.kind() == SyntaxKind::COMPONENT && names::declared_name(n).is_some_and(|t| t.text() == m)
    })
}

/// A module's statements of `kind` (`INPUT`, `OUTPUT_DECL`) by name, in
/// order: an output declared (`output k: T`) and defined (`output k = t`)
/// is one, its typed line shown, its doc comment from either.
fn members(module: &SyntaxNode, kind: SyntaxKind) -> Vec<(String, String, Option<String>)> {
    let mut out: Vec<(String, String, Option<String>)> = Vec::new();
    let Some(block) = module
        .children()
        .find(|c| c.kind() == SyntaxKind::STMT_BLOCK)
    else {
        return out;
    };
    for n in block.children().filter(|n| n.kind() == kind) {
        let Some((_, name)) = doc::item(&n) else {
            continue;
        };
        let typed = n.children().any(|c| c.kind() == SyntaxKind::TYPE_EXPR)
            || n.kind() == SyntaxKind::INPUT;
        let description = doc::comment(&n).and_then(|(_, ps)| {
            ps.into_iter()
                .find(|(k, _)| k == "description")
                .map(|(_, v)| v)
        });
        match out.iter_mut().find(|(n, _, _)| *n == name) {
            Some(m) => {
                if typed {
                    m.1 = doc::header(&n);
                }
                if m.2.is_none() {
                    m.2 = description;
                }
            }
            None => out.push((name, doc::header(&n), description)),
        }
    }
    out
}

/// A module's first line and doc comment, then its inputs and outputs with
/// theirs.
fn module_md(files: &[Parsed], m: &str) -> Option<String> {
    let module = module_node(files, m)?;
    let mut out = item_md(&module);
    for (kind, title) in [
        (SyntaxKind::INPUT, "inputs"),
        (SyntaxKind::OUTPUT_DECL, "outputs"),
    ] {
        let ms = members(&module, kind);
        if ms.is_empty() {
            continue;
        }
        out.push_str(&format!("\n**{title}**\n\n"));
        for (_, header, description) in ms {
            match description {
                Some(d) => out.push_str(&format!("- `{header}`: {}\n", d.replace('\n', " "))),
                None => out.push_str(&format!("- `{header}`\n")),
            }
        }
    }
    Some(out)
}

/// An output read through its instance, `n.k` or `c[e].k`: the output's
/// declaration and doc comment.
fn output_md(files: &[Parsed], t: &SyntaxToken) -> Option<String> {
    let chain = t.parent().filter(|c| c.kind() == SyntaxKind::CHAIN)?;
    let text = chain.text().to_string();
    let segs: Vec<&str> = text.split('.').collect();
    let (m, k) = match segs.as_slice() {
        [m, k, ..] => match m.split_once('[') {
            Some((c, _)) => (c.to_string(), *k),
            // The instance `n`'s component.
            None => {
                let path = files
                    .iter()
                    .flat_map(|f| f.tree.descendants())
                    .filter(|n| n.kind() == SyntaxKind::RESOURCE)
                    .map(|n| dform_core::syntax::resolve::copy_parts(&n))
                    .find(|(path, name)| name == m && module_node(files, path).is_some())?
                    .0;
                (path, *k)
            }
        },
        _ => return None,
    };
    if k != t.text() {
        return None;
    }
    let module = module_node(files, &m)?;
    let (_, header, description) = members(&module, SyntaxKind::OUTPUT_DECL)
        .into_iter()
        .find(|(n, _, _)| n == k)?;
    let mut out = format!("```dform\n{header}\n```\n\noutput of component `{m}`\n");
    if let Some(d) = description {
        out.push_str(&format!("\n{d}\n"));
    }
    Some(out)
}

/// The description the evaluated schemas give `typ`'s `path`.
fn schema_doc(p: &refs::Project, typ: &str, path: &str) -> Option<String> {
    p.evaluated
        .iter()
        .find_map(|e| e.schema.docs().get(&(typ, path)).map(|d| d.to_string()))
}

/// A resource block's field, with no evaluation to read: its path's
/// description.
fn field_doc(p: &refs::Project, t: &SyntaxToken) -> Option<String> {
    let path = t.parent().filter(|n| n.kind() == SyntaxKind::BLOCK_PATH)?;
    let resource = path
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::RESOURCE)?;
    let typ = names::header(&resource)?.typ;
    let path = path.text().to_string().replace(' ', "");
    let d = schema_doc(p, &typ, &path)?;
    Some(format!("**{typ} .{path}**\n\n{d}\n"))
}

/// A type no schema declares whose namespace is a provider's: the
/// provider says it at run time (Kubernetes' kinds), so it has no
/// definition to go to.
fn run_time_type(files: &[Parsed], typ: &str) -> Option<String> {
    let ns = typ.split('.').next()?;
    let declared = files.iter().flat_map(|f| f.tree.descendants()).any(|n| {
        // `use ovh as ca` names the namespace `ca` (R-115).
        dform_core::syntax::resolve::maybe_provider_use(&n).is_some()
            && dform_core::syntax::resolve::use_parts(&n).1 == ns
    });
    (declared && typ.contains('.'))
        .then(|| format!("resource type `{typ}`\n\ndeclared by provider {ns} at run time\n"))
}

/// A schema type's name: its description.
fn type_md(p: &refs::Project, t: &SyntaxToken) -> Option<String> {
    // The dotted run of names the token is in.
    let glued = |x: &SyntaxToken| matches!(x.kind(), SyntaxKind::IDENT | SyntaxKind::DOT);
    let mut first = t.clone();
    while let Some(prev) = first.prev_token().filter(glued) {
        first = prev;
    }
    let mut typ = String::new();
    let mut cur = Some(first);
    while let Some(x) = cur.filter(glued) {
        typ.push_str(x.text());
        cur = x.next_token();
    }
    let typ = typ.trim_matches('.').to_string();
    let known = p.evaluated.iter().any(|e| {
        e.schema
            .facts
            .iter()
            .any(|f| matches!(f.args.first(), Some(Term::Val(Value::Str(x))) if *x == typ))
    });
    if !known {
        return None;
    }
    let mut out = format!("resource type `{typ}`\n");
    if let Some(d) = schema_doc(p, &typ, "") {
        out.push_str(&format!("\n{d}\n"));
    }
    Some(out)
}
