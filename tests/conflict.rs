//! A conflict is reported as the conflict (R-35 leftovers): a resource whose
//! attribute's contributions disagree is not handed to the provider, so its
//! refusal of the incomplete document ("required attribute .. is not set")
//! does not hide it, and the conflict names the leaf that disagrees and,
//! for an element of a keyed list, its key.

mod common;
use common::{Run, Scratch};

fn plan(name: &str, program: &str) -> Run {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("main.df", program);
    s.run(&["dev", "--world", "w.json", "plan", "main.df"])
}

/// A conflict under `spec`, whose `spec.selector.matchLabels` is required:
/// the plan shows the conflict at the element's leaf, not the provider's
/// refusal.
#[test]
fn a_conflict_under_a_required_attribute_is_the_conflict() {
    let r = plan(
        "conflict-required",
        "\n\nuse k8s\n\n\
         resource k8s.deployment web {\n  \
         metadata.name = \"web\"\n  \
         spec.selector.matchLabels = { app: \"web\" }\n  \
         spec.template.spec.containers = [{ name: \"api\", image: \"api:1\" }]\n\
         }\n\n\
         set w.spec.template.spec.containers[\"api\"].image = \"other\" where w in k8s.deployment\n",
    )
    .failure();
    assert!(!r.stderr.contains("is not set"), "{}", r.stderr);
    assert!(
        r.stdout.contains(
            "! k8s.deployment web.spec: two contributions disagree at \
             spec.template.spec.containers[name=api].image\n"
        ),
        "{}{}",
        r.stdout,
        r.stderr
    );
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

/// Two writes of `metadata.name`: the conflict names `metadata.name`, not
/// the top attribute alone.
#[test]
fn a_conflict_names_its_leaf() {
    let r = plan(
        "conflict-leaf",
        "\n\nuse k8s\n\n\
         resource k8s.namespace n {\n  metadata.name = \"a\"\n}\n\n\
         set n.metadata.name = \"b\" where n in k8s.namespace\n",
    )
    .failure();
    assert!(
        r.stdout
            .contains("! k8s.namespace n.metadata: two contributions disagree at metadata.name\n"),
        "{}{}",
        r.stdout,
        r.stderr
    );
}
