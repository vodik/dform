//! What the provider asks the API to read: each list it reads (a region's
//! flavors and images, the project's private networks, a network's
//! subnets) once per session and run, shared by every Read, Plan and
//! Apply of the session, and asked afresh after a write to its class of
//! endpoint; each object by its id. Counted at the fake.

use dform_core::plugin::backend::{self, Call, Reply};
use dform_core::plugin::{pb, wire};
use dform_provider_ovh::api::Client;
use dform_provider_ovh::config;
use dform_provider_ovh::fake::{self, Server};
use dform_provider_ovh::ovh::Ovh;
use serde_json::{Value as Json, json};

fn lookup(env: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
}

/// A server whose objects are ready at once, and a client of it.
fn world() -> (Server, Client) {
    let server = Server::start();
    server.build_polls(0);
    server.ip_lag(0);
    let env = lookup(server.env());
    let client = Client::new(config::resolve(Some(&server.endpoint), &env, &[]).unwrap());
    (server, client)
}

/// The provider, configured for `server`'s project: one session.
fn provider(server: &Server) -> Ovh {
    let ovh = Ovh::new();
    ovh.configure_with(
        &json!({"settings": {"endpoint": server.endpoint, "project": fake::PROJECT}}),
        &lookup(server.env()),
        &[],
    )
    .unwrap();
    ovh
}

fn project(rest: &str) -> String {
    format!("/cloud/project/{}{rest}", fake::PROJECT)
}

/// How many times `GET path` (with its query) was answered.
fn gets(server: &Server, path: &str) -> usize {
    let call = format!("GET {path}");
    server.calls().iter().filter(|c| **c == call).count()
}

/// Private network `lab` in BHS5 with a subnet there, and two instances
/// on it: the network's id and the instances'.
fn lab(server: &Server, c: &Client) -> (String, [String; 2]) {
    let net = c
        .post(
            &project("/network/private"),
            Some(&json!({"name": "lab", "vlanId": 42, "regions": ["BHS5"]})),
        )
        .unwrap();
    let id = net["id"].as_str().unwrap().to_string();
    c.post(
        &project(&format!("/network/private/{id}/subnet")),
        Some(
            &json!({"region": "BHS5", "network": "10.0.0.0/24", "dhcp": true,
                     "noGateway": false, "start": "10.0.0.10", "end": "10.0.0.200"}),
        ),
    )
    .unwrap();
    let net = c.get(&project(&format!("/network/private/{id}"))).unwrap();
    let os = net["regions"][0]["openstackId"].as_str().unwrap();
    let vms = ["a", "b"].map(|name| {
        let vm = server.add_instance(name, "BHS5");
        c.post(
            &project(&format!("/instance/{vm}/interface")),
            Some(&json!({"networkId": os})),
        )
        .unwrap();
        vm
    });
    (id, vms)
}

fn read(ovh: &Ovh, typ: &str, remote: &str) -> Json {
    let r = pb::ReadRequest {
        r#type: typ.into(),
        remote: remote.into(),
        name: "x".into(),
        ..Default::default()
    };
    match ovh.answer(Call::Read(r), &backend::silent) {
        Ok(Reply::Read(r)) if r.found => wire::from_doc(r.attrs.as_ref().unwrap()).unwrap(),
        other => panic!("read {typ} {remote}: {other:?}"),
    }
}

/// An instance on `net` asking `ip` there.
fn on(name: &str, net: &str, ip: &str) -> Json {
    json!({"name": name, "region": "BHS5", "flavor": "b2-7", "image": "Ubuntu 24.04",
           "networks": [{"network": net, "ip": ip}]})
}

