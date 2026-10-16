//! A read of an attribute its type does not have: `r.p`, `T[e].p`, `x.p`
//! of a variable `x in T`, `world.T[a].p`, where `T`'s provider declares
//! no attribute `p` and no block of the program writes one (a provider
//! that keeps what it is given, the mock, holds what the program writes).
//! Such a read never has a value, so it is an error at its site in every
//! run: plan, apply and `dform test` all read the program through
//! [`super::read`]. Not checked: a type that takes attributes its schema
//! does not list ([`crate::schema::TYPE_OPEN`], the fake's), and one its
//! schema declares no attribute of, whose attributes are not known here
//! (a component, a kind a cluster defines).

use super::entries;
use crate::ast::Term;
use crate::diag::Diagnostic;
use crate::program::node::{Expr, ExprKind, ItemKind, ResourceBody, Step, Target};
use crate::program::{self as prog};
use crate::schema::Schema;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Each read of an attribute its type does not have, an error at its
/// site naming the attribute and the type.
pub(super) fn undeclared(p: &prog::Program, schema: &Schema) -> Vec<Diagnostic> {
    let written = Written::of(p);
    p.exprs
        .values()
        .filter_map(|e| {
            let (typ, attr) = read_of(p, e)?;
            if written.has(typ, attr) || schema.is_open(typ) {
                return None;
            }
            let declared = declared(schema, typ);
            (!declared.is_empty() && !declared.contains(attr))
                .then(|| no_such_attribute(e, typ, attr, &declared))
        })
        .collect()
}

/// The type and the attribute a read of a resource's attribute reads:
/// the first field of its path.
fn read_of<'p>(p: &'p prog::Program, e: &'p Expr) -> Option<(&'p str, &'p str)> {
    let (typ, path) = match &e.kind {
        ExprKind::Field { base, path } => match &p.exprs[*base].kind {
            ExprKind::Resource { typ, .. } => (typ, path),
            _ => return None,
        },
        ExprKind::World { typ, path, .. } => (typ, path),
        _ => return None,
    };
    match path.first()? {
        Step::Field(attr, _) => Some((typ.name.as_str(), attr.as_str())),
        _ => None,
    }
}

/// The top-level attributes `typ`'s provider declares.
fn declared(schema: &Schema, typ: &str) -> BTreeSet<String> {
    schema
        .attrs
        .range((typ.to_string(), String::new())..)
        .take_while(|((t, _), _)| t == typ)
        .map(|((_, path), _)| head(path))
        .collect()
}

/// `{typ} has no attribute `{attr}``, its help the declared attribute it
/// is nearest (a slip of the pen), else the command that prints the
/// type's attributes: its provider's schema, by the provider's name in
/// the type's (`ovh.s3_credential` is `ovh`'s).
fn no_such_attribute(e: &Expr, typ: &str, attr: &str, declared: &BTreeSet<String>) -> Diagnostic {
    let help = match crate::diag::nearest(attr, declared.iter().map(String::as_str)) {
        Some(n) => format!("`{n}` is an attribute of {typ}"),
        None => {
            let provider = typ.split_once('.').map_or(typ, |(p, _)| p);
            format!("`dform provider schema {provider}` prints {typ}'s attributes")
        }
    };
    Diagnostic::error(
        e.span,
        format!(
            "{typ} has no attribute `{attr}`: its provider does not declare it and the program \
             writes none"
        ),
    )
    .with_help(help)
}

/// A stored path's first key, unquoted, without its index
/// (`containers[name=api].image` is `containers`).
fn head(path: &str) -> String {
    let first = crate::address::path_keys(path)
        .into_iter()
        .next()
        .unwrap_or_default();
    match first.split_once('[') {
        Some((key, _)) if !path.starts_with('"') => key.to_string(),
        _ => first,
    }
}

/// The attributes the program writes, by type: what a read may find
/// though the type's provider does not declare it.
#[derive(Default)]
struct Written {
    by_type: BTreeMap<String, BTreeSet<String>>,
    /// The types a block writes whole (`resource T n = f(x)`, a spread):
    /// any attribute.
    whole: BTreeSet<String>,
    /// What a `set` through a variable writes, on any type.
    any: BTreeSet<String>,
}

impl Written {
    /// Every block's and every `set`'s, a component's included.
    fn of(p: &prog::Program) -> Written {
        let mut w = Written::default();
        for item in p.items.values() {
            match &item.kind {
                ItemKind::Resource { typ, body, .. } => w.block(p, &typ.name, body),
                ItemKind::Set { writes, .. } => {
                    for x in writes {
                        w.set(p, &x.target);
                    }
                }
                _ => {}
            }
        }
        w
    }

