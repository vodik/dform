//! The objects state holds of a provider the program configures (R-177):
//! a kubeconfig read from a server state already maps. The program is
//! evaluated first, so the provider is configured from what state holds,
//! and only then is what it serves read and an Apply call whose answer
//! was lost looked up. While its settings are not known (the read "not
//! yet", the server gone), its objects are listed under `later` as state
//! has them (`=`, no diff), counted `later`, and read at the boundary
//! that configures it; a create whose answer was lost is sent again with
//! its idempotency key then. No message names that key.

mod common;
use common::{Run, Scratch};

/// The k3s shape on the mock: a server, a provider configured from it,
/// the cluster's objects. `KUBECONFIG` is where the settings come from.
const PROG: &str = r#"
use fake { source = "prov" }
resource db.postgres server { name = "server" }
use k8s { kubeconfig = KUBECONFIG }
resource k8s.namespace ns { metadata.name = "app" }
resource k8s.namespace web { metadata.name = "web" }
resource k8s.namespace cache { metadata.name = "cache" }
"#;

const FROM_SERVER: &str = r#"str.format("kc@%s", server.endpoint)"#;

/// A host that refuses the connection: the read is "not yet".
const NOT_YET: &str = r#"io.read("ssh://ubuntu@127.0.0.1:1/etc/rancher/k3s/k3s.yaml")"#;

fn program(kubeconfig: &str) -> String {
    PROG.replace("KUBECONFIG", kubeconfig)
}

/// Applied: the server and the cluster's three objects exist.
fn applied(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", &program(FROM_SERVER));
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    dev(&s, &["apply", "--yes"]).success();
    s
}

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    let out = common::dform()
        .args(&all)
        .env("NO_COLOR", "1")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    Run::from(out)
}

/// The create of `cache` made its object and its answer was lost: state
/// does not map it, and holds the call as uncertain under the key the
/// object carries.
fn lose_the_create(s: &Scratch) -> String {
    let world = s.json("w.json");
    let key = world["resources"]["k8s.namespace::cache"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut st = s.json("w.state.json");
    st["resources"]
        .as_object_mut()
        .unwrap()
        .remove("k8s.namespace::cache")
        .unwrap();
    st["uncertain"] = serde_json::json!({
        "k8s.namespace::cache": { "op": "create", "key": key }
    });
    s.write("w.state.json", &st.to_string());
    key
}

/// The server goes behind dform's back: no kubeconfig can be made.
fn lose_the_server(s: &Scratch) {
    let mut cloud = s.json("w.fakecloud.json");
    cloud["resources"]
        .as_object_mut()
        .unwrap()
        .remove("db.postgres::server")
        .unwrap();
    s.write("w.fakecloud.json", &cloud.to_string());
}

fn namespaces(s: &Scratch) -> Vec<String> {
    let world = s.json("w.json");
    let mut out: Vec<String> = world["resources"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with("k8s.namespace::"))
        .cloned()
        .collect();
    out.sort();
    out
}

/// The provider is configured from the server state maps before its
/// objects are read: a change behind dform's back is planned, and a
/// create whose answer was lost is found by its key, not an error that
/// the provider is not configured yet.
#[test]
fn objects_of_a_provider_configured_from_state_are_read_once_it_is() {
    let s = applied("deferred-refresh-resolves");
    let key = lose_the_create(&s);
    let mut cloud = s.json("w.json");
    cloud["resources"]["k8s.namespace::web"]["attrs"]["metadata"]["labels"] =
        serde_json::json!({ "team": "ops" });
    s.write("w.json", &cloud.to_string());
    let r = dev(&s, &["plan"]).success();
    let all = format!("{}{}", r.stdout, r.stderr);
    assert!(
        all.contains(
            "k8s.namespace cache: the create whose answer was lost made cache; state maps it"
        ),
        "{all}"
    );
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{all}"
    );
    assert!(r.stdout.contains("  ~ k8s.namespace web"), "{all}");
    assert!(!r.stdout.contains("later"), "{all}");
    assert!(!all.contains(&key), "{all}");
}

