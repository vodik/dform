//! Quick fixes: the code actions of the diagnostics that carry a fix. The
//! compiler's own (a grant a policy pack lacks, an unknown name to quote, a
//! predicate with both facts and rules to declare `mixed`) come with the
//! core's diagnostic; the evaluation's are made here from its facts: the
//! collision lint (interpolate the key, or say `isolated = true`), a
//! required attribute no contribution sets (a typed placeholder), and a
//! ref to an address no rule wants (guard the block on it). Every edit is
//! formatted by `dform fmt`'s formatter when the file was formatted.

use crate::analysis::{self, Evaluated, Outcome, Reader, Where};
use dform_core::ast::Term;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use dform_core::value::Value;
use dform_core::{engine, ir, provider, stack, transform};
use rowan::TextSize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A fix and the diagnostic it is for.
#[derive(Debug, Clone)]
pub struct Action {
    pub title: String,
    /// What the diagnostic's message says (a part of it).
    pub message: String,
    /// Where the diagnostic is published: a file and a byte range; none
    /// for one at the top of the stack's file, known by its message.
    pub at: Option<(PathBuf, usize, usize)>,
    /// Per file, each byte range and the text that replaces it.
    pub edits: BTreeMap<PathBuf, Vec<(usize, usize, String)>>,
}

impl Action {
    fn new(title: String, message: String, at: Option<(PathBuf, usize, usize)>) -> Action {
        Action {
            title,
            message,
            at,
            edits: BTreeMap::new(),
        }
    }

    fn edit(mut self, file: &Path, start: usize, end: usize, text: String) -> Action {
        self.edits
            .entry(file.to_path_buf())
            .or_default()
            .push((start, end, text));
        self
    }
}

/// The fixes the compiler's diagnostics of an evaluation carry, as the
/// evaluation resolved them.
pub fn compiled(outcome: &Outcome) -> Vec<Action> {
    let mut out = Vec::new();
    for p in &outcome.problems {
        let Where::Bytes(file, start, end) = &p.at else {
            continue;
        };
        for f in &p.fixes {
            let mut a = Action::new(
                f.title.clone(),
                p.message.clone(),
                Some((file.clone(), *start, *end)),
            );
            for (file, start, end, text) in &f.edits {
                a = a.edit(file, *start, *end, text.clone());
            }
            out.push(a);
        }
    }
    out
}

/// The fixes of the diagnostics evaluation `e` of the stack at `stack`
/// found.
pub fn evaluated(e: &Evaluated, stack: &Path, read: Reader) -> Vec<Action> {
    let mut out = dangling_refs(e, read);
    out.extend(collisions(e, stack, read));
    out.extend(required(e, read));
    out
}

/// Where fact `a` is published: the rule (or stated fact) that derived it,
/// as `analysis::provenance` places it; a stated fact's place is the rest
/// of its line.
fn place(
    e: &Evaluated,
    a: &dform_core::ast::Atom,
    read: Reader,
) -> Option<(PathBuf, usize, usize)> {
    let id = e.res.circuit.fact_id(&engine::circuit_fact(a))?;
    placed(e, id, read)
}

fn placed(
    e: &Evaluated,
    id: dform_core::circuit::NodeId,
    read: Reader,
) -> Option<(PathBuf, usize, usize)> {
    match analysis::provenance(e, id).0? {
        Where::Span(s) => Some((
            e.files.get(&s.file)?.clone(),
            s.start as usize,
            s.end as usize,
        )),
        // `stacks/dform.df:18:3 (arg)`, relative to the project's root.
        Where::Place(p) => {
            let at = p.split(' ').next()?;
            let mut parts = at.rsplitn(3, ':');
            let (col, line, name) = (parts.next()?, parts.next()?, parts.next()?);
            let (line, col): (usize, usize) = (line.parse().ok()?, col.parse().ok()?);
            let f = std::env::current_dir().ok()?.join(name);
            let f = std::fs::canonicalize(&f).unwrap_or(f);
            let text = read(&f).ok()?;
            let start_of_line = text
                .split_inclusive('\n')
                .take(line.checked_sub(1)?)
                .map(str::len)
                .sum::<usize>();
            let rest = &text[start_of_line..];
            let start = start_of_line
                + rest
                    .char_indices()
                    .nth(col.checked_sub(1)?)
                    .map_or(rest.len(), |(i, _)| i);
            let end = start_of_line + rest.find('\n').unwrap_or(rest.len());
            Some((f, start, end))
        }
        _ => None,
    }
}

fn tree(read: Reader, file: &Path) -> Option<(String, SyntaxNode)> {
    let text = read(file).ok()?;
    let root = dform_core::syntax::parser::parse(&text).syntax();
    Some((text, root))
}

/// The resource statement around byte `at`.
fn resource_stmt(root: &SyntaxNode, at: usize) -> Option<SyntaxNode> {
    let t = root
        .token_at_offset(TextSize::from(u32::try_from(at).ok()?))
        .right_biased()?;
    t.parent_ancestors()
        .find(|n| n.kind() == SyntaxKind::RESOURCE)
}

