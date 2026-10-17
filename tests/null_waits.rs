//! An entirely unknown value waits, whatever reads it (Rule 2, R-199): a
//! call given a null for an argument has nothing to compute from, so the
//! instance waits for it, never an error and never a wrong answer; a null
//! inside an otherwise known argument is carried by a function declared
//! `forwards nulls` (tests/pending_fields.rs). On the mock: `db.postgres`'s
//! `endpoint` is computed.

use crate::common;
use common::Scratch;

fn plan(name: &str, program: &str) -> common::Run {
    let s = Scratch::project(name);
    s.write(
        "main.df",
        &format!("use fake\n\nresource db.postgres pg {{ name = \"pg\" }}\n{program}"),
    );
    s.run(&["dev", "--world", "w.json", "plan", "main.df"])
}

/// A resource's value body that is unknown whole waits for it (`__body`
/// carries an unknown inside an object, not one that is the object).
#[test]
fn a_value_body_unknown_whole_waits_for_it() {
    let r = plan(
        "null-waits-body",
        "let conf(e) = e\nresource compute.vm c = conf(pg.endpoint)\n",
    )
    .success();
    assert!(r.stdout.contains("over 2 ticks"), "{}", r.stdout);
    assert!(r.stdout.contains("  + compute.vm c "), "{}", r.stdout);
}

/// A range with an unknown end waits for it, as a range of unknown
/// membership does: neither reads the membership false nor is an error.
#[test]
fn a_range_with_an_unknown_end_waits_for_it() {
    let r = plan(
        "null-waits-range",
        "resource compute.vm v { name = \"v\" } where \"m\" in \"a\"..pg.endpoint\n\
         resource compute.vm w { name = \"w\" } where pg.endpoint in \"a\"..\"z\"\n",
    )
    .success();
    for vm in ["v", "w"] {
        let want = format!("compute.vm {vm}      main.df:");
        let line = r.stdout.lines().find(|l| l.contains(&want));
        assert!(
            line.is_some_and(|l| l.ends_with("waits on pg.endpoint")),
            "{vm}: {}",
            r.stdout
        );
    }
}
