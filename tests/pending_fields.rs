//! A value with an unknown inside is pending only there (the user's
//! project, 2026-10-16, edge 3: an A record whose target is computed
//! could not be a whole-resource function). A call answers its own copy
//! of the let, and its value carries an argument's unknown where the body
//! puts it; a resource's body made of that value, `..record(..)` or `=
//! record(..)`, is known where it is known, the unknown an edge applied
//! with the resource that computes it. On the mock: `db.postgres`'s
//! `endpoint` is computed.

use crate::common;
use common::Scratch;

const PROGRAM: &str = r#"use fake

resource db.postgres pg { name = "pg" }

let record(name, target) = { name = name, target = target }

resource compute.vm a { ..record("www", pg.endpoint) }
resource compute.vm b = record("api", pg.endpoint)
"#;

#[test]
fn a_record_from_a_function_plans_its_known_fields_and_applies_the_rest() {
    let s = Scratch::project("pending-fields");
    s.write("main.df", PROGRAM);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "main.df"])
        .success();
    for want in [
        "  + compute.vm a    main.df:7\n      name = \"www\"\n      target = pg.endpoint\n",
        "  + compute.vm b    main.df:8\n      name = \"api\"\n      target = pg.endpoint\n",
    ] {
        assert!(r.stdout.contains(want), "{want}---\n{}", r.stdout);
    }
    // One tick: the unknown is an edge, filled when `pg` is made.
    assert!(r.stdout.contains("over 1 tick"), "{}", r.stdout);
    s.run(&["dev", "--world", "w.json", "apply", "--yes", "main.df"])
        .success();
    let world: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let endpoint = &world["resources"]["db.postgres::pg"]["computed"]["endpoint"];
    for vm in ["compute.vm::a", "compute.vm::b"] {
        assert_eq!(
            &world["resources"][vm]["attrs"]["target"], endpoint,
            "{vm}: {world}"
        );
    }
}
