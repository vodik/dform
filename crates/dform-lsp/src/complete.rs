//! Schema completion: a resource block's attribute paths (with their type,
//! flags and refinements) and an enum attribute's values, from the
//! provider's schema facts; resource types after `resource`; a module
//! instance's inputs and, after `module.instance.`, its outputs; grant
//! patterns in `contributes`, and in a policy the paths its grants allow.
//! A type's or path's documentation is its `type_doc`. Elsewhere a word
//! completes to the builtins and keywords it starts (`engine::REFERENCE`).

use crate::nav;
use dform_core::ast::{Atom, Term};
use dform_core::engine::{self, RefKind};
use dform_core::schema::Schema;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use dform_core::value::Value;
use lsp_types::{CompletionItem, CompletionItemKind, Documentation};
use std::collections::BTreeMap;

/// One type's attribute as the schema states it.
struct Attr {
    ty: String,
    flags: Vec<String>,
    refinements: Vec<String>,
}

fn str_arg(a: &Atom, i: usize) -> Option<String> {
    match a.args.get(i)? {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Per type, per path, what the schema facts say.
fn attrs(schema: &Schema) -> BTreeMap<String, BTreeMap<String, Attr>> {
    let mut out: BTreeMap<String, BTreeMap<String, Attr>> = BTreeMap::new();
    for f in &schema.facts {
        if f.pred != "type_attr" {
            continue;
        }
        let (Some(t), Some(p), Some(ty)) = (str_arg(f, 0), str_arg(f, 1), str_arg(f, 2)) else {
            continue;
        };
        let flags = match f.args.get(3) {
            Some(Term::Val(Value::List(xs))) => xs
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            Some(Term::List(xs)) => xs
                .iter()
                .filter_map(|x| match x {
                    Term::Val(Value::Str(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        out.entry(t).or_default().insert(
            p,
            Attr {
                ty,
                flags,
                refinements: Vec::new(),
            },
        );
    }
    for f in &schema.facts {
        if f.pred != "type_refine" {
            continue;
        }
        let (Some(t), Some(p), Some(c)) = (str_arg(f, 0), str_arg(f, 1), str_arg(f, 2)) else {
            continue;
        };
        // A path the schema refines is one a program may write, declared
        // or not (the fake schema declares only what it must).
        out.entry(t)
            .or_default()
            .entry(p)
            .or_insert_with(|| Attr {
                ty: String::new(),
                flags: Vec::new(),
                refinements: Vec::new(),
            })
            .refinements
            .push(c);
    }
    out
}

/// The quoted strings of `enum(["a", "b"])` or `enum("a", "b")`.
fn enum_values(text: &str) -> Vec<String> {
    let Some(rest) = text.trim().strip_prefix("enum(") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            let v: String = chars.by_ref().take_while(|c| *c != '"').collect();
            out.push(v);
        }
    }
    out
}

fn item(
    label: String,
    kind: CompletionItemKind,
    detail: String,
    doc: Option<String>,
) -> CompletionItem {
    CompletionItem {
        label,
        kind: Some(kind),
        detail: (!detail.is_empty()).then_some(detail),
        documentation: doc.map(Documentation::String),
        ..Default::default()
    }
}

/// The candidates at byte `at` of the file whose tree is `root` (and
/// text `text`). `modules` finds a module's tree by name in the project.
pub fn complete(
    root: &SyntaxNode,
    text: &str,
    at: usize,
    schema: &Schema,
    modules: &dyn Fn(&str) -> Option<nav::Interface>,
) -> Vec<CompletionItem> {
    let line = &text[text[..at].rfind('\n').map_or(0, |i| i + 1)..at];
    let word: String = line
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let before_word = line[..line.len() - word.len()].trim_end();
    let attrs = attrs(schema);
    let docs = schema.docs();
    let doc_of = |t: &str, p: &str| docs.get(&(t, p)).map(|d| d.to_string());

    // `contributes T.path`: every type's paths, and `_.path`.
    if before_word.ends_with("contributes") {
        let mut out = Vec::new();
        let mut any: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (t, ps) in &attrs {
            out.push(item(
                t.clone(),
                CompletionItemKind::CLASS,
                "every path of the type".into(),
                doc_of(t, ""),
            ));
            for (p, a) in ps {
                out.push(item(
                    format!("{t}.{p}"),
                    CompletionItemKind::FIELD,
                    a.ty.clone(),
                    doc_of(t, p),
                ));
                any.entry(p.clone()).or_default().push(t.clone());
            }
        }
        for (p, ts) in any {
            out.push(item(
                format!("t.{p}"),
                CompletionItemKind::FIELD,
                format!("every type's .{p}"),
                Some(format!("types with .{p}: {}", ts.join(", "))),
            ));
        }
        return out;
    }
    // `resource TYPE`: the schema's types.
    if before_word.ends_with("resource") {
        return attrs
            .keys()
            .map(|t| {
                item(
                    t.clone(),
                    CompletionItemKind::CLASS,
                    "resource type".into(),
                    doc_of(t, ""),
                )
            })
            .collect();
    }
    // `module.instance.`: the module's outputs.
    let segs: Vec<&str> = word.split('.').collect();
    if segs.len() == 3
        && let Some(m) = modules(segs[0])
    {
        return m
            .outputs
            .into_iter()
            .map(|(n, ty)| {
                item(
                    n,
                    CompletionItemKind::PROPERTY,
                    ty,
                    Some(format!("output of module {}", segs[0])),
                )
            })
            .collect();
    }

    let Some(tok) = nav::token_before(root, at) else {
        return Vec::new();
    };
    let Some(parent) = tok.parent() else {
        return Vec::new();
    };
    // A value after `path =` in a resource block: the path's enum values.
    let assign = parent.ancestors().find(|n| n.kind() == SyntaxKind::ASSIGN);
    let resource = parent
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::RESOURCE);
    if let (Some(assign), Some(resource)) = (&assign, &resource) {
        let after_eq = assign
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == SyntaxKind::EQ && usize::from(t.text_range().end()) <= at);
        if after_eq {
            let path = assign
                .children()
                .find(|c| c.kind() == SyntaxKind::BLOCK_PATH)
                .map(|c| c.text().to_string().replace(' ', ""));
            let typ = nav::name_after_keyword(resource);
            let Some(a) = typ
                .zip(path)
                .and_then(|(t, p)| attrs.get(&t).and_then(|m| m.get(&p)))
            else {
                return Vec::new();
            };
            let mut values = enum_values(&a.ty);
            for r in &a.refinements {
                values.extend(enum_values(r));
            }
            return values
                .into_iter()
                .map(|v| {
                    item(
                        format!("\"{v}\""),
                        CompletionItemKind::ENUM_MEMBER,
                        a.ty.clone(),
                        None,
                    )
                })
                .collect();
        }
    }
    let in_block = |kind: SyntaxKind| {
        parent.ancestors().find(|n| n.kind() == kind).filter(|n| {
            n.children()
                .find(|c| c.kind() == SyntaxKind::BLOCK)
                .is_some_and(|b| b.text_range().contains_inclusive((at as u32).into()))
        })
    };
    // A resource block's attribute paths.
    if let Some(r) = in_block(SyntaxKind::RESOURCE) {
        let Some((t, ps)) = nav::name_after_keyword(&r).and_then(|t| attrs.get_key_value(&t))
        else {
            return Vec::new();
        };
        return ps
            .iter()
            .filter(|(_, a)| !a.flags.iter().any(|f| f == "computed"))
            .map(|(p, a)| {
                let mut detail = a.ty.clone();
                if !a.flags.is_empty() {
                    detail.push_str(&format!(" [{}]", a.flags.join(", ")));
                }
                let refined = (!a.refinements.is_empty())
                    .then(|| format!("refined: {}", a.refinements.join(", ")));
                let doc = match (doc_of(t, p), refined) {
                    (Some(d), Some(r)) => Some(format!("{d}\n\n{r}")),
                    (d, r) => d.or(r),
                };
                item(p.clone(), CompletionItemKind::FIELD, detail, doc)
            })
            .collect();
    }
    // An instance block: its module's inputs.
    if let Some(i) = in_block(SyntaxKind::INSTANCE) {
        let Some(m) = nav::declared_name(&i) else {
            return Vec::new();
        };
        let Some(interface) = modules(m.text()) else {
            return Vec::new();
        };
        return interface
            .inputs
            .into_iter()
            .map(|(n, ty)| {
                item(
                    n,
                    CompletionItemKind::FIELD,
                    ty,
                    Some(format!("input of module {}", m.text())),
                )
            })
            .collect();
    }
    // `r.` in a policy: the paths its grants allow.
    if segs.len() == 2 {
        let grants = nav::grants(&parent);
        return grants
            .iter()
            .filter_map(|g| {
                // `t.tags`: a name no type starts with is every type.
                let (h, rest) = g.split_once('.')?;
                let typed = h == "settings"
                    || attrs
                        .keys()
                        .any(|t| t == h || t.starts_with(&format!("{h}.")));
                let (t, p) = if !typed {
                    ("_", rest)
                } else {
                    // `net.vpc.cidr` by the schema's types; `settings.x`,
                    // a type no schema declares, by its first name.
                    match attrs.keys().find(|t| g.starts_with(&format!("{t}."))) {
                        Some(t) => (t.as_str(), &g[t.len() + 1..]),
                        None => (h, rest),
                    }
                };
                Some(item(
                    p.to_string(),
                    CompletionItemKind::FIELD,
                    format!("granted: contributes {g}"),
                    (t != "_").then(|| format!("of {t}")),
                ))
            })
            .collect();
    }
    // A plain word: the builtins and keywords it starts.
    if word.is_empty() || word.contains('.') {
        return Vec::new();
    }
    engine::REFERENCE
        .iter()
        .filter(|r| r.name.starts_with(word.as_str()))
        .map(|r| {
            let kind = match r.kind {
                RefKind::Keyword => CompletionItemKind::KEYWORD,
                _ => CompletionItemKind::FUNCTION,
            };
            let doc = format!("{}\n\n{}", r.summary, r.example);
            item(r.name.to_string(), kind, r.signature.to_string(), Some(doc))
        })
        .collect()
}
