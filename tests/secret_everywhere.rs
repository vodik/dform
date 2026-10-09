//! A sensitive value is `(sensitive)` in every printer at every level
//! (R-215): the plan (`-q`, the default, `-v`, `-vv`), `plan --json`, the
//! plan file, `why` (each level, `--tree`, `--json`), `query`, `dev
//! show`, the apply's output, its audit log and its state. Each test
//! drives every printer over one program and looks for the bytes.
//!
//! Two ways a value is sensitive that taint does not see, the program
//! writing a literal: a path below the attribute the program sets
//! (`spec.value` of `spec`), and a kind no schema has until its provider
//! learns it at a tick's boundary, where no path is known not to be.

mod common;
use common::{Run, Scratch};

/// What every printer of `program` in `s` says, the run's flags
/// (`--world w.json`, a provider) before each: the printers that read
/// the program and the plan, at each level.
fn printers(s: &Scratch, flags: &[&str], subject: &str, resource: &str) -> Vec<(String, Run)> {
    let (typ, name) = resource.rsplit_once(' ').unwrap();
    let address = format!("{typ}[\"{name}\"]");
    let runs: [&[&str]; 16] = [
        &["plan", "-q"],
        &["plan"],
        &["plan", "-v"],
        &["plan", "-vv"],
        &["plan", "--json"],
        &["plan", "-vv", "--json"],
        &["why", subject],
        &["why", "-v", subject],
        &["why", "-vv", subject],
        &["why", "--tree", subject],
        &["why", "--json", subject],
        &["why", resource],
        &["query", "attr(T, A, P, V)"],
        &["query", "--json", "arg(T, A, P, V)"],
        &["show", &address],
        &["plan", "--out", "plan.json"],
    ];
    runs.iter()
        .map(|args| {
            let mut all = vec!["dev"];
            all.extend_from_slice(flags);
            all.extend_from_slice(args);
            all.push("p.df");
            (all.join(" "), run(s, &all))
        })
        .collect()
}

fn run(s: &Scratch, args: &[&str]) -> Run {
    Run::from(
        common::dform()
            .args(args)
            .env("R45_KUBECONFIG", "kc")
            .env("DFORM_WAIT_POLL_MS", "50")
            .env("NO_COLOR", "1")
            .current_dir(&s.dir)
            .output()
            .unwrap(),
    )
}

/// Neither stream of any run says `secret`, and each run succeeded.
fn never_says(runs: &[(String, Run)], secret: &str) {
    for (cmd, r) in runs {
        assert!(r.ok, "{cmd}:\n{}{}", r.stdout, r.stderr);
        assert!(
            !r.stdout.contains(secret) && !r.stderr.contains(secret),
            "{cmd} printed {secret}:\n{}{}",
            r.stdout,
            r.stderr
        );
    }
}

