//! A value in the formatter's layout (R-124): what the plan, `why`,
//! `query` and the editor's hover print for a value with structure. An
//! object or a list on one line when it fits, else one field or element
//! per line with a trailing comma, from the outside in; a list whose only
//! element is an object hugs it, `[{` .. `}]`, and an object whose only
//! field is a list hugs that, `{ k: [` .. `] }`, as [`super::layout`]
//! lays out the source. A leaf is text the caller spelled (a string
//! quoted whole, a reference by its address, a secret redacted).

use super::doc::{Doc, concat, group, if_break, indent, line, nil, print, propagate, text};
use crate::value::Value;

/// A value to lay out: its leaves already spelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tree {
    Leaf(String),
    /// Fields by their key as printed (quoted when it is not a name).
    Obj(Vec<(String, Tree)>),
    List(Vec<Tree>),
}

impl Tree {
    /// `v` as a tree: `leaf` spells a value that is one (a scalar, a
    /// secret, a null), `None` for an object or a list to open.
    pub fn of(v: &Value, leaf: &dyn Fn(&Value) -> Option<String>) -> Tree {
        if let Some(t) = leaf(v) {
            return Tree::Leaf(t);
        }
        match v {
            Value::Obj(m) => Tree::Obj(
                m.iter()
                    .map(|(k, x)| (key_text(k), Tree::of(x, leaf)))
                    .collect(),
            ),
            Value::List(xs) => Tree::List(xs.iter().map(|x| Tree::of(x, leaf)).collect()),
            v => Tree::Leaf(crate::partition::fmt_value(v)),
        }
    }

    /// Its leaves.
    pub fn leaves(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::Obj(fs) => fs.iter().map(|(_, t)| t.leaves()).sum(),
            Tree::List(xs) => xs.iter().map(Tree::leaves).sum(),
        }
    }

    fn doc(&self) -> Doc {
        match self {
            Tree::Leaf(s) => text(s.clone()),
            Tree::Obj(fs) if fs.is_empty() => text("{}"),
            Tree::List(xs) if xs.is_empty() => text("[]"),
            // `{ k: [` .. `] }`
            Tree::Obj(fs) if matches!(fs.as_slice(), [(_, Tree::List(xs))] if !xs.is_empty()) => {
                let (k, l) = &fs[0];
                concat(vec![text(format!("{{ {k}: ")), l.doc(), text(" }")])
            }
            Tree::Obj(fs) => bracketed(
                "{",
                "}",
                " ",
                fs.iter()
                    .map(|(k, t)| concat(vec![text(format!("{k}: ")), t.doc()]))
                    .collect(),
            ),
            // `[{` .. `}]`
            Tree::List(xs) if matches!(xs.as_slice(), [Tree::Obj(fs)] if !fs.is_empty()) => {
                concat(vec![text("["), xs[0].doc(), text("]")])
            }
            Tree::List(xs) => bracketed("[", "]", "", xs.iter().map(Tree::doc).collect()),
        }
    }
}

/// `open items close`: on one line `{ a, b }` (`pad` inside the
/// brackets), broken one item per line, each with a comma.
fn bracketed(open: &str, close: &str, pad: &'static str, items: Vec<Doc>) -> Doc {
    let n = items.len();
    let mut inner = Vec::new();
    for (i, it) in items.into_iter().enumerate() {
        inner.push(line(if i == 0 { pad } else { " " }));
        inner.push(it);
        inner.push(match i + 1 < n {
            true => text(","),
            false => if_break(text(","), nil()),
        });
    }
    group(
        concat(vec![
            text(open),
            indent(concat(inner)),
            line(pad),
            text(close),
        ]),
        false,
    )
}

/// `head` followed by `t`, in `width` columns: the lines, the first
/// starting with `head`, the others indented from column 0 (the caller
/// indents them as it indents the first).
pub fn layout(head: &str, t: &Tree, width: usize) -> Vec<String> {
    let mut d = concat(vec![text(head.to_string()), t.doc()]);
    propagate(&mut d);
    print(&d, width).lines().map(str::to_string).collect()
}

/// An object key as the source writes it: a name bare, anything else
/// quoted.
pub fn key_text(k: &str) -> String {
    let mut cs = k.chars();
    let name = cs
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    match name {
        true => k.to_string(),
        false => crate::partition::quote(k),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(s: &str) -> Tree {
        Tree::Leaf(s.into())
    }

    #[test]
    fn a_value_that_fits_is_one_line() {
        let t = Tree::Obj(vec![
            ("name".into(), leaf("\"web\"")),
            (
                "args".into(),
                Tree::List(vec![leaf("\"a\""), leaf("\"b\"")]),
            ),
        ]);
        assert_eq!(
            layout("c = ", &t, 100),
            ["c = { name: \"web\", args: [\"a\", \"b\"] }"]
        );
    }

    #[test]
    fn a_value_too_wide_breaks_from_the_outside_in() {
        let long = "\"--certificatesresolvers.letsencrypt.acme.email=admin@example.com\"";
        let t = Tree::Obj(vec![
            ("name".into(), leaf("\"traefik\"")),
            (
                "args".into(),
                Tree::List(vec![leaf(long), leaf("\"--api\"")]),
            ),
            (
                "ports".into(),
                Tree::List(vec![Tree::Obj(vec![("name".into(), leaf("\"web\""))])]),
            ),
        ]);
        assert_eq!(
            layout("c = ", &t, 60),
            [
                "c = {",
                "  name: \"traefik\",",
                "  args: [",
                format!("    {long},").as_str(),
                "    \"--api\",",
                "  ],",
                "  ports: [{ name: \"web\" }],",
                "}",
            ]
        );
    }

    #[test]
    fn a_list_of_one_object_hugs_it() {
        let t = Tree::List(vec![Tree::Obj(vec![
            ("name".into(), leaf("\"migrate\"")),
            ("image".into(), leaf("\"gcr.io/shop/crud-api\"")),
        ])]);
        assert_eq!(
            layout("containers = ", &t, 30),
            [
                "containers = [{",
                "  name: \"migrate\",",
                "  image: \"gcr.io/shop/crud-api\",",
                "}]",
            ]
        );
    }

    #[test]
    fn an_object_of_one_list_hugs_it() {
        let t = Tree::Obj(vec![(
            "Statement".into(),
            Tree::List(vec![Tree::Obj(vec![
                ("Action".into(), leaf("\"rds-db:connect\"")),
                (
                    "Resource".into(),
                    leaf("\"orders.cx3k.us-east-1.rds.amazonaws.com\""),
                ),
            ])]),
        )]);
        assert_eq!(
            layout("policy = ", &t, 94),
            [
                "policy = { Statement: [{",
                "  Action: \"rds-db:connect\",",
                "  Resource: \"orders.cx3k.us-east-1.rds.amazonaws.com\",",
                "}] }",
            ]
        );
    }

    #[test]
    fn a_key_that_is_no_name_is_quoted() {
        assert_eq!(key_text("app"), "app");
        assert_eq!(
            key_text("pod-security.kubernetes.io/enforce"),
            "\"pod-security.kubernetes.io/enforce\""
        );
    }
}
