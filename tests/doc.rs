//! `dform doc` (R-6): after the project's documented items, the standard
//! library, every callable function of std/*.df with its signature,
//! summary and example, from the registry.

mod common;
use common::repo;
use dform::functions::{SOURCES, registry};

#[test]
fn dform_doc_renders_the_standard_library() {
    let demo = repo().join("examples/demo");
    let out = common::dform()
        .args(["-C", demo.to_str().unwrap(), "doc"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let md = String::from_utf8(out.stdout).unwrap();
    // The project's items first, then one section per signature file.
    let std = md
        .find("\n## std/prelude.df\n")
        .expect("the prelude's section");
    assert!(md[..std].contains("### input `env`"), "{md}");
    for (file, _) in SOURCES {
        assert!(md.contains(&format!("\n## {file}\n")), "{file} in\n{md}");
    }
    assert!(
        md.contains(
            "### function `inet.subnet`\n\n```dform\nfn inet.subnet(net: inet, bits: int, n: int) -> inet?\n```\n"
        ),
        "{md}"
    );
    // Every callable function, none of the lowering's own.
    for f in registry().functions() {
        let heading = format!("### function `{}`\n", f.name);
        assert_eq!(md.contains(&heading), !f.internal, "{}", f.name);
    }
}
