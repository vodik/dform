//! Instancing (R-211): each copy of a component is its definition's
//! items lowered again under it, from the program's nodes. A definition's
//! records are placed by its own `decl`s in every copy; a record its
//! scope cannot place fails the evaluation, never the load (what
//! `deployment::load` and the language server read), whether or not a
//! copy of its definition is made.

use crate::common;
use common::facts;
use dform_core::engine;
use dform_core::parser::parse_program;

/// A component whose own `decl` names its relation's fields.
const COMPONENT: &str = r#"component c {
  decl zone(name, index)
  zone("a", 1)
  zone("b", 2)
  seen(n) where zone({ name = n, index = 2 })
}
"#;

#[test]
fn a_definitions_record_is_placed_by_its_own_decl_in_each_copy() {
    let src = format!("{COMPONENT}resource c x {{}}\nresource c y {{}}\n");
    for copy in ["x", "y"] {
        let seen = format!("{copy}::seen");
        assert_eq!(facts(&src, &seen), [format!("{seen}(\"b\")")], "{seen}");
    }
}

#[test]
fn a_record_its_scope_cannot_place_fails_the_evaluation_copied_or_not() {
    let bad = COMPONENT.replace("index = 2", "size = 2");
    for src in [bad.clone(), format!("{bad}resource c x {{}}\n")] {
        let program = parse_program(&src).unwrap_or_else(|e| panic!("{e:#}\n{src}"));
        let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
        assert!(
            e.contains("unknown field 'size' for predicate 'zone'"),
            "{e}\n{src}"
        );
    }
}
