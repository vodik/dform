//! `dform destroy TARGET` (R-149): the plan against an empty wanted set.
//! Every object state holds is a delete, dependents first (a provider
//! configured from the world included: the cluster's objects before the
//! server their kubeconfig is read from), none with a reason; a
//! `prevent_destroy` is a refusal; it asks as apply asks; a stop resumes.
//! Afterwards the deployment's state is empty and `stack list` omits it;
//! its audit log stays. `plan --destroy` prints the plan.

mod common;
use common::{Run, Scratch};
use expectrl::{Eof, Expect, Session};

/// The k3s shape on the mock: a server, a provider configured from it,
/// the cluster's object; a role and a policy, and (by a second apply) an
/// attachment that references both by name.
const PROG: &str = r#"
use fake { source = "prov" }
resource db.postgres server { name = "server" }
use k8s { kubeconfig = format("kc@%s", server.endpoint) }
resource k8s.namespace ns { metadata.name = "app" }
resource iam.role app_role { name = "app", assume = { principals: ["x"] } }
resource iam.policy app_policy { name = "p", document = "{}" }
"#;

const ATTACH: &str =
    "resource iam.role_policy_attachment attach { role = app_role, policy = app_policy }\n";

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> std::process::Command {
    let mut c = common::dform();
    c.args(args).env("NO_COLOR", "1").current_dir(&s.dir);
    c
}

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    Run::from(dform(s, &all).output().unwrap())
}

/// Two applies: the attachment's references are to objects the first made.
fn applied(name: &str) -> Scratch {
    let s = scratch(name);
    dev(&s, &["apply", "--yes"]).success();
    s.write("p.df", &format!("{PROG}{ATTACH}"));
    dev(&s, &["apply", "--yes"]).success();
    s
}

