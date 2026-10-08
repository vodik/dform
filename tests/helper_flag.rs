//! The relations the compiler writes beside a program's (a `not { .. }`
//! body, an aggregate's fold) are known by what they are, not by their
//! names (R-212), so they behave the same in a `use`d module, where the
//! relation is the module's (`policy::__neg_0`), and at a let with
//! parameters' site, where it is the site's copy: a `not { }` is matched
//! as it is, nulls and all (R-193); one whose body reads what waits waits
//! too; and none is reported stuck in place of the rule it was written
//! for. On the mock.

mod common;
use common::{Run, Scratch};

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource db.postgres server { name = "server" }
output kubeconfig = server.endpoint
"#;

/// The apps stack: two Deployments, `web` runs as root, `api` does not.
const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s { kubeconfig = platform[env].kubeconfig }
resource k8s.namespace apps { metadata.name = "apps" }
resource k8s.deployment web {
  metadata = { name: "web", namespace: apps.metadata.name }
  spec.selector.matchLabels = { app: "web" }
  spec.template.spec.containers = [{ name: "web", image: "nginx" }]
}
resource k8s.deployment api {
  metadata = { name: "api", namespace: apps.metadata.name }
  spec.selector.matchLabels = { app: "api" }
  spec.template.spec = {
    securityContext: { runAsNonRoot: true },
    containers: [{ name: "api", image: "api" }],
  }
}
RULES
"#;

fn plan(name: &str, rules: &str, files: &[(&str, &str)]) -> Run {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", &APPS.replace("RULES", rules));
    for (path, src) in files {
        s.write(path, src);
    }
    s.run(&["plan", "apps"])
}

/// The deny holds for `web` and not for `api`, decided before the
/// platform is applied: not under `later`, not undetermined.
fn decided(r: Run) {
    let r = r.failure();
    let all = format!("{}{}", r.stdout, r.stderr);
    assert!(
        all.contains("- pod may run as root  workload = \"web\"\n")
            && !all.contains("workload = \"api\"")
            && !r.stdout.contains("undetermined")
            && !r.stdout.contains("deny \"pod may run as root\""),
        "{all}"
    );
}

/// A policy pack's deny, in a module the stack uses: its `not { }` is
/// the module's helper and reads only what the program wrote.
#[test]
fn a_deny_in_a_used_module_is_decided() {
    decided(plan(
        "helper-module",
        "use policy\n",
        &[(
            "policy.df",
            "deny \"pod may run as root\" { workload: w } where {\n  w in k8s.deployment\n  \
             p = w.spec.template.spec\n  not p.securityContext.runAsNonRoot == true\n}\n",
        )],
    ));
}

/// The same `not { }` in a let with parameters, called by the deny: the
/// site's copy of the helper reads only what the program wrote.
#[test]
fn a_deny_through_a_let_with_parameters_is_decided() {
    decided(plan(
        "helper-let-site",
        "let root(p) = true where not { p.securityContext.runAsNonRoot == true }\n\
         deny \"pod may run as root\" { workload: w } where {\n  w in k8s.deployment\n  \
         root(w.spec.template.spec) == true\n}\n",
        &[],
    ));
}

/// A box whose computed `status` is not known until it is created.
const BOX: &str = r#"
type_provider(x.box, "boxcloud")
type_attr(x.box, "id", "string", ["computed", "id"])
type_attr(x.box, "status", "object", ["computed"])
"#;

/// The policy's denies over `x.box`: `not has` of a field of the computed
/// object, a `not { }` over it, a count, and a `not { }` of a relation
/// that waits on it.
const BOX_POLICY: &str = r#"
deny "not ready" { box: r } where r in x.box, not has r.status.ready
deny "odd" { box: r } where r in x.box, not { r.status.phase == "x" }
ready(r) where r in x.box, r.status.ready == true
deny "few" { n: n } where n = count(r), ready(r), n < 1
deny "none ready" { box: r } where r in x.box, not { ready(r) }
"#;

/// `p.df` holding `src`, beside the box's schema and `policy.df`.
fn boxes(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("s.df", BOX);
    s.write("policy.df", BOX_POLICY);
    s.write("p.df", src);
    s
}

fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json", "--provider", "s.df"];
    all.extend_from_slice(args);
    all.push("p.df");
    s.run(&all)
}

/// A `not { }` whose body reads a relation that waits on a value not
/// known yet waits with it, at the top and in a module alike: `ready(b)`
/// may hold once `b.status` is known, so "none ready" is undetermined, not
/// a violation: the plan is no refusal and `why` says what it waits on.
#[test]
fn a_negation_of_what_waits_waits() {
    let top = format!("resource x.box b {{ size = 1 }}\n{BOX_POLICY}");
    for (name, src) in [
        ("helper-waits-top", top.as_str()),
        (
            "helper-waits-module",
            "use policy\nresource x.box b { size = 1 }\n",
        ),
    ] {
        let s = boxes(name, src);
        dev(&s, &["plan"]).success();
        let r = dev(&s, &["why", "deny \"none ready\""]).success();
        assert!(
            r.stdout
                .starts_with("deny \"none ready\": undetermined, waits on b.status\n"),
            "{name}\n{}",
            r.stdout
        );
    }
}

/// `stuck/4` names the program's rules, never a helper of a module: the
/// denies and `ready` that wait, not `policy::__neg_0` or
/// `policy::__agg_0`; as the evaluation ends, and where a rule reads it.
#[test]
fn a_modules_helper_is_never_stuck_in_its_rules_place() {
    let s = boxes(
        "helper-stuck",
        "use policy\nresource x.box b { size = 1 }\nwaiting(h) where stuck(_, h, _, _)\n",
    );
    for relation in ["stuck", "waiting"] {
        let r = dev(&s, &["query", relation]).success();
        assert!(
            r.stdout
                .contains("deny(\\\"none ready\\\", {box: \\\"b\\\"})")
                && r.stdout.contains("policy::ready(")
                && !r.stdout.contains("__"),
            "{relation}\n{}",
            r.stdout
        );
    }
}

/// Not reached yet: a component copy's clause is its gate,
/// `g::__instance(..)`, a relation the compiler names but the program's
/// clause, so it is no helper and `stuck/4` names it by the compiler's
/// word. How it is spelled is R-211's (its origin), not a flag's.
#[test]
#[ignore = "a copy's gate is spelled by the compiler's name in stuck/4 (R-211)"]
fn a_copys_gate_is_stuck_in_the_programs_words() {
    let s = boxes(
        "helper-gate",
        "component pair {\n  input n: int\n  resource x.box inner { size = n }\n}\n\
         resource x.box b { size = 1 }\n\
         resource pair g { n = 2 } where b.status.ready == true\n",
    );
    let r = dev(&s, &["query", "stuck"]).success();
    assert!(!r.stdout.contains("__instance"), "{}", r.stdout);
}
