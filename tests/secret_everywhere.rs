//! A sensitive value is `(sensitive)` in every printer at every level
//! (R-215): the plan (`-q`, the default, `-v`, `-vv`), `plan --json`, the
//! plan file, `why` (each level, `--tree`, `--json`), `query`, `dev
//! show`, the apply's output, its audit log and its state. Each test
//! drives every printer over one program and looks for the bytes.
//!
//! A value is sensitive by its path or by what reached it (the taint): a
//! path below the attribute the program sets (`spec.value` of `spec`) is
//! one; a kind no schema has until its provider learns it at a tick's
//! boundary marks no path, so only the taint could, and a secret never
//! reaches it.

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
            .env("TOKEN", TOKEN)
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

/// The mock's `schemas` setting plays a cluster's CRDs: kinds the
/// provider serves only once the program configures it, so a resource of
/// one is planned in tick 2 before any schema says which of its paths are
/// sensitive. `k8s.example.io.v1.token`'s `spec.value` is, once learned.
const CRDS: &str = "type_provider(k8s.example.io.v1.token, \"k8s\")\n\
     type_attr(k8s.example.io.v1.token, \"metadata.name\", \"string\", [\"id\"])\n\
     type_attr(k8s.example.io.v1.token, \"metadata.namespace\", \"string\", [])\n\
     type_attr(k8s.example.io.v1.token, \"metadata.uid\", \"string\", [\"computed\", \"id\"])\n\
     type_mint(k8s.example.io.v1.token, \"metadata.uid\", \"uid-{name}\")\n\
     type_attr(k8s.example.io.v1.token, \"spec.value\", \"string\", [\"sensitive\"])\n\
     type_provider(k8s.traefik.middleware, \"k8s\")\n\
     type_attr(k8s.traefik.middleware, \"metadata.name\", \"string\", [\"id\"])\n\
     type_attr(k8s.traefik.middleware, \"metadata.namespace\", \"string\", [])\n\
     type_attr(k8s.traefik.middleware, \"metadata.uid\", \"string\", [\"computed\", \"id\"])\n\
     type_mint(k8s.traefik.middleware, \"metadata.uid\", \"uid-{name}\")\n\
     type_attr(k8s.traefik.middleware, \"spec\", \"object\", [])\n";

/// A project whose k8s provider is configured from what tick 1 makes,
/// serving [`CRDS`], with `resources` below.
fn learned(name: &str, resources: &str) -> Scratch {
    let s = Scratch::new(name);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s.write("crd.df", CRDS);
    s.write(
        "p.df",
        &format!(
            "use env\n\
             use fake {{ source = \"prov\" }}\n\
             resource db.postgres server {{ name = \"server\" }}\n\
             let kc = str.format(\"kc@%s\", server.endpoint)\n\
             use k8s {{ kubeconfig = kc, schemas = [\"crd.df\"] }}\n\
             resource k8s.namespace apps {{ metadata.name = \"apps\" }}\n\
             {resources}"
        ),
    );
    s
}

/// A secret given to a kind no schema has yet is refused before anything
/// prints it (E0304: no path of the kind is marked sensitive), by every
/// printer: the taint is what masks a value of such a kind, and it never
/// reaches one.
#[test]
fn a_secret_never_reaches_a_kind_no_schema_has() {
    let s = learned(
        "everywhere-learned-secret",
        "resource k8s.example.io.v1.token t {\n\
           metadata.name = \"t\"\n\
           metadata.namespace = apps.metadata.name\n\
           spec.value = env.var(\"TOKEN\")\n\
         }\n",
    );
    let flags = ["--world", "w.json"];
    for (cmd, r) in printers(&s, &flags, "t.spec.value", "k8s.example.io.v1.token t") {
        assert!(
            !r.ok
                && r.stderr.contains(
                    "E0304: a secret reaches k8s.example.io.v1.token .spec.value, not marked \
                     sensitive in the schema"
                )
                && !r.stdout.contains(TOKEN)
                && !r.stderr.contains(TOKEN),
            "{cmd}:\n{}{}",
            r.stdout,
            r.stderr
        );
    }
    never_stored(&s, TOKEN);
}

/// A kind no schema has yet prints what no secret reached, in every
/// printer and in a `warn` that quotes it (the user's two Traefik
/// middlewares printed `metadata = { name: (sensitive), namespace:
/// (sensitive) }` under R-215's every-string rule). A literal at a path
/// its learned schema marks sensitive prints until the boundary and is
/// `(sensitive)` from it on; the tick re-planned there is the plan shown
/// (the digests agree), so nothing is asked again.
#[test]
fn a_kind_no_schema_has_prints_what_no_secret_reached() {
    let s = learned(
        "everywhere-learned",
        "resource k8s.traefik.middleware security_headers {\n\
           metadata = { name: \"security-headers\", namespace: apps.metadata.name }\n\
           spec.headers = {\n\
             stsSeconds: 31536000,\n\
             contentTypeNosniff: true,\n\
             referrerPolicy: \"strict-origin-when-cross-origin\",\n\
           }\n\
         }\n\
         resource k8s.traefik.middleware large_upload {\n\
           metadata = { name: \"large-upload\", namespace: apps.metadata.name }\n\
           spec.buffering.maxRequestBodyBytes = 536870912\n\
         }\n\
         warn \"headers: ${security_headers.spec.headers.referrerPolicy}\"\n\
         resource k8s.example.io.v1.token t {\n\
           metadata.name = \"t\"\n\
           metadata.namespace = apps.metadata.name\n\
           spec.value = \"TOKEN-VALUE\"\n\
         }\n",
    );
    let flags = ["--world", "w.json"];
    let middleware = "k8s.traefik.middleware security_headers";
    let before = printers(&s, &flags, "security_headers.metadata", middleware);
    for (cmd, r) in &before {
        assert!(r.ok, "{cmd}:\n{}{}", r.stdout, r.stderr);
        // `secret(?)` is a value not known yet; none of these is masked.
        for masked in ["(sensitive", "secret("] {
            assert!(!r.stdout.contains(masked), "{cmd}:\n{}", r.stdout);
        }
        assert!(
            r.stderr
                .contains("warning: headers: strict-origin-when-cross-origin\n"),
            "{cmd}:\n{}",
            r.stderr
        );
    }
    let plan = &before[1].1.stdout;
    for line in [
        "  + k8s.traefik.middleware large_upload      p.df:",
        "      metadata = { name: \"large-upload\", namespace: \"apps\" }\n",
        "      spec.buffering.maxRequestBodyBytes = 536870912\n",
        "      metadata = { name: \"security-headers\", namespace: \"apps\" }\n",
        "        referrerPolicy: \"strict-origin-when-cross-origin\",\n",
        "      spec.value = \"TOKEN-VALUE\"\n",
    ] {
        assert!(plan.contains(line), "{line}\n{plan}");
    }
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
        applied.ok && !applied.stdout.contains("differs from the plan shown"),
        "{}{}",
        applied.stdout,
        applied.stderr
    );
    // Its schema learned: `spec.value` is sensitive by it.
    never_says(
        &printers(&s, &flags, "t.spec.value", "k8s.example.io.v1.token t"),
        TOKEN,
    );
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