/// The block of the resource statement that holds byte `at`.
fn resource_block(root: &SyntaxNode, at: usize) -> Option<SyntaxNode> {
    resource_stmt(root, at)?
        .children()
        .find(|n| n.kind() == SyntaxKind::BLOCK)
}

/// A ref to an address no rule wants: guard the resource block that holds
/// it on the address being wanted (`} where "other" in net.vpc`), in its
/// clause. Offered where the address is written as it is (the block has
/// no clause, or names it quoted).
fn dangling_refs(e: &Evaluated, read: Reader) -> Vec<Action> {
    let mut out = Vec::new();
    for a in &e.res.facts {
        if a.pred != "deny" {
            continue;
        }
        let (Some(Term::Val(Value::Str(m))), Some(Term::Val(Value::Obj(ctx)))) =
            (a.args.first(), a.args.get(1))
        else {
            continue;
        };
        if m != transform::DANGLING_REF {
            continue;
        }
        let (Some(Value::Str(typ)), Some(Value::Str(addr))) = (ctx.get("type"), ctx.get("addr"))
        else {
            continue;
        };
        let Some(at) = place(e, a, read) else {
            continue;
        };
        let (file, start, end) = at.clone();
        let Some((text, root)) = tree(read, &file) else {
            continue;
        };
        let Some(stmt) = resource_stmt(&root, start) else {
            continue;
        };
        let clause = stmt.children().find(|n| n.kind() == SyntaxKind::CLAUSE);
        let quoted = format!("\"{addr}\"");
        // A clause that binds rows: the ref may name each row's own; guard
        // only a ref written as the address itself.
        if clause.is_some() && !text.get(start..end).is_some_and(|w| w.contains(&quoted)) {
            continue;
        }
        let lit = format!("{quoted} in {typ}");
        // The block takes one clause, after it (R-1): the guard joins it,
        // or is it.
        let (at_edit, insert) = match clause
            .as_ref()
            .and_then(|c| c.children().find(|b| b.kind() == SyntaxKind::BODY))
        {
            Some(body)
                if body
                    .first_token()
                    .is_some_and(|t| t.kind() == SyntaxKind::L_BRACE) =>
            {
                let Some(close) = body.last_token() else {
                    continue;
                };
                let at = usize::from(close.text_range().start());
                (at, format!("  {lit}\n"))
            }
            Some(body) => (usize::from(body.text_range().end()), format!(", {lit}")),
            None => match stmt
                .children()
                .find(|n| n.kind() == SyntaxKind::BLOCK)
                .and_then(|b| b.last_token())
                .filter(|t| t.kind() == SyntaxKind::R_BRACE)
            {
                Some(b) => (usize::from(b.text_range().end()), format!(" where {lit}")),
                None => continue,
            },
        };
        out.push(
            Action::new(
                format!("guard the block on {typ} {addr} existing: `{lit}`"),
                transform::DANGLING_REF.to_string(),
                Some(at),
            )
            .edit(&file, at_edit, at_edit, insert),
        );
    }
    out
}

/// The collision lint: derive the name from the key (`"fixed-${env}"`, where
/// the name is a string written in the stack's own file, where the key is
/// in scope), or say `isolated = true` on the stack.
fn collisions(e: &Evaluated, stack_file: &Path, read: Reader) -> Vec<Action> {
    let Ok(cfg) = stack::config(&e.program) else {
        return Vec::new();
    };
    if cfg.keys.is_empty() || cfg.isolated {
        return Vec::new();
    }
    let keys: Vec<String> = cfg.keys.iter().map(|(k, _)| k.clone()).collect();
    let isolated = isolate(stack_file, read);
    let mut out = Vec::new();
    for c in &e.collisions {
        let message = e.redact.text(&c.text);
        let at = e
            .res
            .circuit
            .fact_id(&c.fact)
            .and_then(|id| placed(e, id, read));
        if let (Some(Value::Str(name)), Some((file, start, end))) = (c.fact.args.get(3), &at)
            && file == stack_file
            && let Ok(text) = read(file)
        {
            let quoted = format!("\"{name}\"");
            let written = text.get(*start..*end).unwrap_or_default();
            if written.matches(&quoted).count() == 1 {
                let i = start + written.find(&quoted).unwrap_or_default();
                let with = format!("\"{name}-${{{}}}\"", keys[0]);
                out.push(
                    Action::new(
                        format!("derive the name from the key: {with}"),
                        message.clone(),
                        None,
                    )
                    .edit(file, i, i + quoted.len(), with),
                );
            }
        }
        if let Some((start, end, text)) = &isolated {
            out.push(
                Action::new("say `isolated = true` on the stack".into(), message, None).edit(
                    stack_file,
                    *start,
                    *end,
                    text.clone(),
                ),
            );
        }
    }
    out
}

