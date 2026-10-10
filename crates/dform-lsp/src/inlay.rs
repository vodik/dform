//! Inlay hints (R-20), read off the last evaluation of the selected
//! deployment: at the end of each resource header's line, the plan's
//! deformation of each object the block declares (`+ create`, `~ update`,
//! `- delete`, `undeformed`, `~ pending on k3s.server.ip`), as `dform
//! plan` has it; after a read of an input, a `let`, an output, an object
//! input's field or a resource's attribute, its value (`cells`). A secret
//! shows as the redactor prints it.

use crate::analysis::{self, Evaluated, Planned, Where};
use crate::cells;
use crate::refs::Project;
use dform_core::address::Address;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use std::path::Path;

/// A value longer than this is cut, with `…`.
const WIDTH: usize = 60;

/// One hint: shown at byte `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub at: usize,
    pub label: String,
    pub tooltip: String,
}

/// The hints of the file at `path` whose place is in bytes `range`.
pub fn hints(p: &Project, path: &Path, range: (usize, usize)) -> Vec<Hint> {
    let files = p.parse();
    let d = p.decls(&files);
    let Some(f) = files.iter().find(|f| f.path == path) else {
        return Vec::new();
    };
    let inside = |at: usize| range.0 <= at && at <= range.1;
    let mut out = Vec::new();
    for n in f.tree.descendants() {
        match n.kind() {
            SyntaxKind::RESOURCE => {
                let start: usize = n.text_range().start().into();
                let at = f.text[start..]
                    .find('\n')
                    .map_or(f.text.len(), |i| start + i);
                if !inside(at) {
                    continue;
                }
                if let Some(h) = action(p, path, &f.text, &n, at) {
                    out.push(h);
                }
            }
            SyntaxKind::CHAIN if is_read(&n) => {
                let at: usize = n.text_range().end().into();
                let Some(last) = n
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .filter(|t| t.kind() == SyntaxKind::IDENT)
                    .last()
                else {
                    continue;
                };
                if !inside(at) {
                    continue;
                }
                let Some((sym, path)) = cells::read_at(&d, &n, &last) else {
                    continue;
                };
                let found = cells::cells(&p.evaluated, &d, &files, &sym, &path);
                if let Some(label) = cells::label(&found) {
                    let deployments: Vec<String> = found
                        .iter()
                        .map(|c| c.e.deployment.clone())
                        .fold(Vec::new(), |mut v, x| {
                            if !v.contains(&x) {
                                v.push(x);
                            }
                            v
                        });
                    out.push(Hint {
                        at,
                        label: format!("= {}", cut(&label)),
                        tooltip: format!("{label}\n\nin {}", deployments.join(", ")),
                    });
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|h| h.at);
    out
}

fn cut(s: &str) -> String {
    match s.char_indices().nth(WIDTH) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Whether a chain is read where it stands: not the left of a `set`
/// block's entry (a write), not a call's name, not inside a type.
fn is_read(chain: &SyntaxNode) -> bool {
    let Some(parent) = chain.parent() else {
        return false;
    };
    match parent.kind() {
        SyntaxKind::CALL => false,
        SyntaxKind::ASSIGN => parent.first_child().as_ref() != Some(chain),
        _ => !chain
            .ancestors()
            .any(|a| matches!(a.kind(), SyntaxKind::TYPE_EXPR | SyntaxKind::DECL)),
    }
}

/// The objects the resource block `n` of the file at `path` declares in
/// evaluation `e`: the addresses whose `want` it writes.
fn addresses(e: &Evaluated, path: &Path, text: &str, n: &SyntaxNode) -> Vec<Address> {
    let c = &e.res.circuit;
    let (start, end): (usize, usize) = (n.text_range().start().into(), n.text_range().end().into());
    let here = |w: Where| match w {
        Where::Span(s) => {
            e.files.get(&s.file).is_some_and(|f| f == path)
                && start <= s.start as usize
                && (s.start as usize) < end
        }
        // `file:line:col`, a stated fact's place.
        Where::Place(place) => {
            let at = place.split(' ').next().unwrap_or("");
            let mut it = at.rsplitn(3, ':');
            let (Some(col), Some(line), Some(name)) = (it.next(), it.next(), it.next()) else {
                return false;
            };
            let (Ok(line), Ok(col)) = (line.parse::<usize>(), col.parse::<usize>()) else {
                return false;
            };
            let byte: usize = text
                .split_inclusive('\n')
                .take(line.saturating_sub(1))
                .map(str::len)
                .sum::<usize>()
                + col.saturating_sub(1);
            path.ends_with(name) && start <= byte && byte < end
        }
        _ => false,
    };
    let mut out: Vec<Address> = Vec::new();
    for a in e.res.facts.iter().filter(|a| a.pred == "want") {
        let s = |i: usize| match a.args.get(i) {
            Some(dform_core::ast::Term::Val(dform_core::value::Value::Str(x))) => Some(x.clone()),
            _ => None,
        };
        let (Some(typ), Some(name)) = (s(0), s(1)) else {
            continue;
        };
        let Some(id) = c.fact_id(&dform_core::engine::circuit_fact(a)) else {
            continue;
        };
        if analysis::written(e, id).is_some_and(here) {
            let addr = Address { typ, name };
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

/// A deformation as the hint says it.
fn said(p: &Planned) -> String {
    use dform_core::provider::ActionKind;
    use dform_core::report::{kind_name, marker_of};
    match (&p.kind, &p.on) {
        (ActionKind::Noop, _) => "undeformed".into(),
        (k, Some(on)) => format!(
            "{} pending on {}",
            marker_of(k),
            on.iter()
                .map(|n| dform_core::report::label(n))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        (k, None) => format!("{} {}", marker_of(k), kind_name(k)),
    }
}

/// The hint of the resource block `n`: each object's deformation, the
/// same ones counted (`2× + create`).
fn action(p: &Project, path: &Path, text: &str, n: &SyntaxNode, at: usize) -> Option<Hint> {
    let mut counted: Vec<(String, usize)> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for e in &p.evaluated {
        let Some(plan) = &e.plan else {
            continue;
        };
        for addr in addresses(e, path, text, n) {
            let s = plan
                .iter()
                .find(|x| x.addr == addr)
                .map_or_else(|| "undeformed".to_string(), said);
            lines.push(format!(
                "{}: {s} ({})",
                dform_core::report::address(&addr),
                e.deployment
            ));
            match counted.iter_mut().find(|(x, _)| *x == s) {
                Some((_, k)) => *k += 1,
                None => counted.push((s, 1)),
            }
        }
    }
    if counted.is_empty() {
        return None;
    }
    let label = counted
        .iter()
        .map(|(s, k)| match k {
            1 => s.clone(),
            k => format!("{k}× {s}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(Hint {
        at,
        label,
        tooltip: lines.join("\n"),
    })
}