/// The Reads and Plans of a session, at once as dform sends them, ask
/// each list once: the region's flavors and images, the private
/// networks, the network's subnets; again, none. A write to networks (a
/// network made) forgets the networks' lists, and only them.
#[test]
fn each_list_is_asked_once_per_run_and_again_after_a_write_to_it() {
    let (server, c) = world();
    let (net, vms) = lab(&server, &c);
    let subnet = server.subnets()[0]["id"].as_str().unwrap().to_string();
    let ovh = provider(&server);
    let from = server.calls().len();
    let session = || {
        std::thread::scope(|s| {
            for vm in &vms {
                s.spawn(|| read(&ovh, "ovh.instance", vm));
            }
            s.spawn(|| read(&ovh, "ovh.network", &net));
            s.spawn(|| read(&ovh, "ovh.subnet", &format!("{net}/{subnet}")));
            for (name, ip) in [("a", "10.0.0.20"), ("b", "10.0.0.21")] {
                let d = on(name, &net, ip);
                let ovh = &ovh;
                s.spawn(move || ovh.plan("ovh.instance", name, "", None, Some(&d)).unwrap());
            }
        })
    };
    session();
    session();
    let lists = [
        project("/flavor?region=BHS5"),
        project("/image?region=BHS5"),
        project("/network/private"),
        project(&format!("/network/private/{net}/subnet")),
    ];
    for list in &lists {
        assert_eq!(
            gets(&server, list),
            1,
            "{list}: {:?}",
            &server.calls()[from..]
        );
    }
    let made = ovh.answer(
        Call::Apply(pb::ApplyRequest {
            op: pb::Op::Create as i32,
            r#type: "ovh.network".into(),
            name: "db".into(),
            config: Some(wire::doc(&json!({"name": "db", "regions": ["BHS5"]}))),
            idempotency_key: "db".into(),
            ..Default::default()
        }),
        &backend::silent,
    );
    assert!(matches!(made, Ok(Reply::Apply(_))), "{made:?}");
    session();
    let asked: Vec<usize> = lists.iter().map(|l| gets(&server, l)).collect();
    assert_eq!(asked, [1, 1, 2, 2], "{:?}", &server.calls()[from..]);
}

/// A Read asks its object by its id: a network, a volume, an S3
/// container one GET; a subnet its network's subnets (the API has no GET
/// of one), listed once. An instance asks itself, its ports and the
/// private networks at once, the networks once per run (and, where the
/// API names its flavor and image by id alone, the region's flavors and
/// images, once).
#[test]
fn a_read_asks_its_object_by_id_and_an_instance_asks_at_once() {
    let (server, c) = world();
    let (net, [vm, _]) = lab(&server, &c);
    let subnet = server.subnets()[0]["id"].as_str().unwrap().to_string();
    let volume = server.add_volume("data", "BHS5", 10);
    server.add_container("BHS5", "backups");
    let ovh = provider(&server);
    server.slow(std::time::Duration::from_millis(50));
    let asked = |typ: &str, remote: &str| {
        let from = server.calls().len();
        read(&ovh, typ, remote);
        let mut calls = server.calls()[from..].to_vec();
        calls.sort();
        calls
    };
    let get = |rest: &str| format!("GET {}", project(rest));
    let mut first = [
        get(&format!("/instance/{vm}")),
        get(&format!("/instance/{vm}/interface")),
        get("/network/private"),
        get("/flavor?region=BHS5"),
        get("/image?region=BHS5"),
    ];
    first.sort();
    assert_eq!(asked("ovh.instance", &vm), first);
    assert_eq!(
        server.most_at_once(),
        3,
        "the instance, its ports, the networks"
    );
    assert_eq!(
        asked("ovh.instance", &vm),
        [
            get(&format!("/instance/{vm}")),
            get(&format!("/instance/{vm}/interface"))
        ]
    );
    assert_eq!(
        asked("ovh.network", &net),
        [get(&format!("/network/private/{net}"))]
    );
    assert_eq!(
        asked("ovh.subnet", &format!("{net}/{subnet}")),
        [get(&format!("/network/private/{net}/subnet"))]
    );
    assert!(asked("ovh.subnet", &format!("{net}/{subnet}")).is_empty());
    assert_eq!(
        asked("ovh.volume", &volume),
        [get(&format!("/volume/{volume}"))]
    );
    assert_eq!(
        asked("ovh.storage_container", "BHS5/backups"),
        [get("/region/BHS5/storage/backups")]
    );
}
