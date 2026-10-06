//! `dform test` (R-32): the program's denies are its tests. The runner
//! evaluates the program once per combination of its inputs against an
//! empty world, and every deny must hold in each. The input space: an
//! input the target pins (a key's `k=v`) or `--set` pins is that value; an
//! `enum` input takes each of its values, a `bool` both; a key whose type
//! is not an enum takes each value a deployment of the stack was applied
//! with; any other input takes its default, and one with none is an error
//! naming it. An input a `set .. where` (or `set from`) gives is the
//! program's to decide, in the deployments its clause holds in, and no
//! axis (R-38): the space is what the outside may give. A failure is
//! printed as the command that reproduces it.

use crate::ast::TypeExpr;
use crate::inputs::Declared;
use crate::value::Value;
use anyhow::{Result, bail};

/// The most combinations a run enumerates: past it, pin some inputs.
pub const MAX_COMBINATIONS: usize = 4096;

/// One enumerated input and the values it takes.
#[derive(Debug, Clone)]
pub struct Axis {
    pub input: String,
    pub key: bool,
    pub values: Vec<Value>,
    /// A dependent input's clause (R-104): the axis is in a combination
    /// only where it holds over the axes before it.
    pub guard: Vec<crate::ast::Lit>,
}

/// The values a type bounds: an `enum`'s members, `bool`'s two.
fn bounded(t: &TypeExpr) -> Option<Vec<Value>> {
    match t {
        TypeExpr::Name(n) if n == "bool" => Some(vec![Value::Bool(false), Value::Bool(true)]),
        // The values `x in T` enumerates (R-70).
        t => crate::types::members(t).map(|ms| ms.into_iter().map(Value::Str).collect()),
    }
}

