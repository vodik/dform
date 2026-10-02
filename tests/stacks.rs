//! State is scoped to the program: two programs against one `dform.state/` do not
//! see each other's resources.

mod common;
use common::Scratch;

const NET: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
provider fake
"#;

const DB: &str = r#"edition 2026

resource db.postgres main { backup_days = 7 }
provider fake
"#;

#[test]
fn a_second_program_does_not_plan_deletes_of_the_first() {
    let s = Scratch::project("stacks");
    s.write("net.df", NET);
    s.write("db.df", DB);

    s.run(&["apply", "net.df"]).success();
    assert!(s.path("dform.state/net/state.json").exists());

    let db = s.run(&["plan", "db.df"]).success();
    assert_eq!(
        db.summary(),
        "plan: 1 deformation (1 create)",
        "{}",
        db.stdout
    );

    s.run(&["apply", "db.df"]).success();
    let net = s.run(&["plan", "net.df"]).success();
    assert_eq!(net.summary(), "stack net is undeformed", "{}", net.stdout);
}
