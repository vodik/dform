//! A team's custody (After R-164): `[secrets] recipients` seals each
//! deployment's master to every member's age key, and each opens it with
//! their own identity (`AGE_IDENTITY`, or the credential `age:NAME`); a
//! recipient added or removed in dform.toml is sealed to, or no longer,
//! by the next apply, with an audit entry, and `secrets list` says who
//! opens each epoch and who could (the offboarding list).

mod common;
use age::secrecy::ExposeSecret;
use common::{Run, Scratch, dform, repo, yes};

/// A member: their identity (secret) and recipient (public).
struct Member {
    identity: String,
    recipient: String,
}

fn member() -> Member {
    let id = age::x25519::Identity::generate();
    Member {
        identity: id.to_string().expose_secret().to_string(),
        recipient: id.to_public().to_string(),
    }
}

/// `dform ARGS` as the holder of `identity` (none: no identity at all).
fn run(s: &Scratch, identity: Option<&Member>, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env("DFORM_CREDENTIALS", s.path("no-credentials"))
        .env_remove("RANDOM_MASTER")
        .env_remove("AGE_IDENTITY")
        .env_remove("DFORM_TEST_PASSPHRASE");
    if let Some(m) = identity {
        c.env("AGE_IDENTITY", &m.identity);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// examples/crud-api, its masters sealed to `members` by name.
fn team(name: &str, members: &[(&str, &Member)], extra: &str) -> Scratch {
    let s = Scratch::new(name);
    common::copy_dir(&repo().join("examples/crud-api"), &s.dir);
    let _ = std::fs::remove_dir_all(s.path("dform.state"));
    recipients(&s, members, extra);
    s
}

fn recipients(s: &Scratch, members: &[(&str, &Member)], extra: &str) {
    let base = s.read("dform.toml");
    let base = base.split("\n[secrets]").next().unwrap().to_string();
    let list: Vec<String> = members
        .iter()
        .map(|(n, m)| format!("{n} = \"{}\"", m.recipient))
        .collect();
    s.write(
        "dform.toml",
        &format!(
            "{base}\n[secrets]\n{extra}recipients = {{ {} }}\n",
            list.join(", ")
        ),
    );
}

fn entries(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("dform.state/crud_api/state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == kind)
        .collect()
}

fn up_to_date(r: &Run) {
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
    assert!(!r.stderr.contains("planned without"), "{}", r.stderr);
}

/// Alice applies; Bob, the other member, plans and applies with his own
/// identity; Carol, no member, plans without the master.
#[test]
fn each_member_opens_the_master_with_their_own_identity() {
    let (alice, bob, carol) = (member(), member(), member());
    let s = team("recipients-team", &[("alice", &alice), ("bob", &bob)], "");
    run(&s, Some(&alice), &[], &["apply"]).success();
    assert!(!s.path("dform.state/crud_api/state.key").exists());
    let record = s.json("dform.state/crud_api/state.master");
    assert!(record["passphrase"].is_null(), "{record}");
    let mut want = vec![alice.recipient.clone(), bob.recipient.clone()];
    want.sort();
    assert_eq!(
        record["age"]["recipients"],
        serde_json::json!(want),
        "{record}"
    );
    let text = s.read("dform.state/crud_api/state.master");
    assert!(!text.contains(&alice.identity) && !text.contains(&bob.identity));
    up_to_date(&run(&s, Some(&bob), &[], &["plan"]).success());
    let r = run(&s, Some(&carol), &[], &["plan"]).success();
    assert!(
        r.stderr.contains(
            "crud_api: planned without its master (no age identity it is sealed to \
             (AGE_IDENTITY tried))"
        ),
        "{}",
        r.stderr
    );
    let r = run(&s, None, &[], &["plan"]).success();
    assert!(
        r.stderr
            .contains("no age identity (AGE_IDENTITY, or a credential age:NAME)"),
        "{}",
        r.stderr
    );
    // An identity from the operator's credentials, `age:NAME`.
    s.write("creds/age/bob", &format!("# bob's\n{}\n", bob.identity));
    let mut c = dform();
    c.args(["plan"])
        .current_dir(&s.dir)
        .env("DFORM_CREDENTIALS", s.path("creds"))
        .env_remove("AGE_IDENTITY")
        .env_remove("RANDOM_MASTER");
    up_to_date(&Run::from(c.output().unwrap()).success());
    // The audit log names who could open it from the start.
    let e = entries(&s, "recipients");
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0]["added"].as_array().unwrap().len(), 2, "{e:?}");
}

/// Bob leaves: dform.toml no longer names him, the plan says the next
/// apply stops sealing to him (which revokes nothing he opened), the apply
/// seals again and logs it, and Bob's identity opens nothing after; Carol
/// joins the same way. `secrets list` names who opens each epoch and who
/// could.
#[test]
fn a_recipient_removed_is_sealed_to_no_longer_and_listed() {
    let (alice, bob, carol) = (member(), member(), member());
    let s = team(
        "recipients-offboard",
        &[("alice", &alice), ("bob", &bob)],
        "",
    );
    run(&s, Some(&alice), &[], &["apply"]).success();
    let id = s.json("dform.state/crud_api/state.master")["id"].clone();
    recipients(&s, &[("alice", &alice), ("carol", &carol)], "");
    let r = run(&s, Some(&alice), &[], &["plan"]).success();
    assert!(
        r.stderr.contains(
            "crud_api: the next apply seals it to carol; no longer to bob (dform.toml's \
             [secrets]); sealing to a recipient no longer revokes what it opened before"
        ),
        "{}",
        r.stderr
    );
    // Bob, still in the record, may not reseal what he is leaving: only
    // a run that holds the master does, and his does until then.
    run(&s, Some(&alice), &[], &["apply"]).success();
    let record = s.json("dform.state/crud_api/state.master");
    assert_eq!(record["id"], id, "the same master: {record}");
    let mut want = vec![alice.recipient.clone(), carol.recipient.clone()];
    want.sort();
    assert_eq!(record["age"]["recipients"], serde_json::json!(want));
    let e = entries(&s, "recipients");
    let last = e.last().unwrap();
    assert_eq!(last["removed"][0]["key"], bob.recipient.as_str(), "{e:?}");
    assert_eq!(last["added"][0]["name"], "carol", "{e:?}");
    up_to_date(&run(&s, Some(&carol), &[], &["plan"]).success());
    let r = run(&s, Some(&bob), &[], &["plan"]).success();
    assert!(
        r.stderr.contains("planned without its master"),
        "{}",
        r.stderr
    );
    up_to_date(&run(&s, Some(&alice), &[], &["plan"]).success());

    let r = run(&s, Some(&alice), &[], &["secrets", "list", "crud_api"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.starts_with("master epoch 1 (current, id ")
                && l.ends_with("): opens with alice, carol")),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  could also be opened by bob (removed "),
        "{}",
        r.stdout
    );
    // A new epoch began after Bob left: he could not open it.
    run(&s, Some(&alice), &[], &["secrets", "cycle", "crud_api"]).success();
    let r = run(&s, Some(&carol), &[], &["secrets", "list", "crud_api"]).success();
    let lines: Vec<&str> = r.stdout.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("master epoch 2 (current"))
        .unwrap_or_else(|| panic!("{}", r.stdout));
    assert!(
        !lines.get(at + 1).is_some_and(|l| l.contains("bob")),
        "{}",
        r.stdout
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("master epoch 1 (earlier")),
        "{}",
        r.stdout
    );
}

