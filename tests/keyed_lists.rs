//! Keyed lists (R-35, R-69): a list the schema keys (`type_list_key`, the
//! k8s mock's `containers` by `name`) is written by element:
//! `containers[k].p` and `set c.p where c in w.containers` write the
//! element whose key is `k` (or `c`'s), leaf by leaf at their own rank, and
//! several authors' lists merge by key. An unkeyed list stays one value.

mod common;
use common::{Run, Scratch};

const DEPLOYMENT: &str = "\n\nprovider k8s\n\n\
    resource k8s.deployment web {\n  \
    metadata.name = \"web\"\n  \
    spec.selector.matchLabels = { app: \"web\" }\n  \
    spec.template.spec.containers = [\n    \
    { name: \"api\", image: \"api:1\" },\n    \
    { name: \"side\", image: \"side:1\", resources: { limits: { cpu: 2, memory: 1Gi } } }\n  \
    ]\n  \
    spec.template.spec.tolerations = [{ key: \"spot\", operator: \"Exists\" }]\n\
    }\n\n";

fn project(name: &str, policy: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("main.df", &format!("{DEPLOYMENT}{policy}\n"));
    s
}

fn plan(s: &Scratch) -> Run {
    s.run(&["dev", "--world", "w.json", "plan", "main"])
}

fn has(r: &Run, lines: &[&str]) {
    for l in lines {
        assert!(r.stdout.contains(l), "{l}:\n{}{}", r.stdout, r.stderr);
    }
}

/// README.next's policy: one `@default` limits contribution per container,
/// by the container's key; a container's own limits win.
#[test]
fn an_indexed_default_writes_every_element_and_yields_to_its_own() {
    let s = project(
        "kl-index",
        "set w.spec.template.spec.containers[c.name].resources.limits = \
         { cpu: \"500m\", memory: \"256Mi\" } @default \
         where w in k8s.deployment, c in w.spec.template.spec.containers",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "containers[name=api].image = \"api:1\"",
            "containers[name=api].resources.limits.cpu = \"500m\"",
            "containers[name=api].resources.limits.memory = \"256Mi\"",
            "containers[name=side].resources.limits.cpu = \"2\"",
            "containers[name=side].resources.limits.memory = \"1Gi\"",
        ],
    );
    // `why` names each element written by its key.
    let w = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "why",
            "attr(k8s.deployment, \"web\", \"spec\", V)",
            "main",
        ])
        .success();
    for l in [
        "spec.template.spec.containers[name=api] = {resources: {limits: {cpu: 500m, memory: 256Mi}}} @default",
        "spec.template.spec.containers[name=side] = {resources: {limits: {cpu: 500m, memory: 256Mi}}} @default",
    ] {
        assert!(w.stdout.contains(l), "{l}:\n{}{}", w.stdout, w.stderr);
    }
}

/// R-69: `set c.p` with `c` an element of a keyed list writes that element;
/// the same plan as the indexed form.
#[test]
fn set_writes_through_a_bound_element() {
    let s = project(
        "kl-bound",
        "set c.resources.limits = { cpu: 500m, memory: 256Mi } @default \
         where w in k8s.deployment, c in w.spec.template.spec.containers\n\
         set c.env = [{ name: \"POS\", value: \"${i}\" }] \
         where w in k8s.deployment, (i, c) in w.spec.template.spec.containers",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "containers[name=api].resources.limits.cpu = \"500m\"",
            "containers[name=side].resources.limits.cpu = \"2\"",
            "containers[name=side].resources.limits.memory = \"1Gi\"",
            "containers[name=api].env[name=POS].value = \"0\"",
            "containers[name=side].env[name=POS].value = \"1\"",
        ],
    );
}

/// The element write's rule reads the list as the blocks and whole-list
/// writes give it (the base), so it may test what it writes.
#[test]
fn an_element_write_reads_the_list_without_element_writes() {
    let s = project(
        "kl-base",
        "set w.spec.template.spec.containers[c.name].resources.limits.memory = 512Mi @default \
         where w in k8s.deployment, c in w.spec.template.spec.containers, \
         not has c.resources.limits.memory",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "containers[name=api].resources.limits.memory = \"512Mi\"",
            "containers[name=side].resources.limits.memory = \"1Gi\"",
        ],
    );
}