fn objects(s: &Scratch) -> Vec<String> {
    s.json("w.json")["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

const ORDER: [&str; 5] = [
    "  - iam.role_policy_attachment attach",
    "  - iam.policy app_policy",
    "  - iam.role app_role",
    "  - k8s.namespace ns",
    "  - db.postgres server",
];

#[test]
fn the_plan_deletes_every_object_dependents_first() {
    let s = applied("destroy-plan");
    let before = objects(&s);
    let r = dev(&s, &["plan", "--destroy"]).success();
    assert_eq!(
        r.summary(),
        "plan: 5 changes (5 delete) over 1 tick",
        "{}",
        r.stdout
    );
    let order: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("  - ")).collect();
    assert_eq!(order, ORDER, "{}", r.stdout);
    // The operation is every delete's reason: none says one.
    assert!(!r.stdout.contains("no rule wants it"), "{}", r.stdout);
    // A preview: nothing changed.
    assert_eq!(objects(&s), before);
    let j = dev(&s, &["plan", "--destroy", "--json"]).success();
    assert!(!j.stdout.contains("\"reason\""), "{}", j.stdout);
}

#[test]
fn destroy_deletes_in_that_order_and_leaves_an_empty_state() {
    let s = applied("destroy-apply");
    let r = dev(&s, &["destroy", "--yes"]).success();
    assert!(r.stdout.ends_with("destroy: complete\n"), "{}", r.stdout);
    assert!(objects(&s).is_empty(), "{}", s.read("w.json"));
    // The calls in the order they were made.
    let calls: Vec<String> = s
        .read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "action" && e["action"] == "delete")
        .map(|e| e["address"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        calls,
        [
            "iam.role_policy_attachment[\"attach\"]",
            "iam.policy[\"app_policy\"]",
            "iam.role[\"app_role\"]",
            "k8s.namespace[\"ns\"]",
            "db.postgres[\"server\"]",
        ]
    );
    let st = s.json("w.state.json");
    assert_eq!(st["resources"], serde_json::json!({}), "{st}");
    // Nothing is left to destroy.
    let again = dev(&s, &["destroy", "--yes"]).success();
    assert!(
        again.stdout.ends_with("destroy: nothing to do\n"),
        "{}",
        again.stdout
    );
}

#[test]
fn prevent_destroy_refuses_naming_the_resource() {
    let s = applied("destroy-prevent");
    s.write(
        "p.df",
        &format!("{PROG}{ATTACH}lifecycle(server, \"prevent_destroy\")\n"),
    );
    let before = objects(&s);
    let r = dev(&s, &["destroy", "--yes"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains(
            "denied\n  lifecycle prevent_destroy: the plan would delete db.postgres[\"server\"]"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("destroy: refused  1 deny"),
        "{}",
        r.stdout
    );
    assert_eq!(objects(&s), before);
    let p = dev(&s, &["plan", "--destroy"]);
    assert_eq!(p.code, Some(4), "{}\n{}", p.stdout, p.stderr);
}

#[test]
fn a_stopped_destroy_resumes() {
    let s = applied("destroy-resume");
    let mut args = vec!["dev", "--world", "w.json", "--chaos", "stop-after=2"];
    args.extend(["destroy", "--yes", "p.df"]);
    Run::from(dform(&s, &args).output().unwrap()).failure();
    // The checkpoint and the log after it (R-146).
    let held = common::replayed(&s, "w.state.json")["resources"]
        .as_object()
        .unwrap()
        .len();
    assert_eq!(held, 3, "{}", s.read("w.state.json"));
    let r = dev(&s, &["destroy", "--yes"]).success();
    assert!(
        r.stdout.contains("tick 1  3 remaining, resumed\n"),
        "{}",
        r.stdout
    );
    let order: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("  - ")).collect();
    assert_eq!(order[..3], ORDER[2..], "{}", r.stdout);
    assert!(objects(&s).is_empty());
}

/// On a terminal it asks before deleting anything; `n` keeps everything
/// (exit 3, R-147).
#[test]
fn destroy_asks_first_and_a_no_keeps_everything() {
    let s = applied("destroy-asks");
    let before = objects(&s);
    let cmd = dform(&s, &["dev", "--world", "w.json", "destroy", "p.df"]);
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(std::time::Duration::from_secs(60)));
    let said = p.expect("[y/N] ").unwrap();
    let said = String::from_utf8_lossy(said.before()).replace('\r', "");
    assert!(said.ends_with("Destroy these 5 objects of p? "), "{said}");
    p.send_line("n").unwrap();
    p.expect(Eof).unwrap();
    match p.get_process().wait().unwrap() {
        expectrl::process::unix::WaitStatus::Exited(_, code) => assert_eq!(code, 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(objects(&s), before);
}

const APP: &str = r#"
key env: enum("staging", "prod") = "staging"
use fake
resource net.vpc main { cidr = "10.0.0.0/16", tags = { env } }
"#;

/// After a destroy, `stack list` no longer shows the deployment; its log
/// stays, and says so; an apply brings it back.
#[test]
fn a_destroyed_deployment_leaves_stack_list_and_keeps_its_log() {
    let s = Scratch::project("destroy-list");
    s.write("stacks/app.df", APP);
    s.run(&["apply", "app", "env=prod"]).success();
    s.run(&["apply", "app", "env=staging"]).success();
    s.run(&["destroy", "--yes", "app", "env=prod"]).success();
    let r = s.run(&["stack", "list"]).success();
    assert!(!r.stdout.contains("app[env=prod]"), "{}", r.stdout);
    assert!(r.stdout.contains("app[env=staging]"), "{}", r.stdout);
    let log = s.run(&["log", "app", "env=prod"]).success();
    assert!(log.stdout.contains("destroyed"), "{}", log.stdout);
    assert!(log.stdout.contains("apply_start"), "{}", log.stdout);
    s.run(&["apply", "app", "env=prod"]).success();
    let r = s.run(&["stack", "list"]).success();
    assert!(r.stdout.contains("app[env=prod]"), "{}", r.stdout);
}

/// A deny over the program's resources has nothing to bind in a destroy,
/// which wants none of them: it refuses neither the destroy nor its plan
/// (the user's `deny "image not pinned" .. where container(w, c), ..`).
#[test]
fn a_deny_over_the_programs_resources_does_not_refuse_a_destroy() {
    let s = applied("destroy-resource-deny");
    s.write(
        "p.df",
        &format!(
            "{PROG}{ATTACH}deny \"a role named app\" where r in iam.role, r.name == \"app\"\n"
        ),
    );
    // The apply is refused by it, as ever.
    let r = dev(&s, &["apply", "--yes"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    let p = dev(&s, &["plan", "--destroy"]).success();
    assert!(!p.stderr.contains("a role named app"), "{}", p.stderr);
    let r = dev(&s, &["destroy", "--yes"]).success();
    assert!(r.stdout.ends_with("destroy: complete\n"), "{}", r.stdout);
    assert!(!r.stderr.contains("a role named app"), "{}", r.stderr);
    assert!(objects(&s).is_empty(), "{}", s.read("w.json"));
}

/// A deny over the plan's changes binds to a destroy's deletes and refuses
/// it as it would an apply that removed them (the user: "if i have a deny
/// that prevents prod infra from being deleted without approval, destroy
/// shouldn't bypass that"); the refusal says the deny's message, never
/// its context as JSON.
#[test]
fn a_deny_over_deletes_refuses_a_destroy() {
    let s = applied("destroy-delete-deny");
    s.write(
        "p.df",
        &format!(
            "{PROG}{ATTACH}deny \"the database stays: ${{r}}\" where deformation(\"delete\", r, _), r in db.postgres\n"
        ),
    );
    let before = objects(&s);
    let r = dev(&s, &["destroy", "--yes"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stderr
            .contains("constraint violations:\n- the database stays: db.postgres[\"server\"]\n"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("ctx="), "{}", r.stderr);
    assert!(
        r.stderr.contains("destroy: refused  1 deny"),
        "{}",
        r.stderr
    );
    assert_eq!(objects(&s), before);
    let p = dev(&s, &["plan", "--destroy"]);
    assert_eq!(p.code, Some(4), "{}\n{}", p.stdout, p.stderr);
}

/// A destroy that cannot configure a provider (the server its kubeconfig
/// is read from already gone) deletes what the other providers hold,
/// lists the rest with why, and stops (exit 5) with them in state.
#[test]
fn a_destroy_deletes_what_it_can_reach_and_stops_on_the_rest() {
    let s = scratch("destroy-unreachable");
    dev(&s, &["apply", "--yes"]).success();
    // The server goes behind dform's back: no kubeconfig can be made.
    let mut cloud = s.json("w.fakecloud.json");
    cloud["resources"]
        .as_object_mut()
        .unwrap()
        .remove("db.postgres::server")
        .unwrap();
    s.write("w.fakecloud.json", &cloud.to_string());
    let unreachable = "\nunreachable  stay in state\n  k8s.namespace ns\n      its provider is \
                       not configured: provider k8s  kubeconfig = format(\"kc@%s\", \
                       server.endpoint)\n";
    let p = dev(&s, &["plan", "--destroy"]).success();
    assert!(p.stdout.contains(unreachable), "{}", p.stdout);
    let r = dev(&s, &["destroy", "--yes"]);
    assert_eq!(r.code, Some(5), "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains(unreachable), "{}", r.stdout);
    assert!(
        r.stderr
            .contains("destroy p: stopped; 1 object no Delete could reach stay in state"),
        "{}",
        r.stderr
    );
    let st = s.json("w.state.json");
    let held: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    assert_eq!(held, ["k8s.namespace::ns"], "{st}");
    assert!(!s.read("w.state.audit.jsonl").contains("\"destroyed\""));
}

/// dform.toml approvals gate a destroy as an apply: a policy that holds a
/// delete in prod for an approval refuses a plain destroy, naming the
/// digest `plan --destroy` prints; a signed approval of it lets it run.
#[test]
fn an_approval_lets_a_held_destroy_through() {
    let s = Scratch::project("destroy-approval");
    s.write(
        "dform.toml",
        include_str!("../examples/approvals/dform.toml"),
    );
    s.write(
        "stacks/approvals.df",
        &format!(
            "{}\nrequires_approval(r, \"a delete in prod\") where env == \"prod\", \
             deformation(\"delete\", r, _)\n",
            include_str!("../examples/approvals/stacks/approvals.df")
        ),
    );
    let signer = |args: &[&str]| {
        let out = std::process::Command::new(common::exe("dform-approve"))
            .args(args)
            .current_dir(&s.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    s.write("approvers.jwks.json", &signer(&["keygen", "approver.key"]));
    s.run(&["apply", "approvals", "env=prod"]).success();
    let p = s
        .run(&["plan", "--destroy", "approvals", "env=prod"])
        .success();
    let digest = p
        .stdout
        .split_once("plan digest: ")
        .and_then(|(_, d)| d.lines().next())
        .unwrap_or_else(|| panic!("{}", p.stdout))
        .to_string();
    let r = s
        .run(&["destroy", "--yes", "approvals", "env=prod"])
        .failure();
    assert!(
        r.stderr.contains(&format!(
            "destroy refused: net.vpc[\"main\"] (a delete in prod) needs an approval, and no \
             --approval was given; the plan's digest is {digest}: have that digest approved \
             (`plan --destroy` prints it) and `destroy --approval FILE`"
        )),
        "{}",
        r.stderr
    );
    let token = signer(&[
        "sign",
        "approver.key",
        "--digest",
        &digest,
        "--stack",
        "approvals",
        "--key",
        "env=prod",
        "--approver",
        "alice",
    ]);
    s.write("approval.json", &token);
    let r = s
        .run(&[
            "destroy",
            "--yes",
            "--approval",
            "approval.json",
            "approvals",
            "env=prod",
        ])
        .success();
    assert!(r.stdout.contains("approved by alice"), "{}", r.stdout);
    assert!(r.stdout.ends_with("destroy: complete\n"), "{}", r.stdout);
}

/// A destroy killed mid-tick: `state show` says the next destroy, not the
/// next apply, resumes it.
#[test]
fn an_interrupted_destroy_says_the_next_destroy_resumes_it() {
    let s = Scratch::project("destroy-killed");
    s.write("stacks/app.df", APP);
    s.run(&["apply", "app"]).success();
    let out = common::dform()
        .args(["destroy", "--yes", "app"])
        .current_dir(&s.dir)
        .env("DFORM_TEST_ABORT_AT", "logged:2")
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(9), "{out:?}");
    let r = s.run(&["state", "show", "app"]).success();
    assert!(
        r.stdout
            .contains("a destroy was interrupted: the next destroy resumes it"),
        "{}",
        r.stdout
    );
}

/// An unreachable object the program retains is forgotten (R-154): the
/// destroy completes, the cluster's object left where it is.
#[test]
fn a_retained_unreachable_object_is_forgotten() {
    let s = scratch("destroy-unreachable-retained");
    dev(&s, &["apply", "--yes"]).success();
    let mut cloud = s.json("w.fakecloud.json");
    cloud["resources"]
        .as_object_mut()
        .unwrap()
        .remove("db.postgres::server")
        .unwrap();
    s.write("w.fakecloud.json", &cloud.to_string());
    s.write("p.df", &format!("{PROG}lifecycle(ns, \"retain\")\n"));
    let r = dev(&s, &["destroy", "--yes"]).success();
    assert!(
        r.stdout
            .contains("  ~ k8s.namespace ns  forgotten, kept in the world  (lifecycle retain)"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("unreachable"), "{}", r.stdout);
    assert!(r.stdout.ends_with("destroy: complete\n"), "{}", r.stdout);
    assert_eq!(objects(&s), ["k8s.namespace::ns"]);
}
