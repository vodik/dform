//! After R-149: a run that refuses prints each violation as every error
//! is printed (`report::refusals`): a deny's message and its site, then
//! its bindings as `key = value`, never the context's JSON.

mod common;
use common::Scratch;

const PROG: &str = r#"
use fake
resource db.postgres main { size = 1 }
deny "image not pinned" { image: "traefik:v3.7", size: s } where d in db.postgres, s = d.size
"#;

/// The plan's refusal names the deny and its bindings, as a program
/// writes the values.
#[test]
fn a_refused_plan_prints_a_deny_by_its_bindings() {
    let s = Scratch::new("violations-plan");
    s.write("p.df", PROG);
    let out = common::dform()
        .args(["dev", "--world", "w.json", "plan", "p.df"])
        .env("NO_COLOR", "1")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    let r = common::Run::from(out);
    assert!(!r.stderr.contains("ctx="), "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "refused  image not pinned  p.df:4\n  └─ image = \"traefik:v3.7\", size = 1\n"
        ),
        "{}",
        r.stderr
    );
}
