//! An attribute given at creation only (R-198): `lifecycle(r, "bootstrap",
//! "user_data")` says the value is sent when the object is made and not
//! compared after, so a rotated token in a server's user data is not a
//! replacement. A differing value is kept, and the plan says so under the
//! resource; a replace for another reason sends the current value.

mod common;
use common::{Run, Scratch, dform, repo, yes};

/// `dform ARGS` in `s`, as alice, at `now`.
fn run(s: &Scratch, now: &str, args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env("RANDOM_MASTER", "bootstrap-master")
        .env("DFORM_ACTOR", "alice")
        .env("DFORM_TEST_NOW", now);
    Run::from(c.output().unwrap())
}

const NOW: &str = "2026-10-08T09:00:00Z";

/// A server whose user data (write-only, `force_new`, as OVH's) holds a
/// k3s token, and a plain `force_new` note; both given at creation only
/// unless `facts` says otherwise.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \
               \"sensitive\"])\n\
               type_attr(compute.vm, \"note\", \"string\", [\"force_new\"])\n\
               type_attr(compute.vm, \"zone\", \"string\", [\"force_new\"])\n"),
    );
    program(&s, Server::default(), BOTH);
    s
}

const BOTH: &str =
    "lifecycle(vm, \"bootstrap\", \"user_data\")\nlifecycle(vm, \"bootstrap\", \"note\")\n";

#[derive(Clone, Copy)]
struct Server {
    boot: &'static str,
    note: &'static str,
    zone: &'static str,
    weight: u32,
}

impl Default for Server {
    fn default() -> Server {
        Server {
            boot: "one",
            note: "n1",
            zone: "z1",
            weight: 1,
        }
    }
}

fn program(s: &Scratch, v: Server, facts: &str) {
    s.write(
        "stacks/p.df",
        &format!(
            r##"use fake
let token = random.password("k3s")
resource compute.vm vm {{
  name = "vm"
  user_data = "#cloud-config {} token: ${{token}}"
  note = "{}"
  zone = "{}"
  weight = {}
}}
{facts}"##,
            v.boot, v.note, v.zone, v.weight
        ),
    );
}

fn world(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/p/remote.json")["resources"]["compute.vm::vm"]["attrs"].clone()
}

/// The create sends the value; once the object exists a differing value
/// is no change and no replace, though both attributes are `force_new`:
/// the plan says it keeps each, the object keeps what it was made with.
/// Without the facts the same program replaces the server.
#[test]
fn a_bootstrap_value_is_sent_at_create_and_kept_after() {
    let s = project("bootstrap-kept");
    run(&s, NOW, &["apply", "p"]).success();
    let made = world(&s);
    assert!(
        made["user_data"]
            .as_str()
            .is_some_and(|u| u.starts_with("#cloud-config one token: ")),
        "{made}"
    );
    assert_eq!(made["note"], "n1");

    let changed = Server {
        boot: "two",
        note: "n2",
        ..Server::default()
    };
    program(&s, changed, BOTH);
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(
        r.stdout,
        "= compute.vm vm  stacks/p.df:3\n    note differs (bootstrap): kept\n    user_data \
         differs (bootstrap): kept\nstack p is up to date\n"
    );
    // `-v` says the values, the object's first; a secret stays one.
    let r = run(&s, NOW, &["plan", "-v", "p"]).success();
    assert!(
        r.stdout.contains(
            "    note = \"n1\" → \"n2\"  (bootstrap): kept\n    user_data = (sensitive) → \
             (sensitive)  (bootstrap): kept\n"
        ),
        "{}",
        r.stdout
    );
    run(&s, NOW, &["apply", "p"]).success();
    assert_eq!(world(&s), made);

    program(&s, changed, "");
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 replace) over 1 tick",
        "{}",
        r.stdout
    );
}

/// An update for another reason leaves what the object was made with:
/// the world keeps it (a write-only one its provider keeps), state keeps
/// the digest it was made with, and the next plan still says so.
#[test]
fn an_update_keeps_what_the_object_was_made_with() {
    let s = project("bootstrap-update");
    run(&s, NOW, &["apply", "p"]).success();
    let made = world(&s);
    let digest = |s: &Scratch| {
        s.json("dform.state/p/state.json")["resources"]["compute.vm::vm"]["written"]["user_data"]
            .clone()
    };
    let was = digest(&s);
    program(
        &s,
        Server {
            boot: "two",
            note: "n2",
            weight: 2,
            ..Server::default()
        },
        BOTH,
    );
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert!(
        r.stdout.contains(
            "  ~ compute.vm vm  stacks/p.df:3\n      weight = 1 → 2\n      note differs \
             (bootstrap): kept\n      user_data differs (bootstrap): kept\n"
        ),
        "{}",
        r.stdout
    );
    run(&s, NOW, &["apply", "p"]).success();
    let now = world(&s);
    assert_eq!(now["weight"], 2);
    assert_eq!(
        (&now["note"], &now["user_data"]),
        (&made["note"], &made["user_data"])
    );
    assert_eq!(digest(&s), was);
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert!(
        r.stdout
            .ends_with("user_data differs (bootstrap): kept\nstack p is up to date\n"),
        "{}",
        r.stdout
    );
}

/// A replace for another reason makes the object again: the values given
/// at creation are sent as the program has them, lines of the replace
/// that force nothing.
#[test]
fn a_replace_for_another_reason_sends_the_current_value() {
    let s = project("bootstrap-replace");
    run(&s, NOW, &["apply", "p"]).success();
    program(
        &s,
        Server {
            boot: "two",
            note: "n2",
            zone: "z2",
            ..Server::default()
        },
        BOTH,
    );
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert!(
        r.stdout.contains(
            "  ± compute.vm vm  stacks/p.df:3  zone forces replace\n      zone = \"z1\" → \
             \"z2\"\n      note = \"n1\" → \"n2\"\n      user_data = (sensitive) → (sensitive)\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("kept"), "{}", r.stdout);
    run(&s, NOW, &["apply", "p"]).success();
    let now = world(&s);
    assert_eq!(now["note"], "n2");
    assert!(
        now["user_data"]
            .as_str()
            .is_some_and(|u| u.starts_with("#cloud-config two token: ")),
        "{now}"
    );
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(r.stdout, "stack p is up to date\n");
}

/// `--json` and the plan file carry the kept difference, redacted as a
/// change's values are; the plan file applies.
#[test]
fn the_plan_file_and_json_carry_the_kept_difference() {
    let s = project("bootstrap-file");
    run(&s, NOW, &["apply", "p"]).success();
    program(
        &s,
        Server {
            note: "n2",
            ..Server::default()
        },
        BOTH,
    );
    let r = run(&s, NOW, &["plan", "--json", "p"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["up_to_date"], true);
    assert_eq!(
        j["kept"],
        serde_json::json!([{
            "address": "compute.vm[\"vm\"]", "type": "compute.vm", "name": "vm",
            "path": "note", "before": "n1", "after": "n2", "lifecycle": "bootstrap",
            "site": {"at": "stacks/p.df:3", "statement": "resource compute.vm vm { .. }"},
        }])
    );
    run(&s, NOW, &["plan", "--out", "plan.json", "p"]).success();
    assert_eq!(
        s.json("plan.json")["kept"],
        serde_json::json!([{
            "type": "compute.vm", "name": "vm", "path": "note", "before": "n1", "after": "n2",
        }])
    );
    run(&s, NOW, &["apply", "plan.json"]).success();
    assert_eq!(world(&s)["note"], "n1");
}