/// The stack's input space: an axis per input neither `pinned` nor left
/// to its default. `applied(k)`: the values of the key `k` the stack's
/// deployments were applied with, for a key whose type does not bound it.
pub fn space(
    declared: &[Declared],
    pinned: &[String],
    applied: &dyn Fn(&str) -> Vec<String>,
) -> Result<Vec<Axis>> {
    let mut axes = Vec::new();
    let mut unbounded = Vec::new();
    // The stack's own inputs and every used module's (R-55), each leaf of
    // an object its own axis (R-54); one a `use` block gives is that, and
    // one a `set` gives is the program's to decide (R-38).
    for d in declared.iter().filter(|d| !d.bound && !d.given) {
        let Some(name) = &d.address else { continue };
        let i = &d.decl;
        if pinned.iter().any(|p| {
            p == name
                || name
                    .strip_prefix(p.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        }) {
            continue;
        }
        let values = match bounded(&i.ty) {
            Some(vs) => vs,
            None if i.key => applied(name).into_iter().map(Value::Str).collect(),
            None => Vec::new(),
        };
        // A dependent input declared twice (R-104) is one axis per
        // declaration, each under its own clause.
        if !values.is_empty() {
            axes.push(Axis {
                input: name.clone(),
                key: i.key,
                values,
                guard: i.guard.clone(),
            });
        } else if i.default.is_none() {
            unbounded.push(format!(
                "{} {}: {}",
                if i.key { "key" } else { "input" },
                name,
                crate::inputs::type_text(&i.ty)
            ));
        }
    }
    if !unbounded.is_empty() {
        bail!(
            "test: {} neither pinned nor bounded by its type, and with no default: give a \
             value (`k=v` for a key, `--set k=v` for an input) or an enum type",
            unbounded.join(", ")
        );
    }
    let n = axes
        .iter()
        .try_fold(1usize, |n, a| n.checked_mul(a.values.len()));
    if n.is_none_or(|n| n > MAX_COMBINATIONS) {
        bail!(
            "test: the input space is more than {MAX_COMBINATIONS} combinations ({}); pin some \
             inputs with `k=v` or `--set k=v`",
            axes.iter()
                .map(|a| format!("{} x{}", a.input, a.values.len()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(axes)
}

/// Every combination of the axes' values, the first axis slowest. A
/// dependent input's axis (R-104) is only in the combinations its clause
/// holds in, decided over the axes before it; a clause that reads more is
/// the evaluation's, and the axis is in every one.
pub fn combinations(axes: &[Axis]) -> Vec<Vec<(String, Value)>> {
    let mut out: Vec<Vec<(String, Value)>> = vec![Vec::new()];
    for a in axes {
        out = out
            .into_iter()
            .flat_map(|c| {
                let at: std::collections::BTreeMap<String, Value> = c.iter().cloned().collect();
                let declared =
                    a.guard.is_empty() || crate::lint::guard_holds(&a.guard, &at) != Some(false);
                let values: Vec<Option<&Value>> = match declared {
                    true => a.values.iter().map(Some).collect(),
                    false => vec![None],
                };
                values.into_iter().map(move |v| {
                    let mut c = c.clone();
                    if let Some(v) = v {
                        c.push((a.input.clone(), v.clone()));
                    }
                    c
                })
            })
            .collect();
    }
    out
}

/// The command that plans the combination: `dform plan STACK k=v --set
/// i=v`, keys as the target, every other input as `--set`.
pub fn reproduce(stack: &str, keys: &[(String, String)], sets: &[(String, String)]) -> String {
    let mut out = format!("dform plan {stack}");
    for (k, v) in keys {
        out.push_str(&format!(" {k}={}", shell_word(v)));
    }
    for (k, v) in sets {
        out.push_str(&format!(" --set {k}={}", shell_word(v)));
    }
    out
}

/// `v`, quoted for a shell when it needs it.
fn shell_word(v: &str) -> String {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@,+".contains(c))
    {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{InputDecl, Span, Term};

    fn input(name: &str, ty: TypeExpr, default: Option<Value>, key: bool) -> Declared {
        Declared::new(
            "",
            InputDecl {
                name: name.into(),
                ty,
                default: default.map(Term::Val),
                refinement: Vec::new(),
                key,
                guard: Vec::new(),
                fields: Vec::new(),
                span: Span::default(),
            },
            Some(name.into()),
            false,
        )
    }

    fn env() -> TypeExpr {
        TypeExpr::Apply(
            "enum".into(),
            vec![TypeExpr::Str("dev".into()), TypeExpr::Str("prod".into())],
        )
    }

    #[test]
    fn enums_and_bools_are_enumerated_and_the_rest_defaults() {
        let declared = [
            input("env", env(), Some(Value::Str("dev".into())), true),
            input("public", TypeExpr::Name("bool".into()), None, false),
            input(
                "size",
                TypeExpr::Name("int".into()),
                Some(Value::Int(1)),
                false,
            ),
        ];
        let axes = space(&declared, &[], &|_| Vec::new()).unwrap();
        let names: Vec<&str> = axes.iter().map(|a| a.input.as_str()).collect();
        assert_eq!(names, ["env", "public"]);
        let all = combinations(&axes);
        assert_eq!(all.len(), 4);
        assert_eq!(
            all[1],
            [
                ("env".to_string(), Value::Str("dev".into())),
                ("public".to_string(), Value::Bool(true))
            ]
        );
        // A pinned input is no axis.
        let axes = space(&declared, &["env".into()], &|_| Vec::new()).unwrap();
        assert_eq!(combinations(&axes).len(), 2);
    }

    #[test]
    fn an_unbounded_input_with_no_default_is_an_error_naming_it() {
        let declared = [
            input("region", TypeExpr::Name("string".into()), None, false),
            input("stage", TypeExpr::Name("string".into()), None, true),
        ];
        let e = space(&declared, &[], &|_| Vec::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("input region: string, key stage: string"), "{e}");
        // A key takes the values its deployments were applied with.
        let axes = space(&declared, &["region".into()], &|_| vec!["blue".into()]).unwrap();
        assert_eq!(axes[0].values, [Value::Str("blue".into())]);
    }

    #[test]
    fn reproduce_quotes_what_a_shell_would_split() {
        assert_eq!(
            reproduce(
                "shop",
                &[("env".into(), "prod".into())],
                &[("note".into(), "a b".into())]
            ),
            "dform plan shop env=prod --set note='a b'"
        );
    }
}
