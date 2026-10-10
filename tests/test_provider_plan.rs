//! `dform test` asks each provider's Plan of every combination with no
//! credentials (R-188): a provider whose Plan answers so as it would
//! with them (`offline`: the k8s provider against its snapshot schema,
//! the OVH provider's own checks, the mock) is asked, and its refusal
//! fails the combination at the resource's site in the plan's words; one
//! whose Plan needs its credentials is not asked, and the test says so
//! once.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::{self, Server};

/// `dform ARGS` in `s`, with no cluster and no credentials of the
/// machine's in reach, and `env` set.
fn dform(s: &Scratch, env: &[(&str, String)], args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(args)
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("KUBECONFIG")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("DFORM_K8S_OFFLINE")
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// A program on the real k8s provider (`providers/k8s/` beside it), after
/// its `header`.
fn k8s(name: &str, header: &str, program: &str) -> Scratch {
    let s = Scratch::project(name);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "p.df",
        &format!("{header}use k8s {{ source = \"./providers/k8s\" }}\n{program}"),
    );
    s
}

/// The k8s provider refuses offline, from its snapshot, a ConfigMap with
/// no name: the combination fails as `plan` says it, at its site; with a
/// name it passes. An image's digest is still stood in.
#[test]
fn a_resource_the_k8s_provider_refuses_fails_the_test() {
    let s = k8s(
        "test-plan-k8s",
        "input named: bool = false\n",
        "resource k8s.config_map cm {\n  \
           metadata = { namespace: \"apps\" }\n  \
           data = { image: oci.resolve(\"registry.invalid/acme/app:v1\") }\n\
         }\n\
         set cm.metadata.name = \"cm\" where named\n",
    );
    let r = dform(&s, &[], &["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "named  result\nfalse  error\ntrue   ok\nerror  dform plan p --set named=false\n  \
             refused  plan k8s.config_map cm: metadata.name is not set, nor \
             metadata.generateName\n    p.df:3  resource k8s.config_map cm {\n"
        ) && r.stdout.contains(
            "note: no registry is asked: an image's digest is the one this machine last \
             resolved, else a stand-in\n"
        ),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(r.stdout.matches("note:").count(), 1, "{}", r.stdout);
}

/// The OVH provider's Plan checks a subnet's pool against its range with
/// no credentials: the test fails at the subnet, and the API is never
/// reached, though the environment holds credentials for it.
#[test]
fn the_ovh_provider_plans_with_no_credentials() {
    let server = Server::start();
    let s = Scratch::project("test-plan-ovh");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-ovh")
        ),
    );
    s.write(
        "main.df",
        "use ovh\n\
         resource ovh.network lab { name = \"lab\", regions = [\"BHS5\"] }\n\
         resource ovh.subnet lab {\n  network = lab\n  region = \"BHS5\"\n  \
         range = \"10.42.0.0/24\"\n  pool = \"10.43.0.10..=10.43.0.200\"\n}\n",
    );
    let env: Vec<(&str, String)> = server
        .env()
        .into_iter()
        .chain([("OVH_CLOUD_PROJECT_SERVICE", fake::DESCRIPTION.to_string())])
        .collect();
    let r = dform(&s, &env, &["test", "main.df"]).failure();
    assert!(
        r.stdout.contains(
            "  refused  plan ovh.subnet lab: \
             pool 10.43.0.10..=10.43.0.200 is not in range 10.42.0.0/24"
        ) && r
            .stdout
            .contains("\n    main.df:3  resource ovh.subnet lab {\n"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(server.connections(), 0);
}

/// A provider whose Plan needs its credentials (the mock, as one that
/// does not declare `offline`) is not asked: said once over every
/// combination, and the test passes on its denies alone.
#[test]
fn a_provider_without_an_offline_plan_is_noted_once() {
    let s = Scratch::new("test-plan-note");
    s.write(
        "p.df",
        "input wide: bool = false\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet a { cidr = \"10.0.1.0/24\", vpc = main }\n\
         use fake\n",
    );
    let no = [("DFORM_TEST_FAKE_NO_OFFLINE", "1".to_string())];
    let r = dform(&s, &no, &["test", "p.df"]).success();
    assert_eq!(
        r.stdout,
        "test p: 2 combinations of wide\n\
         note: fakecloud's Plan not run: no offline schema and no fake\n\
         wide   result\nfalse  ok\ntrue   ok\n\
         test p: 2 combinations, 0 failed\n"
    );
    let r = dform(&s, &[], &["test", "p.df"]).success();
    assert!(!r.stdout.contains("note:"), "{}", r.stdout);
}