/// No file under the scratch directory but the program and the mock
/// provider's world (`w.json`, the cloud's own storage) holds `secret`:
/// the plan file, the state, its audit log.
fn never_stored(s: &Scratch, secret: &str) {
    for e in std::fs::read_dir(&s.dir).unwrap() {
        let p = e.unwrap().path();
        let source = p.extension().is_some_and(|e| e == "df");
        if source || p.file_name().is_some_and(|n| n == "w.json") || !p.is_file() {
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
        assert!(
            !text.contains(secret),
            "{} holds {secret}:\n{text}",
            p.display()
        );
    }
}

const TOKEN: &str = "TOKEN-VALUE";

/// A kind the provider serves only once the program configures it (the
/// mock's `schemas` setting plays the cluster's CRD) is planned in tick 2
/// before any schema says which of its paths are sensitive: each is said
/// `(sensitive)` until the boundary learns the schema, and the tick
/// re-planned there is the plan shown (the digests agree), so nothing is
/// asked again. Before the fix, every printer showed `spec.value =
/// "TOKEN-VALUE"`.
#[test]
fn a_kind_learned_at_the_boundary_never_prints_its_values() {
    let s = Scratch::new("everywhere-learned");
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s.write(
        "crd.df",
        "type_provider(k8s.example.io.v1.token, \"k8s\")\n\
         type_attr(k8s.example.io.v1.token, \"metadata.name\", \"string\", [\"id\"])\n\
         type_attr(k8s.example.io.v1.token, \"metadata.namespace\", \"string\", [])\n\
         type_attr(k8s.example.io.v1.token, \"metadata.uid\", \"string\", [\"computed\", \"id\"])\n\
         type_mint(k8s.example.io.v1.token, \"metadata.uid\", \"uid-{name}\")\n\
         type_attr(k8s.example.io.v1.token, \"spec.value\", \"string\", [\"sensitive\"])\n",
    );
    s.write(
        "p.df",
        r#"
use env
use fake { source = "prov" }
resource db.postgres server { name = "server" }
let kc = str.format("%s@%s", env.var("R45_KUBECONFIG"), server.endpoint)
use k8s { kubeconfig = kc, schemas = ["crd.df"] }
resource k8s.namespace ns { metadata.name = "app" }
resource k8s.example.io.v1.token t {
  metadata.name = "t"
  metadata.namespace = ns.metadata.name
  spec.value = "TOKEN-VALUE"
}
"#,
    );
    let flags = ["--world", "w.json"];
    let before = printers(&s, &flags, "t.spec.value", "k8s.example.io.v1.token t");
    never_says(&before, TOKEN);
    // A string equal to one of its values elsewhere prints as it is.
    let plan = &before[1].1.stdout;
    assert!(
        plan.contains("      spec.value = (sensitive)\n")
            && plan.contains("  + k8s.namespace ns  ")
            && plan.contains("      metadata.name = \"app\"\n"),
        "{plan}"
    );
    let applied = run(
        &s,
        &[
            "dev",
            "--world",
            "w.json",
            "apply",
            "--yes",
            "-vv",
            "plan.json",
        ],
    );
    assert!(
        !applied.stdout.contains("differs from the plan shown"),
        "{}",
        applied.stdout
    );
    never_says(&[("apply plan.json".into(), applied)], TOKEN);
    // Its schema learned: `spec.value` is sensitive by it.
    never_says(
        &printers(&s, &flags, "t.spec.value", "k8s.example.io.v1.token t"),
        TOKEN,
    );
    never_stored(&s, TOKEN);
}

const NESTED: &str = "NESTED-SECRET";

/// A path the schema marks sensitive below the attribute the program sets
/// (`spec.value`, the program writing `spec`): its value is `(sensitive)`
/// where the object prints whole (`query`, `why --tree`) as where its leaf
/// does. Before the fix, `query` and `why` printed it.
#[test]
fn a_sensitive_path_below_an_attribute_never_prints() {
    let s = Scratch::new("everywhere-nested");
    s.write(
        "s.df",
        "type_provider(nest.token, \"fakecloud\")\n\
         type_attr(nest.token, \"id\", \"string\", [\"computed\", \"id\"])\n\
         type_attr(nest.token, \"spec.value\", \"string\", [\"sensitive\"])\n\
         type_attr(nest.token, \"spec.other\", \"string\", [])\n",
    );
    s.write(
        "p.df",
        "use fake\nresource nest.token n {\n  spec.value = \"NESTED-SECRET\"\n  spec.other = \"x\"\n}\n",
    );
    let flags = ["--provider", "s.df", "--world", "w.json"];
    never_says(
        &printers(&s, &flags, "n.spec.value", "nest.token n"),
        NESTED,
    );
    let applied = run(
        &s,
        &[
            "dev",
            "--provider",
            "s.df",
            "--world",
            "w.json",
            "apply",
            "--yes",
            "-vv",
            "plan.json",
        ],
    );
    never_says(&[("apply plan.json".into(), applied)], NESTED);
    never_says(
        &printers(&s, &flags, "n.spec.value", "nest.token n"),
        NESTED,
    );
    never_stored(&s, NESTED);
}