/// The kubeconfig's read is "not yet": the provider's objects in state
/// are listed under `later` as state has them, no diff, counted there;
/// the create whose answer was lost waits with them. The plan exits 0.
#[test]
fn objects_of_a_provider_waiting_on_a_read_are_listed_under_later() {
    let s = applied("deferred-refresh-not-yet");
    let key = lose_the_create(&s);
    s.write("p.df", &program(NOT_YET));
    let r = dev(&s, &["plan"]).success();
    let all = format!("{}{}", r.stdout, r.stderr);
    assert_eq!(r.summary(), "plan: 0 changes, 3 later", "{all}");
    let (_, later) = r.stdout.split_once("\nlater\n").expect(&all);
    for line in [
        "  waits on  provider k8s  kubeconfig = io.read(\"ssh://ubuntu@127.0.0.1:1/etc/rancher/k3s/k3s.yaml\"), ssh://ubuntu@127.0.0.1:1/etc/rancher/k3s/k3s.yaml\n",
        "  = k8s.namespace ns     p.df:5\n",
        "  = k8s.namespace web    p.df:6\n",
        "  + k8s.namespace cache  p.df:7\n",
    ] {
        assert!(later.contains(line), "{line}\n{all}");
    }
    // As state has them: nothing under an object's line but the next.
    assert!(
        later.contains("  = k8s.namespace ns     p.df:5\n  = k8s.namespace web"),
        "{all}"
    );
    assert!(!all.contains(&key), "{all}");
    assert!(!all.contains("no cloud"), "{all}");
}

/// The server is gone: its provider's objects wait under `later` as
/// state has them; apply makes the server, configures the provider at the
/// boundary, reads them then, and sends the lost create again with its
/// key: nothing is made twice.
#[test]
fn the_boundary_reads_them_once_the_provider_is_configured() {
    let s = applied("deferred-refresh-boundary");
    lose_the_create(&s);
    lose_the_server(&s);
    let p = dev(&s, &["plan"]).success();
    assert_eq!(
        p.summary(),
        "plan: 1 change (1 create) over 1 tick, 3 later",
        "{}",
        p.stdout
    );
    let (_, later) = p.stdout.split_once("\nlater\n").unwrap();
    assert!(
        later.starts_with(
            "  waits on  provider k8s  kubeconfig = str.format(\"kc@%s\", server.endpoint)\n"
        ),
        "{}",
        p.stdout
    );
    for line in [
        "  = k8s.namespace ns     p.df:5\n",
        "  = k8s.namespace web    p.df:6\n",
        "  + k8s.namespace cache  p.df:7\n",
    ] {
        assert!(later.contains(line), "{line}\n{}", p.stdout);
    }
    let a = dev(&s, &["apply", "--yes"]).success();
    assert!(
        a.stdout.contains(
            "carried over from an interrupted apply:\n  k8s.namespace cache  (sent again with \
             its idempotency key once its provider is configured)\n"
        ),
        "{}",
        a.stdout
    );
    assert!(
        a.stdout
            .contains("provider k8s: configured after tick 1: kubeconfig = kc@server.db.fake"),
        "{}",
        a.stdout
    );
    assert_eq!(
        namespaces(&s),
        [
            "k8s.namespace::cache",
            "k8s.namespace::ns",
            "k8s.namespace::web"
        ]
    );
    let st = s.json("w.state.json");
    assert!(st["resources"]["k8s.namespace::cache"].is_object(), "{st}");
    assert!(st.get("uncertain").is_none(), "{st}");
    dev(&s, &["plan"]).success();
    assert_eq!(dev(&s, &["plan"]).summary(), "stack p is up to date");
}

/// A lost create the provider cannot look up (no cluster to ask): the
/// error names the object by its address, never by the idempotency key
/// dform gave the call.
#[test]
fn a_lookup_that_fails_names_the_address_not_the_key() {
    let s = Scratch::new("deferred-refresh-message");
    s.write(
        "p.df",
        "use k8s { source = \"prov\" }\nresource k8s.namespace ns { metadata.name = \"app\" }\n",
    );
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("prov/dform-provider-k8s"),
    )
    .unwrap();
    let key = "dform-0123456789abcdef0123456789abcdef";
    s.write(
        "w.state.json",
        &serde_json::json!({
            "version": 1,
            "resources": {},
            "uncertain": { "k8s.namespace::ns": { "op": "create", "key": key } },
        })
        .to_string(),
    );
    let out = common::dform()
        .args(["dev", "--world", "w.json", "plan", "p.df"])
        .env("NO_COLOR", "1")
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    let r = Run::from(out).failure();
    assert!(
        r.stderr
            .contains("k8s.namespace ns: looking up what the create whose answer was lost made\n")
            && r.stderr.contains(
                "find what the create of k8s.namespace[\"ns\"] made: no cluster (DFORM_K8S_OFFLINE \
             is set)"
            ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains(key), "{}", r.stderr);
}
