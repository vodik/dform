//! Quick fixes: the code actions of the diagnostics that carry a fix. The
//! compiler's own (a grant a policy pack lacks, an unknown name to quote, a
//! predicate with both facts and rules to declare `mixed`) come with the
//! core's diagnostic; the evaluation's are made here from its facts: the
//! collision lint (interpolate the key, or say `isolated = true`), a
//! required attribute no contribution sets (a typed placeholder), and a
//! ref to an address no rule wants (guard the block on it). Every edit is
//! formatted by `dform fmt`'s formatter when the file was formatted.

use crate::analysis::{self, Evaluated, Reader, Where};
use dform_core::ast::{Span, Term};
use dform_core::diag::{self, Diagnostics};
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use dform_core::value::Value;
use dform_core::{engine, ir, lint, loader, provider, stack, transform};
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

/// A span as a file (relative to the working directory, the project's
/// root) and a byte range, while its source is registered.
fn resolve(span: Span, cwd: &Path) -> Option<(PathBuf, usize, usize)> {
    let (name, _, _) = diag::location(span)?;
    let f = cwd.join(name);
    let f = std::fs::canonicalize(&f).unwrap_or(f);
    Some((f, span.start as usize, span.end as usize))
}

/// The fixes the compiler's diagnostics of the program at `stack` carry:
/// it is loaded and lowered again (as the evaluation did) to have them.
pub fn compiled(stack: &Path, read: Reader) -> Vec<Action> {
    let _scope = diag::Scope::new();
    let cwd = std::env::current_dir().unwrap_or_default();
    let err = match loader::load_program_with(&[stack.to_path_buf()], read) {
        Ok(p) => match transform::lower(&p) {
            Ok(_) => return Vec::new(),
            Err(e) => e,
        },
        Err(e) => e,
    };
    let Some(Diagnostics(ds)) = err.chain().find_map(|x| x.downcast_ref::<Diagnostics>()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in ds {
        let Some(at) = resolve(d.span, &cwd) else {
            continue;
        };
        'fix: for f in &d.fixes {
            let mut a = Action::new(f.title.clone(), d.message.clone(), Some(at.clone()));
            for (span, text) in &f.edits {
                let Some((file, start, end)) = resolve(*span, &cwd) else {
                    continue 'fix;
                };
                a = a.edit(&file, start, end, text.clone());
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

/// The resource block around byte `at`.
fn resource_block(root: &SyntaxNode, at: usize) -> Option<SyntaxNode> {
    let t = root
        .token_at_offset(TextSize::from(u32::try_from(at).ok()?))
        .right_biased()?;
    let r = t
        .parent_ancestors()
        .find(|n| n.kind() == SyntaxKind::RESOURCE)?;
    r.children().find(|n| n.kind() == SyntaxKind::BLOCK)
}

/// An insertion of `line` after byte `at` of `text`, on a line of its own.
fn line_after(text: &str, at: usize, line: &str) -> String {
    let rest = &text[at..];
    let next_on_new_line = rest
        .find(|c: char| !c.is_whitespace())
        .is_none_or(|i| rest[..i].contains('\n'));
    if next_on_new_line {
        format!("\n{line}")
    } else {
        format!("\n{line}\n")
    }
}

/// A ref to an address no rule wants: guard the resource block that holds
/// it on the address being wanted (`if "other" in net.vpc`), after its
/// clauses. Offered where the address is written as it is (the block has
/// no `for`, or names it quoted).
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
        let Some(block) = resource_block(&root, start) else {
            continue;
        };
        let clauses: Vec<SyntaxNode> = block
            .children()
            .filter(|n| n.kind() == SyntaxKind::CLAUSE)
            .collect();
        let has_for = clauses.iter().any(|c| {
            c.first_token()
                .is_some_and(|t| t.kind() == SyntaxKind::FOR_KW)
        });
        let quoted = format!("\"{addr}\"");
        if has_for && !text.get(start..end).is_some_and(|w| w.contains(&quoted)) {
            continue;
        }
        let after = match clauses.last() {
            Some(c) => usize::from(c.text_range().end()),
            None => match block
                .children_with_tokens()
                .find(|t| t.kind() == SyntaxKind::L_BRACE)
            {
                Some(b) => usize::from(b.text_range().end()),
                None => continue,
            },
        };
        let guard = format!("if {quoted} in {typ}");
        out.push(
            Action::new(
                format!("guard the block on {typ} {addr} existing: `{guard}`"),
                transform::DANGLING_REF.to_string(),
                Some(at),
            )
            .edit(&file, after, after, line_after(&text, after, &guard)),
        );
    }
    out
}

/// The collision lint: derive the name from the key (`"fixed-{env}"`, where
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
    // `dform[env=staging] (env from its default)`: the deployment's name
    // is what the lint names.
    let deployment = e.deployment.split(" (").next().unwrap_or_default();
    let isolated = isolate(stack_file, read);
    let mut out = Vec::new();
    for c in lint::key_collisions(&e.res, &e.schema, &keys, deployment) {
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
                let with = format!("\"{name}-{{{}}}\"", keys[0]);
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
                format!(
                    "{typ}/{}: required attribute {first} is not set",
                    r.addr.name
                ),
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
