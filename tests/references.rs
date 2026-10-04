//! References after R-42: `r in T` with `r` bound by a reference column is
//! a type test, so a plan row of a deleted resource binds; a reference
//! interpolates as its address, `T["A"]`, typed by `in` or not; a scoped
//! address is left alone by `modules::scoped_term`.

mod common;
use common::{Scratch, mock};

/// Applies two VPCs and a namespace, then plans a program with none of
/// them and the checks `checks`: every row of the plan is a delete.
fn deletes(name: &str, checks: &str) -> common::Run {
    let s = Scratch::new(name);
    s.write(
        "p.df",
        "\n\nprovider fake\nprovider k8s\n\n\
         resource net.vpc a { cidr = \"10.0.0.0/16\" }\n\
         resource net.vpc b { cidr = \"10.1.0.0/16\" }\n\
         resource k8s.namespace ns { metadata.name = \"ns\" }\n",
    );
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        &format!("\n\nprovider fake\nprovider k8s\n\n{checks}"),
    );
    mock(&s, &["plan"])
}

/// `requires_approval(sg, ..) where deformation(action, sg, _), sg in T`
/// (README): the type test passes a delete, in either order.
#[test]
fn in_a_type_tests_a_bound_reference() {
    for checks in [
        "deny \"vpc ${action}: ${r}\" where deformation(action, r, _), r in net.vpc\n",
        "deny \"vpc ${action}: ${r}\" where r in net.vpc, deformation(action, r, _)\n",
    ] {
        let r = deletes("refs-type-test", checks).failure();
        assert!(
            r.stdout
                .contains("! vpc delete: net.vpc[\"a\"]\n! vpc delete: net.vpc[\"b\"]\n"),
            "{checks}{}",
            r.stdout
        );
        assert_eq!(r.stdout.matches("vpc delete").count(), 2, "{}", r.stdout);
    }
}

/// A reference interpolates as its address, whether `in` types it or not.
#[test]
fn a_reference_interpolates_as_its_address() {
    let r = deletes(
        "refs-interpolation",
        "deny \"typed: ${r}\" where deformation(\"delete\", r, _), r in k8s.namespace\n\
         deny \"untyped: ${r}\" where deformation(\"delete\", r, _), r == k8s.namespace[\"ns\"]\n",
    )
    .failure();
    assert!(
        r.stdout.contains("! typed: k8s.namespace[\"ns\"]\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("! untyped: k8s.namespace[\"ns\"]\n"),
        "{}",
        r.stdout
    );
}
