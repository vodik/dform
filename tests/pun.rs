//! A pun is one simple name on both sides (R-33, R-216): `color` alone is
//! `color = color`, `{ color }` is `{ color: color }`. A bare dotted path
//! is no pun, in every block that takes entries: `spec.selector.color`
//! alone is an error whose help writes it out or puns at the last object,
//! `spec.selector = { color }`.

mod common;
use common::error;
use dform_core::engine;
use dform_core::parser::parse_file;
use dform_core::spell;

/// Everything the program file `src` derives, as `spell::atom` prints it.
fn derived(src: &str) -> Vec<String> {
    let program = parse_file("t.df", src).unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = engine::eval(&program, &[]).unwrap();
    r.facts.iter().map(spell::atom).collect()
}

/// The error at the entry, in a resource block, a provider's `use` block
/// and a copy's block alike, its help the two ways to write it computed
/// from the path.
#[test]
fn a_bare_dotted_path_is_no_pun() {
    let e = error("let color = \"blue\"\nresource k8s.service s {\n  spec.selector.color\n}\n");
    assert!(
        e.contains(
            "`spec.selector.color` alone is not a pun: a pun is one simple name on both sides"
        ),
        "{e}"
    );
    assert!(
        e.contains("write `spec.selector.color = color`, or `spec.selector = { color }`"),
        "{e}"
    );
    let e = error("let region = \"r\"\nuse fake { conf.region }\n");
    assert!(
        e.contains("write `conf.region = region`, or `conf = { region }`"),
        "{e}"
    );
    let e = error(
        "component m {\n  input net: { cidr: string }\n}\n\
         let cidr = \"10.0.0.0/16\"\nresource m a { net.cidr }\n",
    );
    assert!(
        e.contains("write `net.cidr = cidr`, or `net = { cidr }`"),
        "{e}"
    );
    let e = error("let b = 1\nresource net.vpc v {\n  a[0].b @default\n}\n");
    assert!(e.contains("write `a[0].b = b`, or `a[0] = { b }`"), "{e}");
}

/// The explicit entry and the pun at the last object derive the same
/// resource; a bare name and its `k = k` do too.
#[test]
fn the_written_forms_derive_the_same() {
    let one = |entry: &str| {
        derived(&format!(
            "let color = \"blue\"\nresource k8s.service s {{\n  {entry}\n}}\n"
        ))
    };
    let explicit = one("spec.selector.color = color");
    assert!(
        explicit
            .iter()
            .any(|f| f == "attr(\"k8s.service\", \"s\", \"spec\", {selector: {color: \"blue\"}})"),
        "{explicit:?}"
    );
    assert_eq!(one("spec.selector = { color }"), explicit);
    assert_eq!(one("spec.selector = { color: color }"), explicit);
    let bare = one("color");
    assert!(
        bare.iter()
            .any(|f| f == "attr(\"k8s.service\", \"s\", \"color\", \"blue\")"),
        "{bare:?}"
    );
    assert_eq!(one("color = color"), bare);
}

/// A single name still puns where it did: a resource block's entry (with
/// a rank), an object literal's field and a copy's input. (A `use`
/// block's is tests/module_scope.rs's, a keyed read's
/// tests/apply_order.rs's.)
#[test]
fn a_single_name_puns_in_every_position() {
    let resource = |entries: &str| {
        derived(&format!(
            "let zone = \"a\"\nlet tags = {{ team: \"x\" }}\n\
             resource net.subnet s {{\n  {entries}\n}}\n"
        ))
    };
    assert_eq!(
        resource("zone\n  tags @default\n  meta = { zone }"),
        resource("zone = zone\n  tags = tags @default\n  meta = { zone: zone }")
    );
    let copy = |entries: &str| {
        derived(&format!(
            "component m {{\n  input name: string\n}}\n\
             let name = \"n\"\nresource m a {{ {entries} }}\n"
        ))
    };
    assert_eq!(copy("name"), copy("name = name"));
}
