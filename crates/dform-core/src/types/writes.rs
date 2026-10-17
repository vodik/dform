//! A write of an attribute its type's provider does not declare: a
//! block entry (`resource T n { p = v }`) or a `set` (`set x.p = v`,
//! `set c.p = v where c in R.l`) at a path the schema has no attribute
//! of, as deep as the schema declares it: an object whose fields the
//! schema lists is checked to the field named, a `map` or an untyped
//! `object` not beyond itself (the schema says no more below it). Such
//! a write would reach the provider with a field it has no place for,
//! so it is an error at the write, in [`super::reads`]'s words and
//! help. Not checked: a type that takes attributes its schema does not
//! list ([`crate::schema::TYPE_OPEN`]), a write through a variable
//! whose cell's type is not a literal (its type not known here), and a
//! type its schema declares no attribute of (a component, a kind a
//! cluster defines).

use crate::address;
use crate::ast::{Span, Term};
use crate::diag::Diagnostic;
use crate::program::node::{ItemKind, Target};
use crate::program::{self as prog};
use crate::schema::Schema;
use crate::value::Value;
use std::collections::BTreeSet;

/// Each write of an attribute its type has none of, an error at its
/// site naming the attribute and the type.
pub(super) fn undeclared(p: &prog::Program, schema: &Schema) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for item in p.items.values() {
        match &item.kind {
            ItemKind::Resource { typ, body, .. } => {
                for (path, _, span) in super::entries(p, body) {
                    if !path.is_empty() {
                        check(schema, &typ.name, &path, span, &mut out);
                    }
                }
            }
            ItemKind::Set { writes, .. } => {
                for w in writes {
                    if let Some((typ, path)) = write_of(p, &w.target) {
                        check(schema, &typ, &path, w.span, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The type and the stored path a `set`'s target writes: `None` for an
/// input (not a resource attribute) or a cell whose type is a variable
/// (not known here).
fn write_of(p: &prog::Program, target: &Target) -> Option<(String, String)> {
    let (typ, path) = match target {
        Target::Attr { typ, path, .. } => (*typ, path.clone()),
        Target::Element { typ, list, rest, .. } => {
            let path = match rest.is_empty() {
                true => list.clone(),
                false => format!("{list}.{}", rest.join(".")),
            };
            (*typ, path)
        }
        Target::Input { .. } => return None,
    };
    match prog::lower_expr(p, typ).0 {
        Term::Val(Value::Str(t)) => Some((t, path)),
        _ => None,
    }
}

/// `path`'s keys, unquoted and without a selector's index or key
/// (`containers[name=api].image` is `containers`, `image`).
fn plain_segments(path: &str) -> Vec<String> {
    address::path_segments(path)
        .into_iter()
        .map(|seg| address::segment_parts(seg).0.into_owned())
        .collect()
}

/// `typ`'s declared names directly under `prefix` (its own, top-level
/// ones when empty): a declared path's next segment past it, every
/// path sharing one collapsed to it.
fn children(schema: &Schema, typ: &str, prefix: &str) -> BTreeSet<String> {
    let lo = match prefix.is_empty() {
        true => String::new(),
        false => format!("{prefix}."),
    };
    schema
        .attrs
        .range((typ.to_string(), lo.clone())..)
        .take_while(|((t, p), _)| t == typ && p.starts_with(&lo))
        .filter_map(|((_, p), _)| p[lo.len()..].split('.').next().map(str::to_string))
        .collect()
}

/// Whether `typ`'s attribute declared exactly at `prefix` takes any
/// further attribute (a `map`, an untyped `object` or `any`): a write
/// below it is never checked, whatever else the schema happens to
/// declare under it.
fn is_open(schema: &Schema, typ: &str, prefix: &str) -> bool {
    !prefix.is_empty()
        && schema
            .attr(typ, prefix)
            .is_some_and(|spec| matches!(spec.kind(), "map" | "object" | "any"))
}

/// A write of `path` on `typ`, checked a segment at a time against what
/// the schema declares there: an error at the first one it does not
/// (naming the nearest sibling, else the command that prints `typ`'s
/// attributes), unless `typ` is open or its schema declares nothing of
/// it (not known here).
fn check(schema: &Schema, typ: &str, path: &str, span: Span, out: &mut Vec<Diagnostic>) {
    if schema.is_open(typ) {
        return;
    }
    let top = children(schema, typ, "");
    if top.is_empty() {
        return;
    }
    let segs = plain_segments(path);
    let mut prefix = String::new();
    for seg in &segs {
        if is_open(schema, typ, &prefix) {
            return;
        }
        let kids = children(schema, typ, &prefix);
        if !kids.contains(seg) {
            out.push(no_such_attribute(typ, &segs.join("."), seg, &kids, span));
            return;
        }
        prefix = match prefix.is_empty() {
            true => seg.clone(),
            false => format!("{prefix}.{seg}"),
        };
    }
}

/// `{typ} has no attribute `{attr}``, its help the sibling attribute it
/// is nearest to (a slip of the pen), else the command that prints the
/// type's attributes: its provider's schema, by the provider's name in
/// the type's.
fn no_such_attribute(
    typ: &str,
    attr: &str,
    seg: &str,
    siblings: &BTreeSet<String>,
    span: Span,
) -> Diagnostic {
    let help = match crate::diag::nearest(seg, siblings.iter().map(String::as_str)) {
        Some(n) => format!("`{n}` is an attribute of {typ}"),
        None => {
            let provider = typ.split_once('.').map_or(typ, |(p, _)| p);
            format!("`dform provider schema {provider}` prints {typ}'s attributes")
        }
    };
    Diagnostic::error(
        span,
        format!("{typ} has no attribute `{attr}`: its provider does not declare it"),
    )
    .with_help(help)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential whose provider declares `access` and `secret`, and
    /// nested `config.region` and `config.tags` (a `map`); a keyed list
    /// `items`, each with `name` and `config.region`; a key, an open
    /// type, and a type the schema declares nothing of.
    fn schema() -> Schema {
        Schema::parse(
            "type_attr(\"s.cred\", \"id\", \"string\", [\"computed\", \"id\"])\n\
             type_attr(\"s.cred\", \"access\", \"string\", [\"computed\"])\n\
             type_attr(\"s.cred\", \"secret\", \"string\", [\"computed\"])\n\
             type_attr(\"s.cred\", \"config.region\", \"string\", [])\n\
             type_attr(\"s.cred\", \"config.tags\", \"map\", [])\n\
             type_attr(\"s.cred\", \"items\", \"list\", [])\n\
             type_list_key(\"s.cred\", \"items\", [\"name\"])\n\
             type_attr(\"s.cred\", \"items.name\", \"string\", [])\n\
             type_attr(\"s.cred\", \"items.config.region\", \"string\", [])\n\
             type_attr(\"s.key\", \"id\", \"string\", [\"computed\", \"id\"])\n\
             type_attr(\"s.free\", \"id\", \"string\", [\"computed\", \"id\"])\n\
             type_open(\"s.free\")\n",
            "test",
        )
        .and_then(Schema::imported)
        .unwrap()
    }

    /// Each error's message, help and the text at its site.
    fn errors(src: &str) -> Vec<(String, String, String)> {
        let nodes = super::super::tests::nodes(src);
        undeclared(&nodes, &schema())
            .into_iter()
            .map(|d| {
                let at = &src[d.span.start as usize..d.span.end as usize];
                (d.message, d.help.unwrap_or_default(), at.to_string())
            })
            .collect()
    }

    /// A block entry of an attribute the provider does not declare: an
    /// error at the entry, naming the type and the attribute; the help
    /// is the attribute it is nearest, else the command that prints the
    /// type's attributes.
    #[test]
    fn a_block_entry_of_an_attribute_the_type_does_not_have_is_an_error_at_the_entry() {
        let errors = errors("resource s.cred k { secrt = \"x\" }\n");
        assert_eq!(
            errors,
            [(
                "s.cred has no attribute `secrt`: its provider does not declare it".into(),
                "`secret` is an attribute of s.cred".into(),
                "secrt = \"x\"".into(),
            )]
        );
    }

    /// A nested write below a declared object attribute an unknown
    /// field of it: an error naming the full path, the help the
    /// command that prints the type's attributes when no sibling is
    /// near.
    #[test]
    fn a_nested_write_of_a_field_its_object_attribute_does_not_have_is_an_error() {
        let errors = errors("resource s.cred k { config.nosuch = \"x\" }\n");
        assert_eq!(
            errors,
            [(
                "s.cred has no attribute `config.nosuch`: its provider does not declare it"
                    .into(),
                "`dform provider schema s` prints s.cred's attributes".into(),
                "config.nosuch = \"x\"".into(),
            )]
        );
    }

    /// What a write may reach though it is not checked: a `set` of a
    /// declared attribute or of a path below a `map` (any key), a type
    /// marked open, one the schema declares nothing of, and a `set`
    /// through a variable whose type is not known.
    #[test]
    fn a_write_the_check_does_not_reach_is_not_an_error() {
        let errors = errors(
            "input on: bool = true\n\
             resource s.cred k { access = \"a\", config.tags.team = \"x\" }\n\
             set k.secret = \"s\" where on\n\
             set k.config.region = \"eu\" where on\n\
             resource s.key w { }\n\
             resource s.free fr { nosuch = \"y\" }\n",
        );
        assert_eq!(errors, []);
    }

    /// A `set` through a keyed list's element writing a field the
    /// schema declares under it: no error.
    #[test]
    fn a_set_through_an_element_of_a_declared_field_is_not_an_error() {
        let errors = errors(
            "resource s.cred k { items = [{ name = \"a\", config = { region = \"eu\" } }] }\n\
             set x.config.region = \"us\" where x in k.items\n",
        );
        assert_eq!(errors, []);
    }

    /// A `set` through a keyed list's element writing a field its
    /// schema does not declare under it: an error naming the element's
    /// full path.
    #[test]
    fn a_set_through_an_element_of_an_undeclared_field_is_an_error() {
        let errors = errors(
            "resource s.cred k { items = [{ name = \"a\" }] }\n\
             set x.config.nosuch = \"y\" where x in k.items\n",
        );
        assert_eq!(
            errors,
            [(
                "s.cred has no attribute `items.config.nosuch`: its provider does not declare \
                 it"
                    .into(),
                "`dform provider schema s` prints s.cred's attributes".into(),
                "set x.config.nosuch = \"y\" where x in k.items".into(),
            )]
        );
    }
}
