//! A `set(T)` attribute (R-158): its contributions union, each element
//! with its site. Several modules each add a policy to one role, which is
//! what Terraform's attachment resources are for; here the role's
//! `policies` is a set and each module's `set` adds its element. Ranks
//! apply per contribution; the provider receives the whole set, in one
//! order; removing a module removes its element, an update of the set.

mod common;
use common::Scratch;

/// Three modules, each with its policy, each adding it to the role the
/// stack gives it.
fn project(uses: &[&str]) -> Scratch {
    let s = Scratch::project("sets");
    let mut main = String::from("use fake\nuse identity\n");
    for m in uses {
        main.push_str(&format!("use {m} {{ role = identity.app_role }}\n"));
    }
    s.write("stacks/main.df", &main);
    s.write(
        "identity.df",
        "resource iam.role app_role { name = \"app\" }\n",
    );
    for m in ["database", "cache", "queue"] {
        s.write(
            &format!("{m}.df"),
            &format!(
                "input role: iam.role\n\
                 resource iam.policy access {{ name = \"{m}\" }}\n\
                 set role.policies = [access]\n"
            ),
        );
    }
    s
}

const ALL: [&str; 3] = ["database", "cache", "queue"];

fn plan(s: &Scratch) -> String {
    s.run(&["plan", "main"]).success().stdout
}

/// The role's lines of a plan, from its change line to the next change.
fn role(out: &str) -> Vec<&str> {
    let mut lines = out
        .lines()
        .skip_while(|l| !l.contains("iam.role identity.app_role"));
    let head = lines.next().into_iter();
    head.chain(lines.take_while(|l| l.starts_with("      ")))
        .map(str::trim_end)
        .collect()
}

#[test]
fn three_modules_each_add_an_element_with_its_site() {
    let s = project(&ALL);
    let out = plan(&s);
    assert_eq!(
        role(&out),
        [
            "  + iam.role identity.app_role   identity.df:1",
            "      name = \"app\"",
            "      policies[cache.access]     cache.df:3",
            "      policies[database.access]  database.df:3",
            "      policies[queue.access]     queue.df:3",
        ],
        "{out}"
    );
    // The role waits on the policies it references.
    let at = |n: &str| out.find(n).unwrap();
    assert!(at("+ iam.policy queue.access") < at("+ iam.role identity.app_role"));
}

#[test]
fn removing_a_module_removes_its_element() {
    let s = project(&ALL);
    s.run(&["apply", "main"]).success();
    assert_eq!(
        s.run(&["plan", "main"]).success().summary(),
        "stack main is up to date"
    );
    let s2 = project(&["database", "cache"]);
    std::fs::rename(s.path("dform.state"), s2.path("dform.state")).unwrap();
    let out = plan(&s2);
    assert!(out.contains("1 update, 1 delete"), "{out}");
    assert_eq!(
        role(&out),
        [
            "  ~ iam.role identity.app_role  identity.df:1",
            "      - policies[queue.access]",
        ],
        "{out}"
    );
    // The role lets go of the policy before the policy is deleted.
    assert!(out.find("- policies[queue.access]") < out.find("- iam.policy queue.access"));
}

#[test]
fn a_module_added_is_an_element_more() {
    let s = project(&["database", "cache"]);
    s.run(&["apply", "main"]).success();
    let s2 = project(&ALL);
    std::fs::rename(s.path("dform.state"), s2.path("dform.state")).unwrap();
    let out = plan(&s2);
    assert!(out.contains("1 create, 1 update"), "{out}");
    assert_eq!(
        role(&out),
        [
            "  ~ iam.role identity.app_role  identity.df:1",
            "      + policies[queue.access]  queue.df:3",
        ],
        "{out}"
    );
}

#[test]
fn one_writer_prints_the_set_as_written() {
    let s = project(&["database"]);
    let out = plan(&s);
    assert_eq!(
        role(&out),
        [
            "  + iam.role identity.app_role      identity.df:1",
            "      name = \"app\"",
            "      policies = [database.access]  database.df:3",
        ],
        "{out}"
    );
}

