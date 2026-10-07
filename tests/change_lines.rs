//! After R-149 amendments 4 and 5: a change line's site column is the
//! rule and the binding, the kind choosing which. A create's bindings its
//! address and body do not show; an update's `path = before → after`; a
//! replace's attribute that forces it; a delete's why it is gone (the
//! clause that stopped holding, `not in the program`, a rename guess),
//! its attributes `path = value`, no `was`; a destroy's nothing.

mod common;
use common::Scratch;

fn dev(s: &Scratch, args: &[&str]) -> common::Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    common::Run::from(
        common::dform()
            .args(&all)
            .env("NO_COLOR", "1")
            .current_dir(&s.dir)
            .output()
            .unwrap(),
    )
}

const BEFORE: &str = r#"
input agents: int = 2
use fake
resource db.postgres a { size = 1 }
resource db.postgres b { size = 2, tier = "x" }
resource db.postgres old { size = 5 }
resource db.postgres "n${n}" { size = n } where n in [0, 1], n < agents
resource net.vpc v { cidr = "10.0.0.0/16" }
"#;

const AFTER: &str = r#"
input agents: int = 1
use fake
resource db.postgres a { size = 3 }
resource db.postgres c { size = 2, tier = "x" }
resource db.postgres "n${n}" { size = n } where n in [0, 1], n < agents
resource net.vpc v { cidr = "10.9.0.0/16" }
"#;

#[test]
fn each_kind_of_change_says_its_own_why() {
    let s = Scratch::new("change-lines");
    s.write("p.df", BEFORE);
    dev(&s, &["apply", "--yes"]).success();
    s.write("p.df", AFTER);
    let r = dev(&s, &["plan"]).success();
    let line = |start: &str| {
        r.stdout
            .lines()
            .find(|l| l.starts_with(start))
            .unwrap_or_else(|| panic!("{start}\n{}", r.stdout))
            .to_string()
    };
    assert!(
        line("  ~ db.postgres a").ends_with("p.df:4"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\n      size = 1 → 3\n"), "{}", r.stdout);
    assert!(
        line("  - db.postgres b").ends_with("  renamed?  c is created with the same values"),
        "{}",
        r.stdout
    );
    assert!(
        line("  - db.postgres n1").ends_with("p.df:6  n < agents: agents = 1"),
        "{}",
        r.stdout
    );
    assert!(
        line("  - db.postgres old").ends_with("  not in the program"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\n      size = 5\n"), "{}", r.stdout);
    assert!(!r.stdout.contains(" was "), "{}", r.stdout);
    assert!(
        line("  ± net.vpc v").ends_with("p.df:7  cidr forces replace"),
        "{}",
        r.stdout
    );
    // A destroy: the operation is the reason.
    let r = dev(&s, &["plan", "--destroy"]).success();
    assert!(
        r.stdout
            .lines()
            .filter(|l| l.starts_with("  - "))
            .all(|l| !l["  - ".len()..].contains("  ")),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\n      size = 1\n"), "{}", r.stdout);
}

/// A create's binding its address and body do not show is in its site
/// column; one they show is not.
#[test]
fn a_create_names_the_binding_its_address_hides() {
    let s = Scratch::new("change-lines-create");
    s.write(
        "p.df",
        r#"
use fake
decl zone(z: string, region: string)
zone("a", "us-east-1")
resource net.vpc "z-${z}" { cidr = "10.1.0.0/16" } where zone(z, r), r != "eu"
resource net.vpc "y-${z}" { cidr = "10.2.0.0/16", tags.region = r } where zone(z, r)
"#,
    );
    let r = dev(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("  + net.vpc z-a  p.df:5  with r = \"us-east-1\"\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  + net.vpc y-a  p.df:6\n"),
        "{}",
        r.stdout
    );
}
