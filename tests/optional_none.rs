//! A function whose result is optional (`T?`) answers none for some
//! arguments: `regex.capture("16.0.4", "^v(.+)$", 1)` (no match).
//! Such a none reaching a cell is an error at the entry naming the call and
//! the attribute or `let` it was to give (R-119), directly, inside an
//! object, or through a `let`; never "non-ground head" with no place.

mod common;
use common::Scratch;

const CALL: &str = "regex.capture(\"16.0.4\", \"^v(.+)$\", 1) answered nothing";

fn plan(name: &str, src: &str) -> common::Run {
    let s = Scratch::new(name);
    s.write("p.df", src);
    s.run(&["plan", "p.df"]).failure()
}

#[test]
fn a_none_in_an_entry_is_located() {
    let r = plan(
        "none-entry",
        "use fake\nlet release = \"16.0.4\"\nresource net.vpc a {\n  cidr = \"10.0.0.0/16\"\n  \
         name = regex.capture(release, \"^v(.+)$\", 1)\n}\n",
    );
    assert!(
        r.stderr
            .contains(&format!("p.df:5:3: {CALL}, so net.vpc a.name has no value")),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("non-ground"), "{}", r.stderr);
}

#[test]
fn a_none_inside_an_object_is_located() {
    let r = plan(
        "none-object",
        "use fake\nlet release = \"16.0.4\"\nresource net.vpc a {\n  cidr = \"10.0.0.0/16\"\n  \
         tags = { image: regex.capture(release, \"^v(.+)$\", 1) }\n}\n",
    );
    assert!(
        r.stderr
            .contains(&format!("p.df:5:3: {CALL}, so net.vpc a.tags has no value")),
        "{}",
        r.stderr
    );
}

#[test]
fn a_none_through_a_let_is_located_at_the_let() {
    let r = plan(
        "none-let",
        "use fake\nlet release = \"16.0.4\"\n\
         let image = regex.capture(release, \"^v(.+)$\", 1)\n\
         resource net.vpc a {\n  cidr = \"10.0.0.0/16\"\n  name = image\n}\n",
    );
    assert!(
        r.stderr
            .contains(&format!("p.df:3:1: {CALL}, so let image has no value")),
        "{}",
        r.stderr
    );
}

/// A function that is not optional answers every input it takes; one it
/// does not (a tag where a digest is wanted) is an error at the entry
/// naming the call (R-134).
#[test]
fn a_total_functions_bad_input_at_a_cell_is_located() {
    let r = plan(
        "total-entry",
        "use fake\nlet release = \"16.0.4\"\nresource net.vpc a {\n  cidr = \"10.0.0.0/16\"\n  \
         name = oci.with_digest(\"codeberg.org/forgejo/forgejo\", release)\n}\n",
    );
    assert!(
        r.stderr.contains(
            "p.df:5:3: oci.with_digest(codeberg.org/forgejo/forgejo, \"16.0.4\") is not defined \
             for these arguments, so net.vpc a.name has no value"
        ),
        "{}",
        r.stderr
    );
}