    /// A resource block's entries; its keys, of a constant object.
    fn block(&mut self, p: &prog::Program, typ: &str, body: &ResourceBody) {
        for (path, value, _) in entries(p, body) {
            if !path.is_empty() {
                self.add(Some(typ), &path);
                continue;
            }
            match &p.exprs[value].kind {
                ExprKind::Lit(Value::Obj(m)) => {
                    for k in m.keys() {
                        self.add(Some(typ), &crate::address::path_join("", k));
                    }
                }
                _ => {
                    self.whole.insert(typ.to_string());
                }
            }
        }
    }

    /// A `set`'s cell, of its type when the type is a name.
    fn set(&mut self, p: &prog::Program, target: &Target) {
        let (typ, path) = match target {
            Target::Attr { typ, path, .. } => (typ, path),
            Target::Element { typ, list, .. } => (typ, list),
            Target::Input { .. } => return,
        };
        match prog::lower_expr(p, *typ).0 {
            Term::Val(Value::Str(t)) => self.add(Some(&t), path),
            _ => self.add(None, path),
        }
    }

    fn add(&mut self, typ: Option<&str>, path: &str) {
        let attr = head(path);
        match typ {
            Some(t) => self.by_type.entry(t.to_string()).or_default().insert(attr),
            None => self.any.insert(attr),
        };
    }

    fn has(&self, typ: &str, attr: &str) -> bool {
        self.whole.contains(typ)
            || self.any.contains(attr)
            || self.by_type.get(typ).is_some_and(|a| a.contains(attr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential whose provider declares `access`, `secret` and a
    /// nested `status.ready`; a key and an open type that declare their
    /// ids.
    fn schema() -> Schema {
        Schema::parse(
            "type_attr(\"s.cred\", \"id\", \"string\", [\"computed\", \"id\"])\n\
             type_attr(\"s.cred\", \"access\", \"string\", [\"computed\"])\n\
             type_attr(\"s.cred\", \"secret\", \"string\", [\"computed\"])\n\
             type_attr(\"s.cred\", \"status.ready\", \"bool\", [\"computed\"])\n\
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

    /// The renamed attribute (the user's project, edge 6): a read of what
    /// the provider dropped is an error at the read, of a resource or of
    /// a live object, naming the type and the attribute; the help is the
    /// attribute it is nearest, else the command that prints the type's
    /// attributes.
    #[test]
    fn a_read_of_an_attribute_the_type_does_not_have_is_an_error_at_its_site() {
        let errors = errors(
            "resource s.cred k { user = \"u\" }\n\
             let a = k.s3_access_key\n\
             let b = k.secrt\n\
             let c = world.s.cred[\"k\"].acess\n",
        );
        let no = |a: &str| {
            format!(
                "s.cred has no attribute `{a}`: its provider does not declare it and the \
                 program writes none"
            )
        };
        assert_eq!(
            errors,
            [
                (
                    no("s3_access_key"),
                    "`dform provider schema s` prints s.cred's attributes".into(),
                    "k.s3_access_key".into()
                ),
                (
                    no("secrt"),
                    "`secret` is an attribute of s.cred".into(),
                    "k.secrt".into()
                ),
                (
                    no("acess"),
                    "`access` is an attribute of s.cred".into(),
                    "world.s.cred[\"k\"].acess".into()
                ),
            ]
        );
    }

    /// What a read may find though the provider does not declare it: what
    /// a block or a `set` writes (a provider that takes any attribute,
    /// the mock's, holds it), on the type or through a variable on any;
    /// a field under a declared path; any attribute of a type written
    /// whole, of an open one (the fake's) or of one its schema declares
    /// nothing of.
    #[test]
    fn a_read_of_what_the_program_writes_or_the_schema_does_not_know_is_not() {
        let errors = errors(
            "input on: bool = true\n\
             resource s.cred k { user = \"u\" }\n\
             resource s.key w = f(\"w\")\n\
             let f(u) = { user = u }\n\
             set k.label = \"l\" where on\n\
             set c.tag = \"t\" where c in resource\n\
             resource other.thing o { }\n\
             resource s.free fr { }\n\
             let reads = [k.user, k.label, k.tag, k.status.ready, w.anything, o.anything, \
             fr.anything]\n",
        );
        assert_eq!(errors, []);
    }
}
