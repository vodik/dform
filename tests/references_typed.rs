//! A reference is a reference everywhere (R-185): a field read of a value
//! that is not an object is an error, never a literal that does not hold,
//! so a deny cannot pass without checking.

mod common;
use common::{Scratch, mock};

/// A program under the fake provider.
fn scratch(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", &format!("\nuse fake\nuse k8s\n\n{src}"));
    s
}

/// A field of a value the compiler cannot type (a document's) that is a
/// string at run time: an error at the deny naming the read and the value,
/// under `plan` and `dform test` alike, where it was a deny that held.
#[test]
fn a_field_of_a_string_is_an_error_at_run_time() {
    let s = scratch(
        "refs-typed-run-time",
        "let doc = json.decode(\"{\\\"tier\\\": \\\"gold\\\"}\")\n\n\
         deny \"tier is not gold\" where not doc.tier.name == \"gold\"\n",
    );
    let want = "`doc.tier.name`: `doc.tier` is the string \"gold\", which has no field `name`";
    let r = mock(&s, &["plan"]).failure();
    assert!(r.stderr.contains(want), "{}", r.stderr);
    assert!(!r.stdout.contains("tier is not gold"), "{}", r.stdout);
    let r = s.run(&["test", "p.df"]).failure();
    assert!(r.stdout.contains(want), "{}{}", r.stdout, r.stderr);
}
