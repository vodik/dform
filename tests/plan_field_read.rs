//! A read in a resource's field that finds no row holds the whole block
//! back: the resource is not derived. That is reported, not silent: a
//! `warn` naming the read, where it is written and the resource it held
//! back, and a `deny` under strict mode (ticket "A resource whose field
//! read finds no value vanishes silently").

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
        r#"edition 2027
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a {
  cidr = "10.0.1.0/24"
  name = "a-${net.vpc.main.cidrr}"
}
"#,
    )
    .success();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    for want in [r#""resource":"net.subnet.a""#, r#""at":"p.df:5:14""#] {
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
        r#"edition 2027
input env: string = "prod"
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a {
  cidr = "10.0.1.0/24"
  name = "a-${net.vpc.main.cidr}"
}
resource net.subnet b {
  if env == "staging"
  cidr = "10.0.2.0/24"
  name = "b-${net.vpc.main.cidrr}"
}
resource net.vpc gated {
  if env == "staging"
  cidr = "10.1.0.0/16"
}
resource net.subnet c {
  cidr = "10.0.3.0/24"
  name = "c-${net.vpc.gated.cidr}"
}
"#,
    )
    .success();
    assert!(!r.stderr.contains(MSG), "{}", r.stderr);
    assert!(r.stdout.contains("net.subnet[\"a\"]"), "{}", r.stdout);
}

#[test]
fn under_strict_mode_it_is_a_deny() {
    let s = Scratch::new("field-read-strict");
    let r = plan(
        &s,
        r#"edition 2027
stack s { unknowns = "strict" }
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a {
  cidr = "10.0.1.0/24"
  name = "a-${net.vpc.main.cidrr}"
}
"#,
    )
    .failure();
    assert!(r.stderr.contains("constraint violations"), "{}", r.stderr);
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    assert!(r.stderr.contains(r#""at":"p.df:6:14""#), "{}", r.stderr);
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}