/// Any term indexes a keyed list: a string makes the element when no list
/// has it, an `@override` replaces one leaf; and a second author's whole
/// list merges with the block's by key.
#[test]
fn a_literal_key_and_a_second_list_merge_by_key() {
    let s = project(
        "kl-literal",
        "set w.spec.template.spec.containers[\"api\"].image = \"api:2\" @override \
         where w in k8s.deployment\n\
         set w.spec.template.spec.containers[\"log\"] = { image: \"log:1\" } \
         where w in k8s.deployment\n\
         set w.spec.template.spec.containers = [{ name: \"proxy\", image: \"proxy:1\" }] \
         where w in k8s.deployment",
    );
    let r = plan(&s).success();
    has(
        &r,
        &[
            "containers[name=api].image = \"api:2\"",
            "containers[name=log].image = \"log:1\"",
            "containers[name=proxy].image = \"proxy:1\"",
            "containers[name=side].image = \"side:1\"",
        ],
    );
}

/// Two writes of one leaf at one rank conflict, named by the element and
/// the leaf.
#[test]
fn a_same_rank_disagreement_names_the_element() {
    let s = project(
        "kl-conflict",
        "set w.spec.template.spec.containers[\"api\"].image = \"other\" where w in k8s.deployment",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "query", "deny(M, C)", "main"])
        .success();
    assert!(
        r.stdout.contains(
            "two contributions disagree at spec.template.spec.containers[name=api].image"
        ),
        "{}{}",
        r.stdout,
        r.stderr
    );
}

/// An element of a list with no key is not written by key: the error names
/// the declaration that would key it.
#[test]
fn an_unkeyed_list_is_one_value() {
    let s = project(
        "kl-unkeyed",
        "set w.spec.template.spec.tolerations[t.key].effect = \"NoSchedule\" \
         where w in k8s.deployment, t in w.spec.template.spec.tolerations",
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr.contains(
            "resource k8s.deployment[\"web\"]: spec.template.spec.tolerations is not a keyed list"
        ) && r.stderr.contains(
            "type_list_key(k8s.deployment, \"spec.template.spec.tolerations\", [\"FIELD\"])"
        ),
        "{}",
        r.stderr
    );
}

/// `set c.p` over an unkeyed list is the same error.
#[test]
fn a_bound_element_of_an_unkeyed_list_is_an_error() {
    let s = project(
        "kl-bound-unkeyed",
        "set t.effect = \"NoSchedule\" \
         where w in k8s.deployment, t in w.spec.template.spec.tolerations",
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr
            .contains("spec.template.spec.tolerations is not a keyed list"),
        "{}",
        r.stderr
    );
}

/// A quantity in a `set` with no rank is read by its attribute's type as
/// one with a rank is (`types::read` reads the 4-ary `arg` of an unranked
/// `set`): `500m` in a container's cpu limit is millicores.
#[test]
fn a_quantity_in_an_unranked_set_is_read_by_its_attribute() {
    let s = Scratch::project("keyed-unranked-set");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write(
        "main.df",
        "\ninput on: bool = true\n\nprovider k8s\n\n\
         resource k8s.deployment job {\n  metadata.name = \"job\"\n  \
         spec.selector.matchLabels = { app: \"job\" }\n}\n\n\
         set job.spec.template.spec.containers = [\n  \
         { name: \"x\", image: \"x:1\", resources: { limits: { cpu: 500m } } }\n] where on\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "main"])
        .success();
    assert!(
        r.stdout.contains("resources.limits.cpu = \"500m\""),
        "{}",
        r.stdout
    );
}

/// A body reads an element by its key (`containers["api"]`, or a read,
/// `[c.name]`): the element whose key field is that value, no row for a
/// key no element has; an integer is still the position.
#[test]
fn a_body_reads_an_element_by_its_key() {
    let s = project(
        "kl-read",
        "deny \"api ${i}\" where w in k8s.deployment, \
         i = w.spec.template.spec.containers[\"api\"].image\n\
         deny \"side ${i}\" where i = web.spec.template.spec.containers[\"side\"].resources.limits.cpu\n\
         deny \"none ${i}\" where i = web.spec.template.spec.containers[\"none\"].image\n\
         deny \"again ${i}\" where c in web.spec.template.spec.containers, c.name == \"api\", \
         i = web.spec.template.spec.containers[c.name].image\n\
         deny \"first ${i}\" where i = web.spec.template.spec.containers[0].name\n",
    );
    let r = plan(&s).failure();
    for l in [
        "- api api:1\n",
        "- side 2\n",
        "- again api:1\n",
        "- first api\n",
    ] {
        assert!(r.stderr.contains(l), "{l}:\n{}", r.stderr);
    }
    assert!(!r.stderr.contains("none"), "{}", r.stderr);
}
