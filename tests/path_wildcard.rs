//! `[_]` in a path (R-162, docs/grammar.md "Paths"): an anonymous binding
//! per step, over every resource of a type, every element of a list and
//! every value of an object. A `set` is the clause form with a variable
//! per `[_]`, a read enumerates, `v in PATH` binds each value, and a
//! document's selector takes the same step; `[*]` is an error naming it.

mod common;
use common::{Run, Scratch};

const WORKLOADS: &str = "use k8s\n\n\
    resource k8s.stateful_set db {\n  \
    metadata.name = \"db\"\n  \
    metadata.labels = { tier: \"data\" }\n  \
    spec.serviceName = \"db\"\n  \
    spec.selector.matchLabels = { app: \"db\" }\n  \
    spec.template.spec.containers = [\n    \
    { name: \"pg\", image: \"postgres:16\" },\n    \
    { name: \"exporter\", image: \"exporter:1\", resources: { limits: { cpu: 1, memory: 1Gi } } }\n  \
    ]\n\
    }\n\n\
    resource k8s.stateful_set cache {\n  \
    metadata.name = \"cache\"\n  \
    spec.serviceName = \"cache\"\n  \
    spec.selector.matchLabels = { app: \"cache\" }\n  \
    spec.template.spec.containers = [{ name: \"redis\", image: \"redis:7\" }]\n\
    }\n\n\
    resource k8s.deployment web {\n  \
    metadata.name = \"web\"\n  \
    spec.selector.matchLabels = { app: \"web\" }\n  \
    spec.template.spec.containers = [{ name: \"api\", image: \"api:latest\" }]\n\
    }\n\n";

fn project(name: &str, policy: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("main.df", &format!("{WORKLOADS}{policy}\n"));
    s
}

fn plan(s: &Scratch) -> Run {
    s.run(&["dev", "--world", "w.json", "plan", "-vv", "main.df"])
}

fn has(r: &Run, lines: &[&str]) {
    for l in lines {
        assert!(r.stdout.contains(l), "{l}:\n{}{}", r.stdout, r.stderr);
    }
}

/// The user's policy: a `@default` limit on every container of every
/// stateful set, one `[_]` at the type and one at the list. Each
/// container there is gets it, its own limits win, the deployment is
/// untouched, and `why` names each binding by its path under `with`.
#[test]
fn a_set_through_two_wildcards_writes_every_element() {
    let s = project(
        "wild-sts",
        "set k8s.stateful_set[_].spec.template.spec.containers[_].resources.limits = \
         { cpu: 500m, memory: 256Mi } @default",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "containers[name=pg].resources.limits.cpu = \"500m\"",
            "containers[name=pg].resources.limits.memory = \"256Mi\"",
            "containers[name=exporter].resources.limits.cpu = \"1\"",
            "containers[name=exporter].resources.limits.memory = \"1Gi\"",
            "containers[name=redis].resources.limits.cpu = \"500m\"",
        ],
    );
    // Over a list the write touches the elements there are: no new one.
    assert!(!r.stdout.contains("containers[name=_]"), "{}", r.stdout);
    assert!(
        !r.stdout.contains("containers[name=api].resources"),
        "{}",
        r.stdout
    );
    let w = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "why",
            "--tree",
            "attr(k8s.stateful_set, \"cache\", \"spec\", V)",
            "main.df",
        ])
        .success();
    assert!(
        w.stdout.contains(
            "with k8s.stateful_set[_] = k8s.stateful_set cache, containers[_] = \
             {image: \"redis:7\", name: \"redis\"}"
        ),
        "{}",
        w.stdout
    );
    // `fmt` keeps the path as written.
    s.run(&["fmt", "main.df"]).success();
    let src = s.read("main.df");
    assert!(
        src.contains(
            "set k8s.stateful_set[_].spec.template.spec.containers[_].resources.limits = {"
        ),
        "{src}"
    );
}

