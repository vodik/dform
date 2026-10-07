//! A string in an interpolation hole (R-175, docs/grammar.md "Strings and
//! interpolation"): a hole runs to its matching `}`, and a string in it is
//! the hole's own, its own holes nesting to any depth.

mod common;
use common::Scratch;

/// The private project's cloud-init: a `yaml.encode` call in a hole whose
/// object holds a plain string and a string with a hole of its own, over
/// two lines. It renders, `fmt` leaves it as written, and `why` reads it.
#[test]
fn a_string_in_a_hole_is_the_holes() {
    let s = Scratch::project("nested-strings");
    let src = "output braces = \"${str.upper(\"a}${\"b{\"}\")}\"\n\
               output init = agent_init\n\
               \n\
               let install = \"curl -sfL https://get.k3s.io |\"\n\
               let packages = [\"nftables\", \"curl\"]\n\
               let agent_init = \"#cloud-config\n\
               ${yaml.encode({ package_update: true, packages, runcmd: [\"systemctl enable nftables\", \"${install} sh -s - server\"] })}\"\n\
               use fake\n";
    s.write("main.df", src);
    let r = s
        .run(&["query", "--json", "attr(\"output\", \"\", k, v)", "main.df"])
        .success();
    let rows: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let v = |k: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["K"] == k)
            .unwrap_or_else(|| panic!("{k}: {}", r.stdout))["V"]
            .clone()
    };
    assert_eq!(v("braces"), "A}B{");
    assert_eq!(
        v("init"),
        "#cloud-config\npackage_update: true\npackages:\n- nftables\n- curl\nruncmd:\n\
         - systemctl enable nftables\n- curl -sfL https://get.k3s.io | sh -s - server\n"
    );
    s.run(&["fmt", "--check", "main.df"]).success();
    assert_eq!(s.read("main.df"), src);
}

/// A hole never closed is an error at its `${`, the line and the column
/// of the hole, not of the string.
#[test]
fn an_unclosed_hole_is_named_where_it_opens() {
    let s = Scratch::project("nested-strings-open");
    s.write("main.df", "output o = \"a ${b\"\nuse fake\n");
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("main.df:1:15: an interpolation `${` is never closed"),
        "{}",
        r.stderr
    );
}