/// The provider gets one list, the same whatever order the writers come
/// in.
#[test]
fn the_provider_receives_the_whole_set_in_one_order() {
    let ids = |uses: &[&str]| {
        let s = project(uses);
        s.run(&["dev", "--world", "w.json", "apply", "main"])
            .success();
        s.json("w.json")["resources"]["iam.role::identity.app_role"]["attrs"]["policies"].clone()
    };
    let one = ids(&ALL);
    assert_eq!(one.as_array().map(Vec::len), Some(3), "{one}");
    assert_eq!(one, ids(&["queue", "database", "cache"]));
}

#[test]
fn why_says_each_element_by_its_writer() {
    let s = project(&ALL);
    let out = s
        .run(&["why", "iam.role[\"identity.app_role\"].policies", "main"])
        .success()
        .stdout;
    for m in ALL {
        assert!(
            out.contains(&format!("= iam.policy {m}.access\n"))
                && out.contains(&format!("{m}.df:3")),
            "{out}"
        );
    }
}

/// Ranks apply per contribution: an `@override` set replaces the union,
/// a `@default` one yields to any normal one; two that share an element
/// never conflict.
#[test]
fn ranks_apply_per_contribution_and_a_union_never_conflicts() {
    let value = |extra: &str| {
        let s = Scratch::new("set-ranks");
        s.write(
            "p.df",
            &format!(
                "resource iam.role app {{ name = \"app\", policies = [a, b] }}\n\
                 resource iam.policy a {{ name = \"a\" }}\n\
                 resource iam.policy b {{ name = \"b\" }}\n\
                 resource iam.policy c {{ name = \"c\" }}\n{extra}"
            ),
        );
        let out = s
            .run(&[
                "dev",
                "--provider",
                "fake",
                "query",
                "attr(\"iam.role\", \"app\", \"policies\", v)",
                "p.df",
            ])
            .success()
            .stdout;
        out.lines().nth(1).unwrap_or_default().trim().to_string()
    };
    let w = "where r in iam.role";
    assert_eq!(
        value(&format!("set r.policies = [b, c] {w}\n")),
        "[iam.policy a, iam.policy b, iam.policy c]"
    );
    assert_eq!(
        value(&format!("set r.policies = [c] @override {w}\n")),
        "[iam.policy c]"
    );
    assert_eq!(
        value(&format!("set r.policies = [c] @default {w}\n")),
        "[iam.policy a, iam.policy b]"
    );
}

#[test]
fn an_attachment_type_is_an_error_naming_the_attribute() {
    let s = Scratch::new("set-attachment");
    s.write(
        "p.df",
        "resource iam.role app { name = \"app\" }\n\
         resource iam.policy p { name = \"p\" }\n\
         resource iam.role_policy_attachment a { role = app, policy = p }\n",
    );
    let out = s
        .run(&["dev", "--provider", "fake", "plan", "p.df"])
        .failure();
    assert!(
        out.stderr.contains(
            "iam.role_policy_attachment is no resource: it is iam.role's attribute policies"
        ),
        "{}",
        out.stderr
    );
}

/// A network's peerings (the gke mock): a set of objects, each writer's
/// element on its own line with its site; the old peering resource is an
/// error naming the attribute.
#[test]
fn a_networks_peerings_are_a_set_of_objects() {
    let s = Scratch::new("set-peerings");
    s.write(
        "p.df",
        "resource google.compute_network main {\n  name = \"main\"\n  \
         peerings = [{ name: \"a\", peer_network: \"projects/a/global/networks/a\" }]\n}\n\
         set n.peerings = [{ name: \"b\", peer_network: \"projects/b/global/networks/b\" }] \
         where n in google.compute_network\n",
    );
    let out = s
        .run(&["dev", "--provider", "gke", "plan", "p.df"])
        .success()
        .stdout;
    assert!(
        out.contains(
            "      peerings[1] = { name: \"b\", peer_network: \"projects/b/global/networks/b\" }  p.df:5\n"
        ),
        "{out}"
    );
    s.write(
        "q.df",
        "resource google.compute_network_peering p { name = \"a\" }\n",
    );
    let r = s
        .run(&["dev", "--provider", "gke", "plan", "q.df"])
        .failure();
    assert!(
        r.stderr
            .contains("it is google.compute_network's attribute peerings"),
        "{}",
        r.stderr
    );
}