/// The passphrase and recipients together: CI opens it with the
/// passphrase, a member with their identity; adding the passphrase to a
/// team's dform.toml seals it under it at the next apply of someone who
/// has it.
#[test]
fn a_passphrase_beside_recipients() {
    let alice = member();
    let pass = ("DFORM_TEST_PASSPHRASE", "correct horse battery staple");
    let s = team("recipients-ci", &[("alice", &alice)], "");
    run(&s, Some(&alice), &[], &["apply"]).success();
    recipients(
        &s,
        &[("alice", &alice)],
        "passphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
    );
    // Alice has no passphrase: the plan says it is to be added, and her
    // apply cannot add it.
    let r = run(&s, Some(&alice), &[], &["plan"]).success();
    assert!(
        r.stderr
            .contains("the next apply seals it under the passphrase (a run that has it)"),
        "{}",
        r.stderr
    );
    // CI has the passphrase and no identity: not sealed under it yet.
    let r = run(&s, None, &[pass], &["plan"]).success();
    assert!(
        r.stderr
            .contains("it is not sealed under the passphrase yet"),
        "{}",
        r.stderr
    );
    // Alice with the passphrase seals it.
    run(&s, Some(&alice), &[pass], &["apply"]).success();
    assert!(s.json("dform.state/crud_api/state.master")["passphrase"].is_object());
    up_to_date(&run(&s, None, &[pass], &["plan"]).success());
    up_to_date(&run(&s, Some(&alice), &[], &["plan"]).success());
}