/// The written form is the clause form: the same plan as `where w in T,
/// c in w.l`.
#[test]
fn a_wildcard_set_is_the_clause_form() {
    let limits = "{ cpu: 500m, memory: 256Mi } @default";
    let a = project(
        "wild-a",
        &format!(
            "set k8s.stateful_set[_].spec.template.spec.containers[_].resources.limits = {limits}"
        ),
    );
    let b = project(
        "wild-b",
        &format!(
            "set c.resources.limits = {limits} where w in k8s.stateful_set, \
             c in w.spec.template.spec.containers"
        ),
    );
    assert_eq!(plan(&a).success().stdout, plan(&b).success().stdout);
}

/// `T[_]` alone: every resource of the type, and a key beside a `[_]`
/// takes one element.
#[test]
fn a_wildcard_on_a_type_and_a_key_beside_it() {
    let s = project(
        "wild-type",
        "set k8s.stateful_set[_].metadata.annotations.owner = \"platform\"\n\
         set k8s.stateful_set[_].spec.template.spec.containers[\"pg\"].image = \"postgres:17\" \
         @override",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "metadata.annotations.owner = \"platform\"",
            "containers[name=pg].image = \"postgres:17\"",
            "containers[name=redis].image = \"redis:7\"",
        ],
    );
    assert_eq!(
        r.stdout.matches("metadata.annotations.owner").count(),
        2,
        "{}",
        r.stdout
    );
}

/// A policy reads every container image: a read enumerates, `v in PATH`
/// binds each value, `T[_]` on the right of `in` is the type, and `has`
/// over an object's `[_]` holds for some key.
#[test]
fn a_policy_reads_every_image() {
    let s = project(
        "wild-read",
        "warn \"latest: ${img}\" where img in k8s.deployment[_].spec.template.spec.containers[_].image, \
         str.ends_with(img, \":latest\")\n\
         warn \"image ${i}\" where i = k8s.stateful_set[_].spec.template.spec.containers[_].image\n\
         warn \"labelled ${s.metadata.name}\" where s in k8s.stateful_set[_], has s.metadata.labels[_]\n\
         warn \"bare ${s.metadata.name}\" where s in k8s.stateful_set, not has s.metadata.labels[_]",
    );
    let r = plan(&s).success();
    let out = format!("{}{}", r.stdout, r.stderr);
    for l in [
        "latest: api:latest",
        "image postgres:16",
        "image exporter:1",
        "image redis:7",
        "labelled db",
        "bare cache",
    ] {
        assert!(out.contains(l), "{l}:\n{out}");
    }
    assert!(
        !out.contains("labelled cache") && !out.contains("bare db"),
        "{out}"
    );
}

/// A document's selector steps with `[_]`, over a list and over an
/// object's values; `[*]` is an error that names `[_]`.
#[test]
fn a_selector_takes_the_same_step() {
    let s = Scratch::project("wild-doc");
    s.write(
        "teams.yaml",
        "teams:\n  - name: a\n    services: [{svc: x}, {svc: y}]\n  - name: b\n    services: [{svc: z}]\n\
         regions:\n  eu: {zones: [{zone: eu1}]}\n  us: {zones: [{zone: us1}, {zone: us2}]}\n",
    );
    s.write(
        "main.df",
        "input service from yaml.decode(io.read(\"teams.yaml\")).teams[_].services\n\
         input zone from yaml.decode(io.read(\"teams.yaml\")).regions[_].zones\n\
         decl service(name: string, svc: string)\n\
         decl zone(zone: string)\n\
         use fake\n",
    );
    let r = s.run(&["query", "service(n, v)", "main.df"]).success();
    for row in ["\"a\"  \"x\"", "\"a\"  \"y\"", "\"b\"  \"z\""] {
        assert!(r.stdout.contains(row), "{row}\n{}", r.stdout);
    }
    let r = s.run(&["query", "zone(z)", "main.df"]).success();
    for z in ["eu1", "us1", "us2"] {
        assert!(r.stdout.contains(z), "{z}\n{}", r.stdout);
    }
    s.write(
        "main.df",
        "input service from yaml.decode(io.read(\"teams.yaml\")).teams[*].services\n\
         decl service(name: string, svc: string)\n\
         use fake\n",
    );
    let r = s.run(&["query", "service(n, v)", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("`[*]` is no step, every element is `[_]`"),
        "{}",
        r.stderr
    );
}
