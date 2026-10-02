//! A read in a resource's field that finds no row holds the whole block
//! back: the resource is not derived. That is reported, not silent: a
//! `warn` naming the read, where it is written and the resource it held
//! back (ticket "A resource whose field read finds no value vanishes
//! silently").

mod common;
use common::Scratch;

const MSG: &str = "a field read found no value: the resource is not derived";

fn plan(s: &Scratch, src: &str) -> common::Run {
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
}

#[test]
fn a_misspelled_attribute_in_a_field_warns_at_the_read() {
    let s = Scratch::new("field-read");
    let r = plan(
        &s,
        r#"edition 2026
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a {
  cidr = "10.0.1.0/24"
  name = "a-${main.cidrr}"
}
provider fake
"#,
    )
    .success();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    for want in [r#""resource":"net.subnet.a""#, r#""at":"p.df:5:15""#] {
        assert!(r.stderr.contains(want), "{want}: {}", r.stderr);
    }
    assert!(!r.stdout.contains("net.subnet[\"a\"]"), "{}", r.stdout);
}

/// A read that finds its row, a block whose own `if` does not hold, and a
/// read of an object this deployment does not have, are quiet: the block
/// is held back by the program, not by a misspelled read.
#[test]
fn a_read_that_finds_a_row_or_a_gated_block_is_quiet() {
    let s = Scratch::new("field-read-ok");
    let r = plan(
        &s,
        r#"edition 2026
input env: string = "prod"
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a {
  cidr = "10.0.1.0/24"
  name = "a-${main.cidr}"
}
resource net.subnet b {
  cidr = "10.0.2.0/24"
  name = "b-${main.cidrr}"
} where env == "staging"
resource net.vpc gated {
  cidr = "10.1.0.0/16"
} where env == "staging"
resource net.subnet c {
  cidr = "10.0.3.0/24"
  name = "c-${gated.cidr}"
}
provider fake
"#,
    )
    .success();
    assert!(!r.stderr.contains(MSG), "{}", r.stderr);
    assert!(r.stdout.contains("net.subnet[\"a\"]"), "{}", r.stdout);
}
