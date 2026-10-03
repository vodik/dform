//! Schema completion: a resource block's attribute paths (with their type,
//! flags and refinements) and an enum attribute's values, from the
//! provider's schema facts; resource types after `resource`; a module
//! instance's inputs and, after `copy.`, its outputs.
//! A type's or path's documentation is its `type_doc`. Elsewhere a word
//! completes to the builtins and keywords it starts (`engine::references`),
//! a package's name and a dot (`inet.su`) to its functions.

use crate::nav;
use dform_core::ast::{Atom, Term};
use dform_core::engine::{self, RefKind};
use dform_core::schema::Schema;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use dform_core::value::Value;
use lsp_types::{CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, TextEdit};
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
    // `copy.`: the outputs of the copy's component (R-65), the copy an
    // `instance` of the file names, or of the module a `use` binds, its
    // relations among them (`m.p(`, R-55).
    let segs: Vec<&str> = word.split('.').collect();
    if let [copy, ""] = segs.as_slice()
        && let Some(path) = root
            .descendants()
            .filter_map(|n| match n.kind() {
                SyntaxKind::INSTANCE => Some(dform_core::syntax::resolve::instance_parts(&n)),
                SyntaxKind::USE => Some(dform_core::syntax::resolve::use_parts(&n)),
                _ => None,
            })
            .find(|(_, name)| name == copy)
            .map(|(path, _)| path)
        && let Some(m) = modules(&path)
    {
        return m
            .outputs
            .into_iter()
            .map(|(n, ty)| {
                item(
                    n,
                    CompletionItemKind::PROPERTY,
                    ty,
                    Some(format!("output of {copy}, a copy of {path}")),
                )
            })
            .collect();
    }

    // `k.` and `k.f.`, `k` an object input (R-54): its fields there, as
    // `--set k.f=v`, `set k.f = v` and a read address them.
    if let [input, fields @ .., _] = segs.as_slice()
        && let Some(found) = input_fields(root, input, fields)
    {
        return found
            .into_iter()
            .map(|(name, ty)| {
                item(
                    name,
                    CompletionItemKind::FIELD,
                    ty,
                    Some(format!("a field of input {input}")),
                )
            })
            .collect();
    }

    // `inet.su`: a function package's functions, wherever a call goes.
    if let [package, _] = segs.as_slice()
        && dform_core::functions::registry().is_package(package)
    {
        return builtins(&word, Some(crate::text::range(text, at - word.len(), at)));
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
    // An instance block: its component's inputs.
    if let Some(i) = in_block(SyntaxKind::INSTANCE) {
        let (m, _) = dform_core::syntax::resolve::instance_parts(&i);
        let Some(interface) = modules(&m) else {
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
                    Some(format!("input of component {m}")),
                )
            })
            .collect();
    }
    // A plain word: the builtins and keywords it starts.
    if word.is_empty() || word.contains('.') {
        return Vec::new();
    }
    builtins(&word, None)
}

/// The builtins and keywords `word` starts; with `replaced`, a dotted
/// word's range, each replaces the whole word (a client's word ends at
/// the dot).
fn builtins(word: &str, replaced: Option<lsp_types::Range>) -> Vec<CompletionItem> {
    engine::references()
        .iter()
        .filter(|r| r.name.starts_with(word))
        .map(|r| {
            let kind = match r.kind {
                RefKind::Keyword => CompletionItemKind::KEYWORD,
                _ => CompletionItemKind::FUNCTION,
            };
            let doc = format!("{}\n\n{}", r.summary, r.example);
            let mut i = item(r.name.to_string(), kind, r.signature.to_string(), Some(doc));
            if let Some(range) = replaced {
                i.filter_text = Some(r.name.to_string());
                i.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: r.name.to_string(),
                }));
            }
            i
        })
        .collect()
}

/// The fields of the object input `input` the tree declares in its block
/// form (`input k { f: T, g: { .. } }`), below `path`: each name with its
/// type, a nested object's as `{..}`. `None` when there is no such input
/// or path.
fn input_fields(root: &SyntaxNode, input: &str, path: &[&str]) -> Option<Vec<(String, String)>> {
    let name = |n: &SyntaxNode| -> Option<String> {
        n.children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
    };
    let mut at = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::INPUT && name(n).as_deref() == Some(input))?;
    let fields = |n: &SyntaxNode| -> Vec<SyntaxNode> {
        n.children()
            .filter(|c| c.kind() == SyntaxKind::ATTR_DECL)
            .collect()
    };
    let field_name = |f: &SyntaxNode| -> Option<String> {
        f.children()
            .find(|c| c.kind() == SyntaxKind::BLOCK_PATH)
            .map(|p| p.text().to_string())
    };
    for seg in path {
        at = fields(&at)
            .into_iter()
            .find(|f| field_name(f).as_deref() == Some(*seg))?;
    }
    let out: Vec<(String, String)> = fields(&at)
        .iter()
        .filter_map(|f| {
            let ty = match f.children().find(|c| c.kind() == SyntaxKind::TYPE_EXPR) {
                Some(t) => t.text().to_string(),
                None => "{..}".to_string(),
            };
            Some((field_name(f)?, ty))
        })
        .collect();
    (!out.is_empty()).then_some(out)
}
