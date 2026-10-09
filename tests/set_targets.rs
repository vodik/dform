//! A `set` target is one address grammar: a resource by the name its
//! block declares (`web`) or by its type and key (`k8s.deployment["api"]`
//! for one named by its clause), then a path, an element of a keyed list
//! by its key in it (`containers["web"]`) in either spelling. A target
//! that starts with no name in scope is the error at the target, in the
//! program's words.

mod common;
use common::{Run, Scratch};

/// Two deployments, one named in its block and one by its clause.
const DEPLOYMENTS: &str = "\nuse k8s\non(1)\n\
    resource k8s.deployment web {\n  \
    metadata.name = \"web\"\n  \
    spec.selector.matchLabels = { app: \"web\" }\n  \
    spec.template.spec.containers = [{ name: \"web\", image: \"web:1\" }]\n\
    }\n\
    resource k8s.deployment \"${n}\" {\n  \
    metadata.name = n\n  \
    spec.selector.matchLabels = { app: n }\n  \
    spec.template.spec.containers = [{ name: n, image: \"api:1\" }]\n\
    } where n in [\"api\"]\n";

fn plan(name: &str, sets: &str) -> Run {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("main.df", &format!("{DEPLOYMENTS}{sets}\n"));
    s.run(&["dev", "--world", "w.json", "plan", "-vv", "main.df"])
}

/// The short name with an element key writes the element, as the type
/// and key do for a resource named by its clause.
#[test]
fn a_target_names_an_element_by_the_resources_name_or_its_type_and_key() {
    let r = plan(
        "set-target-element",
        "set web.spec.template.spec.containers[\"web\"].image = \"web:2\" @override where on(1)\n\
         set k8s.deployment[\"api\"].spec.template.spec.containers[\"api\"].image = \"api:2\" \
         @override where on(1)",
    )
    .success();
    for l in [
        "containers[name=web].image = \"web:2\"",
        "containers[name=api].image = \"api:2\"",
    ] {
        assert!(r.stdout.contains(l), "{l}:\n{}{}", r.stdout, r.stderr);
    }
}

/// `web` names nothing when the deployment is named by its clause: the
/// target is the error, with or without an element key, and the
/// message is never the compiler's.
#[test]
fn a_target_that_starts_with_no_name_is_the_error_at_the_target() {
    for target in [
        "api.spec.replicas",
        "api.spec.template.spec.containers[\"api\"].image",
    ] {
        let r = plan(
            "set-target-unknown",
            &format!("set {target} = \"x\" @override where on(1)"),
        )
        .failure();
        assert!(
            r.stderr.contains(
                "`set` writes a resource's attribute or an input: nothing in scope is named `api`"
            ) && r.stderr.contains("by its type and key, `T[\"api\"]`"),
            "{target}: {}",
            r.stderr
        );
        assert!(!r.stderr.contains("want"), "{target}: {}", r.stderr);
    }
}

/// A resource by its type and a key no block declares: the attribute
/// written and the address, as the plan prints them.
#[test]
fn a_target_of_no_resource_names_the_address() {
    let r = plan(
        "set-target-none",
        "set k8s.deployment[\"nope\"].spec.replicas = 2 where on(1)",
    )
    .failure();
    assert!(
        r.stderr.contains(
            "k8s.deployment nope.spec is written, but no resource k8s.deployment nope is declared"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("want"), "{}", r.stderr);
}
