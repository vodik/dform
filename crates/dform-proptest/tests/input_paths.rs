//! The object-input check (`inputs::violations`, R-181) over paths of
//! every length: an object input's leaves and a contribution to it at any
//! key (the input, one of its fields, a leaf, a path outside it), of any
//! value (an object, nested or empty, a scalar, a null). It never panics,
//! and every "has no field F" names a nonempty F the value has under the
//! key. The alphabet holds a two-byte character, so a slice by byte
//! count off a char boundary would panic too.

use dform_core::ast::{Atom, InputDecl, Span, Term, TypeExpr};
use dform_core::inputs::{Declared, violations};
use dform_core::value::Value;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

fn segment() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "é", "ab", "aé"]).prop_map(String::from)
}

fn path(min: usize, max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), min..=max).prop_map(|s| s.join("."))
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        segment().prop_map(Value::Str),
        Just(Value::Int(1)),
        Just(Value::Null {
            label: "x".into(),
            class: dform_core::value::NullClass::Open,
            ty: String::new(),
        }),
    ];
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop::collection::btree_map(segment(), inner, 0..3).prop_map(Value::Obj)
    })
}

fn decl(name: &str) -> InputDecl {
    InputDecl {
        name: name.to_string(),
        ty: TypeExpr::Name("string".into()),
        default: None,
        refinement: Vec::new(),
        key: false,
        guard: Vec::new(),
        fields: Vec::new(),
        span: Span::default(),
    }
}

proptest! {
    #[test]
    fn the_object_input_check_never_slices_past_a_path(
        object in path(1, 3),
        fields in prop::collection::vec(path(1, 2), 1..4),
        key in path(0, 4),
        under in any::<bool>(),
        scoped in any::<bool>(),
        v in value(),
    ) {
        let scope = if scoped { "m" } else { "" };
        let declared: Vec<Declared> = fields
            .iter()
            .map(|f| {
                let name = format!("{object}.{f}");
                let address = if scoped { format!("m.{name}") } else { name.clone() };
                Declared::new(scope, decl(&name), Some(address), true)
            })
            .collect();
        // A key under the object (its own, or deeper), or any path at all.
        let k = match (under, key.is_empty()) {
            (true, true) => object.clone(),
            (true, false) => format!("{object}.{key}"),
            (false, _) => key,
        };
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let fact = Atom {
            pred: "attr".into(),
            args: vec![s("input"), s(scope), s(&k), Term::Val(v.clone())],
            record: None::<BTreeMap<String, Term>>,
            span: Span::default(),
        };
        let out = violations(&BTreeSet::from([fact]), &declared);
        for m in &out {
            if let Some((_, field)) = m.split_once(" has no field ") {
                prop_assert!(!field.is_empty(), "{m}");
                let mut at = Some(&v);
                for seg in field.split('.') {
                    at = match at {
                        Some(Value::Obj(o)) => o.get(seg),
                        _ => None,
                    };
                }
                prop_assert!(at.is_some(), "{m}: the value has no {field}: {v:?}");
            }
        }
    }
}
