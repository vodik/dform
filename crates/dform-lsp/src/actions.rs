//! Quick fixes: the code actions of the diagnostics that carry a fix. The
//! compiler's own (an unknown name to quote, a predicate with both facts
//! and rules to declare `mixed`) come with the core's diagnostic; the
//! evaluation's are made here from its facts: the collision lint
//! (interpolate the key, or say `isolated = true` in dform.toml), a
//! required attribute no contribution sets (a typed placeholder), and a ref
//! to an address no rule wants (guard the block on it). Every edit of a
//! program is formatted by `dform fmt`'s formatter when the file was
//! formatted.

use crate::analysis::{self, Evaluated, Outcome, Reader, Where};
use dform_core::ast::Term;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use dform_core::value::Value;
use dform_core::{engine, report, resources, stack};
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
    let mut out = unanswered_reads(e, read);
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

/// A read of an address no rule wants (R-194): guard the resource block
/// that holds it on the address being wanted (`} where "other" in
/// net.vpc`), in its clause. Offered where the address is written as it
/// is (the block has no clause, or names it quoted).
fn unanswered_reads(e: &Evaluated, read: Reader) -> Vec<Action> {
    let mut out = Vec::new();
    for a in &e.res.facts {
        let Some(u) = report::Unanswered::of(a) else {
            continue;
        };
        let (typ, addr) = (&u.to.typ, &u.to.name);
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
                e.redact.text(&u.message()),
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
        for fact in &c.facts {
            let at = e
                .res
                .circuit
                .fact_id(fact)
                .and_then(|id| placed(e, id, read));
            let (Some(Value::Str(name)), Some((file, start, end))) = (fact.args.get(3), &at) else {
                continue;
            };
            if file != stack_file {
                continue;
            }
            let Ok(text) = read(file) else {
                continue;
            };
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
        if let Some((manifest, start, end, text)) = &isolated {
            out.push(
                Action::new("say `isolated = true` in dform.toml".into(), message, None).edit(
                    manifest,
                    *start,
                    *end,
                    text.clone(),
                ),
            );
        }
    }
    out
}

/// The edit that says `isolated = true` in dform.toml's table of the stack
/// `file` is, `[stacks.NAME]` (R-29): the manifest's path, and the edit.
fn isolate(file: &Path, read: Reader) -> Option<(PathBuf, usize, usize, String)> {
    let manifest = dform_core::project::manifest_root(file)?.join(dform_core::project::MANIFEST);
    let text = read(&manifest).ok()?;
    let header = format!("[stacks.{}]", dform_core::state::stack_name(file));
    let mut at = 0;
    let mut table = None;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if table.is_some() && t.starts_with('[') {
            break;
        }
        if t == header {
            table = Some(at + line.len());
        } else if table.is_some() && t.split('=').next().map(str::trim) == Some("isolated") {
            let end = at + line.trim_end_matches(['\n', '\r']).len();
            return Some((manifest, at, end, "isolated = true".into()));
        }
        at += line.len();
    }
    match table {
        Some(after) => Some((manifest, after, after, "isolated = true\n".into())),
        None => {
            let lead = if text.is_empty() || text.ends_with("\n\n") {
                ""
            } else if text.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            let end = text.len();
            Some((
                manifest,
                end,
                end,
                format!("{lead}{header}\nisolated = true\n"),
            ))
        }
    }
}

/// A placeholder of a schema type.
fn placeholder(ty: &str) -> &'static str {
    match ty.split('(').next().unwrap_or(ty) {
        "int" | "number" | "float" | "bytes" | "cpu" => "0",
        "duration" => "0s",
        "bool" => "false",
        "list" | "set" => "[]",
        "map" | "object" => "{}",
        _ => "\"\"",
    }
}

/// A required attribute no contribution sets (what the plan refuses,
/// R-184, `Schema::unset_required`): each such path of the resource, with a
/// typed placeholder, at the end of its block. Paths inside a list element
/// are the element's.
fn required(e: &Evaluated, read: Reader) -> Vec<Action> {
    let Ok(resources) = resources::compile_resources(e.res.facts.iter().cloned(), &e.schema) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in &resources {
        let typ = r.addr.typ.as_str();
        let doc = dform_core::spell::value_to_json(&r.attrs);
        let missing: Vec<(String, &str)> = e
            .schema
            .unset_required(typ, &doc)
            .into_iter()
            .filter_map(|p| {
                let ty = &e.schema.attr(typ, &p)?.ty;
                Some((p, placeholder(ty)))
            })
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
                e.schema.unset(typ, first),
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
