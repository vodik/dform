//! `plan -q` (R-79) says a conflict's object witness as the plan lays a
//! value out, on one line in the order the program wrote it: never the
//! witness's JSON (`{"component":"edge","team":"ops"}`, by key).

mod common;
use common::Scratch;

#[test]
fn a_bare_conflict_says_an_object_as_the_program_writes_it() {
    let s = Scratch::new("bare-witness");
    s.write(
        "p.df",
        "use fake\n\
         ok(1)\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\", tags = { team: \"shop\", component: \"network\" } }\n\
         set main.tags = { team: \"ops\", component: \"edge\" } where ok(1)\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "-q", "p.df"])
        .failure();
    let witnesses: Vec<&str> = r
        .stdout
        .lines()
        .filter(|l| l.starts_with("    normal "))
        .map(|l| l.split("  from ").next().unwrap_or(l))
        .collect();
    assert_eq!(
        witnesses,
        [
            "    normal { team: \"ops\", component: \"edge\" }",
            "    normal { team: \"shop\", component: \"network\" }",
        ],
        "{}",
        r.stdout
    );
}
