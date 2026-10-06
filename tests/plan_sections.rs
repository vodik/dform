//! The plan printer's sections (E §7.4, F DR-2 revised, DR-9 revised,
//! R-79): set-aware diffs, the ticks and what each waits on, `later`,
//! shadowed and conflicts, the summary line and what apply does.

mod common;
use common::{Scratch, repo};

/// The two-phase GKE stack, with the rules of the `extra` files after it
/// (the stack's copy, `p.df`, in the scratch directory).
fn gke(s: &Scratch, extra: &[&str], cmd: &str) -> common::Run {
    let prog = repo().join("examples/gke/stacks/gke_two_phase.df");
    let mut prog = prog.to_str().unwrap().to_string();
    if !extra.is_empty() {
        let mut text = std::fs::read_to_string(&prog).unwrap();
        for e in extra {
            text.push_str(&s.read(e).replace("\n", ""));
        }
        s.write("p.df", &text);
        prog = "p.df".into();
    }
    s.run(&common::on(
        &prog,
        &["--provider", "gke", "--world", "w.json"],
        &[cmd],
    ))
}

#[test]
fn gke_plan_has_the_summary_ticks_and_later() {
    let s = Scratch::new("sections-gke");
    let r = gke(&s, &[], "plan").success();
    let first = r.stdout.lines().next().unwrap();
    assert_eq!(
        first,
        "plan: 6 changes (6 create) over 2 ticks, 1 undetermined"
    );
    for want in [
        "\ntick 1  3 changes, applies now\n  + google.compute_subnetwork gke_subnet  ",
        "\ntick 2  3 changes, after tick 1 reports\n  waits on  pngu.ca_certificate\n            pngu.endpoint\n  + k8s.deployment api  ",
        "\nlater   changes this plan cannot count yet\n  google.container_node_pool \"np-${z}\"  ",
        "  waits on pngu.zones\n",
        "  deny \"cluster must be in at least two zones\"  ",
        "  undetermined until tick 2\n",
        "\napply: tick 1 now, then tick 2 when tick 1 reports; `later` is planned again when tick 1 reports, and apply asks before what it adds\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("to create"), "{}", r.stdout);
}

/// F DR-2 revised, last clause: a deny whose body positively reads a
/// predicate with a stuck instance is not undetermined, but may derive
/// after the boundary; the plan says so.
#[test]
fn a_deny_reading_a_stuck_predicate_may_derive_after_the_tick() {
    let s = Scratch::new("sections-may-derive");
    s.write(
        "extra.df",
        r#"
deny "no nodepool in zone z" {pool: n} where n in google.container_node_pool, arg(google.container_node_pool, n, "zone", "us-east1-z")
"#,
    );
    let r = gke(&s, &["extra.df"], "plan").success();
    assert!(
        r.stdout.contains(
            "  deny \"no nodepool in zone z\"                      p.df:112  may hold at tick 2\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.summary().ends_with(", 2 undetermined"), "{}", r.stdout);
}

const AWS: &str = "examples/aws/stacks/aws_demo.df";

fn aws(s: &Scratch, cmd: &str) -> common::Run {
    let prog = repo().join(AWS);
    s.run(&common::on(
        prog.to_str().unwrap(),
        &["--provider", "aws-mock", "--world", "w.json"],
        &[cmd],
    ))
}

/// A keyless set diffs by element: one rule opened by hand is one element
/// removed, not every later index shifting.
#[test]
fn a_keyless_set_diffs_by_element() {
    let s = Scratch::new("sections-set");
    aws(&s, "apply").success();
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]["aws.security_group::web"]["attrs"]["ingress"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            serde_json::json!({"from_port": 22, "to_port": 22, "protocol": "tcp", "cidr_blocks": ["0.0.0.0/0"]}),
        );
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = aws(&s, "plan").success();
    assert!(
        r.stdout.contains(
            "  ~ aws.security_group web  stacks/aws_demo.df:24\n      - ingress[]\n          cidr_blocks[0] was \"0.0.0.0/0\"\n          from_port was 22\n          protocol was \"tcp\"\n          to_port was 22\n\napply: tick 1 now\n"
        ),
        "{}",
        r.stdout
    );
}

/// A list with merge keys diffs by element too: a new container is one
/// added element with its leaves.
#[test]
fn a_keyed_list_diffs_by_element() {
    let s = Scratch::new("sections-keyed");
    let one = r#"

resource k8s.deployment api {
  metadata.name = "api",
  spec.selector.matchLabels = {app: "api"},
  spec.template.spec.containers = [ {name: "app", image: "api:1"} ]
}
use fake
"#;
    s.write("p.df", one);
    s.run(&common::on(
        "p.df",
        &["--provider", "k8s", "--world", "w.json"],
        &["apply"],
    ))
    .success();
    s.write(
        "p.df",
        &one.replace(
            r#"{name: "app", image: "api:1"} ]"#,
            r#"{name: "app", image: "api:1"}, {name: "sidecar", image: "envoy:1"} ]"#,
        ),
    );
    let r = s
        .run(&common::on(
            "p.df",
            &["--provider", "k8s", "--world", "w.json"],
            &["plan"],
        ))
        .success();
    assert!(
        r.stdout.contains(
            "  ~ k8s.deployment api  p.df:3\n      + spec.template.spec.containers[name=sidecar]\n          image = \"envoy:1\"\n          name = \"sidecar\"\n"
        ),
        "{}",
        r.stdout
    );
}

/// DR-9 revised and E §2.8: a shadowed disagreement is a section, and a
/// conflict is a section naming the resource, the path and every witness
/// with where it is written; the conflicted address is not a change, and
/// the plan refuses.
#[test]
fn shadowed_and_conflicts_are_sections() {
    let s = Scratch::new("sections-conflict");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
set main.cidr = "10.1.0.0/16" where ok(1)
resource net.vpc two @default { cidr = "10.0.0.0/16" }
set two.cidr = "10.9.0.0/16" @default where ok(1)
set two.cidr = "10.3.0.0/16" where ok(1)
ok(1)
use fake
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .failure();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick, 1 conflict",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
    for want in [
        "\nshadowed\n  ! net.vpc two.cidr at rank default: two contributions disagree at cidr\n",
        // Each witness by its value and where it was written (R-111).
        "\nconflicts\n  ! net.vpc main.cidr: two contributions disagree\n      \"10.0.0.0/16\"  p.df:3\n      \"10.1.0.0/16\"  p.df:4\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

/// Two conflicts: the section once, each conflict and its witnesses once,
/// and the refusal once (R-111).
#[test]
fn two_conflicts_print_once_each() {
    let s = Scratch::new("sections-two-conflicts");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
set main.cidr = "10.1.0.0/16" where ok(1)
resource net.vpc two { cidr = "10.0.0.0/16" }
set two.cidr = "10.9.0.0/16" where ok(1)
resource net.vpc three { cidr = "10.0.0.0/16" }
ok(1)
use fake
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .failure();
    for want in [
        "conflicts",
        "  ! net.vpc main.cidr: two contributions disagree",
        "  ! net.vpc two.cidr: two contributions disagree",
        "apply: refused until the conflicts and denies above are resolved",
    ] {
        let n = r.stdout.lines().filter(|l| *l == want).count();
        assert_eq!(n, 1, "{want}\n---\n{}", r.stdout);
    }
    let witnesses = r
        .stdout
        .lines()
        .filter(|l| l.starts_with("      \""))
        .count();
    assert_eq!(witnesses, 4, "{}", r.stdout);
}

/// A conflict at a sensitive path prints neither witness's value nor the
/// rule text that spells it.
#[test]
fn a_conflict_at_a_sensitive_path_is_redacted_in_the_plan() {
    let s = Scratch::new("sections-conflict-secret");
    s.write(
        "p.df",
        r#"

resource leaky.vault v { password = "VAULT-SECRET-A" }
set v.password = "VAULT-SECRET-B" where ok(1)
ok(1)
use fake
"#,
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let r = s
        .run(&[
            "dev",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "plan",
            "p.df",
        ])
        .failure();
    assert!(
        r.stdout.contains("\nconflicts\n  ! leaky.vault v.password"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("VAULT-SECRET"), "{}", r.stdout);
}

/// The executor's entries in text: a replace with its marker and a
/// prevent_destroy deny as a section, the plan still printed.
#[test]
fn a_denied_replace_is_a_section() {
    let s = Scratch::new("sections-denied");
    let net = "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n";
    s.write("p.df", net);
    s.run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"prevent_destroy\")\n",
            net.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["plan"]))
        .failure();
    assert_eq!(
        r.stdout,
        "plan: 1 change (1 replace) over 1 tick, 1 denied\n\n\
         tick 1  1 change, applies now\n\
         \x20 ± net.vpc main  p.df:2  cidr is immutable\n\
         \x20     cidr: \"10.0.0.0/16\" → \"10.1.0.0/16\"\n\n\
         denied\n\
         \x20 lifecycle prevent_destroy: the plan would replace net.vpc[\"main\"]  net.vpc main    p.df:4\n\n\
         apply: refused until the conflicts and denies above are resolved\n"
    );
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

/// `--color always` paints the plan by its semantics, as a hint only
/// (R-111): a create's `+` green and its address bold green, a tick's head
/// bold, the site column and `(sensitive)` dim, a rule in `later` in the
/// warning colour; `never`, `NO_COLOR` under `auto`, and `--json` print
/// none, and lose nothing.
/// What `plan` prints uncoloured is the text every golden has.
#[test]
fn color_is_a_rendering_of_the_same_text() {
    let s = Scratch::new("sections-color");
    let plain = gke(&s, &[], "plan").success().stdout;
    assert!(!plain.contains('\x1b'), "{plain}");
    let colored = |flag: &str| {
        s.run(&common::on(
            repo()
                .join("examples/gke/stacks/gke_two_phase.df")
                .to_str()
                .unwrap(),
            &["--provider", "gke", "--world", "w.json"],
            &["plan", "--color", flag],
        ))
        .success()
        .stdout
    };
    let always = colored("always");
    for want in [
        "  \x1b[32m+\x1b[0m \x1b[1;32mgoogle.compute_subnetwork gke_subnet\x1b[0m  ",
        "\x1b[1mtick 1  3 changes, applies now\x1b[0m\n",
        "  \x1b[2mstacks/gke_two_phase.df:38\x1b[0m\n",
        "  waits on  pngu.ca_certificate\n",
        "      data.password = \x1b[2m(sensitive)\x1b[0m\n",
        "  \x1b[33mgoogle.container_node_pool \"np-${z}\"\x1b[0m  ",
    ] {
        assert!(always.contains(want), "{want:?}\n---\n{always:?}");
    }
    let stripped = strip_sgr(&always);
    assert_eq!(stripped, plain);
    assert_eq!(colored("never"), plain);
    // `auto` on a pipe is plain; `--json` is plain under `always`.
    assert_eq!(colored("auto"), plain);
    let json = s
        .run(&common::on(
            repo()
                .join("examples/gke/stacks/gke_two_phase.df")
                .to_str()
                .unwrap(),
            &["--provider", "gke", "--world", "w.json"],
            &["plan", "--json", "--color", "always"],
        ))
        .success()
        .stdout;
    assert!(!json.contains('\x1b'), "{json}");
}

/// `s` without its ANSI SGR sequences.
fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// R-15: an update is explained by the winning contribution to the
/// attribute it changes, a delete by state alone; a secret input in an
/// explanation prints as its label.
#[test]
fn plan_why_explains_an_update_and_a_delete_and_redacts_a_secret() {
    let s = Scratch::new("sections-why");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let mock = ["--provider", schema.to_str().unwrap(), "--world", "w.json"];
    let run = |pw: &str, args: &[&str]| {
        let mut all = vec!["--set".to_string(), format!("pw={pw}")];
        all.extend(common::on("p.df", &mock, args));
        s.run(&all)
    };
    s.write(
        "p.df",
        "\ninput pw: secret(string)\nresource leaky.vault v {\n  password = pw\n}\nresource leaky.oops o {\n  password = \"plain\"\n}\n",
    );
    run("FIRST-SECRET-123", &["apply"]).success();
    s.write(
        "p.df",
        "\ninput pw: secret(string)\nresource leaky.vault v {\n  password = pw\n}\n",
    );
    let r = run("SECOND-SECRET-456", &["plan", "--why"]).success();
    assert!(
        r.stdout.contains(
            "  ~ leaky.vault v                          p.df:3\n      password: (sensitive) → (sensitive)  --set pw=(sensitive \
             input.pw)\n      by p.df:4  resource leaky.vault v { password = pw }\n      because \
             --set pw=(sensitive input.pw)\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  - leaky.oops o\n      password was \"plain\"\n      because no statement \
             derives it now; state has it\n"
        ),
        "{}",
        r.stdout
    );
    let j = run("SECOND-SECRET-456", &["plan", "--why", "--json"]).success();
    for out in [&r.stdout, &r.stderr, &j.stdout] {
        assert!(!out.contains("SECRET-"), "{out}");
    }
}

/// A string with a newline, quotes or a `${` prints as the literal `fmt`
/// writes for it, on one line, the same in the plan and in `query`.
#[test]
fn a_multi_line_string_prints_as_its_literal_in_plan_and_query() {
    let s = Scratch::new("sections-multiline");
    s.write(
        "p.df",
        "\nuse fake\nresource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  \
         note = \"one\\ntwo \\\"three\\\" $${four}\"\n}\n",
    );
    let lit = r#""one\ntwo \"three\" $${four}""#;
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .success();
    assert!(
        r.stdout.contains(&format!("      note = {lit}\n")),
        "{}",
        r.stdout
    );
    let q = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "query",
            "attr(net.vpc, A, \"note\", V)",
            "p.df",
        ])
        .success();
    assert!(
        q.stdout.contains(&format!("\"main\"  {lit}\n")),
        "{}",
        q.stdout
    );
}