/// The edit that says `isolated = true` in the `stack` statement of
/// `file`: an entry of its block.
fn isolate(file: &Path, read: Reader) -> Option<(usize, usize, String)> {
    let (_, root) = tree(read, file)?;
    let block = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::STACK)?
        .children()
        .find(|n| n.kind() == SyntaxKind::BLOCK)?;
    let entries: Vec<SyntaxNode> = block
        .children()
        .filter(|n| n.kind() == SyntaxKind::ASSIGN)
        .collect();
    let named = |n: &SyntaxNode| n.first_token().is_some_and(|t| t.text() == "isolated");
    if let Some(e) = entries.iter().find(|e| named(e)) {
        let r = e.text_range();
        return Some((r.start().into(), r.end().into(), "isolated = true".into()));
    }
    if let Some(last) = entries.last() {
        let end = usize::from(last.text_range().end());
        return Some((end, end, ", isolated = true".into()));
    }
    let close = block
        .children_with_tokens()
        .find(|t| t.kind() == SyntaxKind::R_BRACE)?;
    let at = usize::from(close.text_range().start());
    Some((at, at, " isolated = true ".into()))
}

/// A placeholder of a schema type.
fn placeholder(ty: &str) -> &'static str {
    match ty {
        "int" | "number" | "float" => "0",
        "bool" => "false",
        "list" | "set" => "[]",
        "map" | "object" => "{}",
        _ => "\"\"",
    }
}

/// A required attribute no contribution sets (what a provider's plan
/// refuses, naming the first): each such path of the resource, with a
/// typed placeholder, at the end of its block. Paths inside a list element
/// are the element's.
fn required(e: &Evaluated, read: Reader) -> Vec<Action> {
    let Ok(resources) = ir::compile_resources(e.res.facts.iter().cloned(), &e.schema) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in &resources {
        let typ = r.addr.typ.as_str();
        let doc = engine::value_to_json(&r.attrs);
        let missing: Vec<(&String, &str)> = e
            .schema
            .attrs
            .iter()
            .filter(|((t, p), spec)| {
                t == typ
                    && spec.has("required")
                    && !e.schema.in_list(typ, p)
                    && provider::get_path(&doc, p).is_none()
            })
            .map(|((_, p), spec)| (p, placeholder(&spec.ty)))
            .collect();
        let Some((first, _)) = missing.first() else {
            continue;
        };
        let want = e.res.facts.iter().find(|a| {
            a.pred == "want"
                && matches!(
                    (a.args.first(), a.args.get(1)),
                    (Some(Term::Val(Value::Str(t))), Some(Term::Val(Value::Str(n))))
                        if *t == r.addr.typ && *n == r.addr.name
                )
        });
        let Some((file, start, _)) = want.and_then(|w| place(e, w, read)) else {
            continue;
        };
        let Some((text, root)) = tree(read, &file) else {
            continue;
        };
        let Some(close) = resource_block(&root, start).and_then(|b| {
            b.children_with_tokens()
                .filter(|t| t.kind() == SyntaxKind::R_BRACE)
                .last()
        }) else {
            continue;
        };
        let at = usize::from(close.text_range().start());
        let lines: Vec<String> = missing.iter().map(|(p, v)| format!("{p} = {v}")).collect();
        // Before the `}`, each on a line of its own.
        let before = text[..at].trim_end_matches([' ', '\t']);
        let lead = if before.ends_with('\n') { "" } else { "\n" };
        let names: Vec<&str> = missing.iter().map(|(p, _)| p.as_str()).collect();
        out.push(
            Action::new(
                format!("set the required {}", names.join(", ")),
                format!("{}: required attribute {first} is not set", r.addr),
                None,
            )
            .edit(&file, at, at, format!("{lead}{}\n", lines.join("\n"))),
        );
    }
    out
}

/// One file's edits as one edit: applied, then formatted by `dform fmt`'s
/// formatter when the file was formatted before, and trimmed to what
/// changed.
pub fn apply(name: &str, text: &str, edits: &[(usize, usize, String)]) -> (usize, usize, String) {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|(s, e, _)| (*s, *e));
    let mut out = text.to_string();
    for (s, e, t) in edits.iter().rev() {
        let (s, e) = ((*s).min(out.len()), (*e).min(out.len()));
        out.replace_range(s..e.max(s), t);
    }
    let formatted = |s: &str| dform_core::fmt::format_source(name, s).ok();
    if formatted(text).as_deref() == Some(text)
        && let Some(f) = formatted(&out)
    {
        out = f;
    }
    let prefix = text
        .char_indices()
        .zip(out.chars())
        .find(|((_, a), b)| a != b)
        .map_or(text.len().min(out.len()), |((i, _), _)| i);
    let suffix = text[prefix..]
        .chars()
        .rev()
        .zip(out[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    (
        prefix,
        text.len() - suffix,
        out[prefix..out.len() - suffix].to_string(),
    )
}
