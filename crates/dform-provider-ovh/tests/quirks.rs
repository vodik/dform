//! What the live OVH API was seen to do that the provider must live with
//! (docs/providers/ovh.md, "API behaviour observed"), played by the fake
//! from its recordings (`fake::cassette`): a private port names its
//! network by the OVH id, not the OpenStack id it was attached by; a call
//! that takes no body refuses one; an instance's `ipAddresses` lags an
//! attach. Each first as the fake answers it, then as the provider copes.

use dform_core::plugin::backend::{self, Call, Handler, Reply};
use dform_core::plugin::{pb, wire};
use dform_provider_ovh::api::Client;
use dform_provider_ovh::config;
use dform_provider_ovh::fake::{self, Server, cassette};
use dform_provider_ovh::ovh::Ovh;
use serde_json::{Value as Json, json};

fn lookup(env: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
}

/// A server whose objects are ready at once, and a client of it.
fn world() -> (Server, Client) {
    let server = Server::start();
    server.build_polls(0);
    let env = lookup(server.env());
    let client = Client::new(config::resolve(Some(&server.endpoint), &env, &[]).unwrap());
    (server, client)
}

/// The provider, configured for `server`'s project.
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

/// Private network `lab` (VLAN 42) in BHS5 with a subnet there: its id
/// (`pn-…`) and its OpenStack id in BHS5.
fn lab(c: &Client) -> (String, String) {
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
    let os = net["regions"][0]["openstackId"]
        .as_str()
        .unwrap()
        .to_string();
    (id, os)
}

/// An Apply of `op` on `typ` `remote` with `config`.
fn apply(ovh: &Ovh, op: pb::Op, typ: &str, remote: &str, config: &Json) -> pb::ApplyResponse {
    let r = pb::ApplyRequest {
        op: op as i32,
        r#type: typ.into(),
        name: "x".into(),
        remote: remote.into(),
        config: Some(wire::doc(config)),
        idempotency_key: format!("key-{typ}-{remote}"),
        ..Default::default()
    };
    match ovh.handle(Call::Apply(r), &backend::silent) {
        Ok(Reply::Apply(a)) => a,
        other => panic!("apply {typ}: {other:?}"),
    }
}

/// The private ports of `instance`, as GET /interface answers them.
fn private_ports(c: &Client, instance: &str) -> Vec<Json> {
    let all = c
        .get(&project(&format!("/instance/{instance}/interface")))
        .unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "private")
        .cloned()
        .collect()
}

/// Attached by its OpenStack id, a private network is named by its OVH
/// id (`pn-…`) on the port, in the attach's answer and in every list of
/// the instance's interfaces after; an attach repeated makes another
/// port, as on the lab's instance.
#[test]
fn a_port_names_its_network_by_the_ovh_id_not_the_openstack_id_it_was_attached_by() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    assert!(id.starts_with("pn-"), "{id}");
    assert_ne!(id, os);
    let vm = server.add_instance("vm", "BHS5");
    let path = project(&format!("/instance/{vm}/interface"));
    let port = c.post(&path, Some(&json!({"networkId": os}))).unwrap();
    assert_eq!(port["networkId"], json!(id));
    let ports = private_ports(&c, &vm);
    assert_eq!(ports.len(), 1, "{ports:?}");
    assert_eq!(ports[0]["networkId"], json!(id));
    // Each field the recording has, no other.
    let keys = |o: &Json| o.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&ports[0]), keys(&cassette::get("interface").answer[0]));
    c.post(&path, Some(&json!({"networkId": os}))).unwrap();
    let ports = private_ports(&c, &vm);
    assert_eq!(ports.len(), 2, "{ports:?}");
    assert_ne!(ports[0]["id"], ports[1]["id"]);
    assert_ne!(ports[0]["fixedIps"], ports[1]["fixedIps"]);
}

/// The provider reads an instance's networks from its ports by the
/// `pn-…` id they answer, so an Update that attached one reads it back
/// and a second Update attaches nothing.
#[test]
fn the_provider_reads_a_port_by_the_ovh_id_and_attaches_once() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let config = json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
                        "image": "Ubuntu 24.04", "networks": [id]});
    let a = apply(&ovh, pb::Op::Update, "ovh.instance", &vm, &config);
    let attrs = wire::from_doc(a.attrs.as_ref().unwrap()).unwrap();
    assert_eq!(attrs["networks"], json!([id]));
    let (attrs, _) = ovh.read("ovh.instance", &vm, "vm", None).unwrap().unwrap();
    assert_eq!(attrs["networks"], json!([id]));
    let a = apply(&ovh, pb::Op::Update, "ovh.instance", &vm, &config);
    assert!(a.notes.is_empty(), "{:?}", a.notes);
    assert_eq!(private_ports(&c, &vm).len(), 1);
}

/// A call the API takes no body for refuses one, `{}` included, with the
/// API's own 400; without one it is answered.
#[test]
fn a_call_that_takes_no_body_refuses_one_with_the_apis_400() {
    let (server, c) = world();
    let user = server.add_user("backup");
    let access = server.s3_credentials(user)[0].0.clone();
    let (status, refusal) = cassette::replay("input-body");
    let calls = [
        project(&format!("/user/{user}/s3Credentials")),
        project(&format!("/user/{user}/s3Credentials/{access}/secret")),
        "/domain/zone/example.com/refresh".to_string(),
    ];
    for path in &calls {
        let e = c.post(path, Some(&json!({}))).unwrap_err();
        assert_eq!(e.status(), Some(status), "{e}");
        assert!(
            e.to_string()
                .ends_with(refusal["message"].as_str().unwrap()),
            "{e}"
        );
        c.post(path, None).unwrap();
    }
}

/// The provider sends no body where the API takes none: a user's S3
/// credential made, its secret revealed, a zone refreshed after a
/// record is written.
#[test]
fn the_provider_sends_no_body_where_the_api_takes_none() {
    let (server, _) = world();
    let ovh = provider(&server);
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.cloud_project_user",
        "",
        &json!({"description": "backup", "roles": ["objectstore_operator"]}),
    );
    let reveal = pb::RevealRequest {
        held: Some(pb::Held {
            r#type: "ovh.cloud_project_user".into(),
            remote: made.remote.clone(),
            path: "s3_secret_key".into(),
            ..Default::default()
        }),
        lease: "lease".into(),
    };
    match ovh.handle(Call::Reveal(reveal), &backend::silent) {
        Ok(Reply::Reveal(r)) => assert!(!r.value.is_empty()),
        other => panic!("reveal: {other:?}"),
    }
    let record = apply(
        &ovh,
        pb::Op::Create,
        "ovh.domain_record",
        "",
        &json!({"zone": "example.com", "subdomain": "www", "type": "A",
                "target": "192.0.2.1"}),
    );
    assert!(record.notes.is_empty(), "{:?}", record.notes);
    let bodyless: Vec<_> = server
        .seen()
        .into_iter()
        .filter(|s| {
            s.method == "POST"
                && (s.path.ends_with("/s3Credentials")
                    || s.path.ends_with("/secret")
                    || s.path.ends_with("/refresh"))
        })
        .collect();
    assert_eq!(bodyless.len(), 3, "{bodyless:?}");
    for s in bodyless {
        assert_eq!(s.body, Json::Null, "{} {} sent a body", s.method, s.path);
    }
}

/// A port attached is in GET /interface at once, with its address; the
/// instance's `ipAddresses` has the address only some reads later. The
/// provider's networks and `private_ips` come from the ports, so they are
/// right at once.
#[test]
fn ip_addresses_lag_an_attach_and_the_ports_do_not() {
    let (server, c) = world();
    server.ip_lag(3);
    let (id, os) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let instance = || c.get(&project(&format!("/instance/{vm}"))).unwrap();
    let private = |o: &Json| {
        o["ipAddresses"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["type"] == "private")
            .count()
    };
    c.post(
        &project(&format!("/instance/{vm}/interface")),
        Some(&json!({"networkId": os})),
    )
    .unwrap();
    let ip = private_ports(&c, &vm)[0]["fixedIps"][0]["ip"].clone();
    assert!(ip.is_string(), "{ip}");
    let (attrs, computed) = provider(&server)
        .read("ovh.instance", &vm, "vm", None)
        .unwrap()
        .unwrap();
    assert_eq!(attrs["networks"], json!([id]));
    assert_eq!(computed["private_ips"], json!({id.clone(): ip}));
    // The read was the first of the three that lack it.
    assert_eq!(private(&instance()), 0);
    assert_eq!(private(&instance()), 0);
    assert_eq!(private(&instance()), 1);
}

// An attach repeated makes another port, and the API marks none as the
// instance's: the provider keeps the port it attached under the
// instance's computed values (`ports[NET].id`), and dform tells its Read
// and Apply what it kept (their `prior`).

/// A Read of `vm`, told `prior`: its computed values.
fn read(ovh: &Ovh, vm: &str, prior: Option<&Json>) -> Json {
    let r = pb::ReadRequest {
        r#type: "ovh.instance".into(),
        remote: vm.into(),
        name: "vm".into(),
        prior: prior.map(wire::doc),
    };
    match ovh.handle(Call::Read(r), &backend::silent) {
        Ok(Reply::Read(r)) if r.found => wire::from_doc(r.computed.as_ref().unwrap()).unwrap(),
        other => panic!("read: {other:?}"),
    }
}

/// An Update of `vm` with `config`, told `prior`: its computed values.
fn update(ovh: &Ovh, vm: &str, config: &Json, prior: Option<&Json>) -> Json {
    let r = pb::ApplyRequest {
        op: pb::Op::Update as i32,
        r#type: "ovh.instance".into(),
        name: "vm".into(),
        remote: vm.into(),
        config: Some(wire::doc(config)),
        prior: prior.map(wire::doc),
        ..Default::default()
    };
    match ovh.handle(Call::Apply(r), &backend::silent) {
        Ok(Reply::Apply(a)) => wire::from_doc(a.computed.as_ref().unwrap()).unwrap(),
        other => panic!("update: {other:?}"),
    }
}

/// The port the provider attached is its own: the Update answers it by
/// id, and a Read told so finds it by that id among the ports on the
/// network, its address the instance's there; told nothing, the Read
/// answers as it did before ports were kept, with none of its own.
#[test]
fn an_attach_keeps_its_port_and_a_read_finds_it_by_id() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let config = json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
                        "image": "Ubuntu 24.04", "networks": [id]});
    let made = update(&ovh, &vm, &config, None);
    let ports = private_ports(&c, &vm);
    let (own, ip) = (&ports[0]["id"], &ports[0]["fixedIps"][0]["ip"]);
    assert_eq!(made["ports"], json!({id.clone(): {"id": own, "ipv4": ip}}));
    // Another port on the network (the attach loop's), listed after.
    c.post(
        &project(&format!("/instance/{vm}/interface")),
        Some(&json!({"networkId": os})),
    )
    .unwrap();
    let other = private_ports(&c, &vm)[1].clone();
    let (theirs, their_ip) = (&other["id"], &other["fixedIps"][0]["ip"]);
    let again = read(&ovh, &vm, Some(&made));
    assert_eq!(again["ports"], made["ports"]);
    assert_eq!(again["private_ips"], json!({id.clone(): ip}));
    // Told the second is its own, the second it is: by id, not by place.
    let second = json!({"ports": {id.clone(): {"id": theirs}}});
    let read2 = read(&ovh, &vm, Some(&second));
    assert_eq!(
        read2["ports"],
        json!({id.clone(): {"id": theirs, "ipv4": their_ip}})
    );
    assert_eq!(read2["private_ips"], json!({id.clone(): their_ip}));
    // Told nothing: as before, the first port's address, and no port its own.
    let bare = read(&ovh, &vm, None);
    assert_eq!(bare["ports"], json!({}));
    let mut without = again.clone();
    without["ports"] = json!({});
    assert_eq!(bare, without);
}

/// A port the prior remembers that is gone (detached outside dform) is
/// absent from the Read, never carried over: the network is still the
/// instance's while another port is on it, its address that port's.
#[test]
fn a_remembered_port_that_is_gone_reads_as_absent() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let config = json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
                        "image": "Ubuntu 24.04", "networks": [id]});
    let made = update(&ovh, &vm, &config, None);
    let path = project(&format!("/instance/{vm}/interface"));
    c.post(&path, Some(&json!({"networkId": os}))).unwrap();
    let own = made["ports"][&id]["id"].as_str().unwrap().to_string();
    c.delete(&format!("{path}/{own}")).unwrap();
    let left = private_ports(&c, &vm);
    assert_eq!(left.len(), 1, "{left:?}");
    let now = read(&ovh, &vm, Some(&made));
    assert_eq!(now["ports"], json!({}));
    assert_eq!(
        now["private_ips"],
        json!({id.clone(): left[0]["fixedIps"][0]["ip"]})
    );
    let (attrs, _) = ovh
        .read("ovh.instance", &vm, "vm", Some(&made))
        .unwrap()
        .unwrap();
    assert_eq!(attrs["networks"], json!([id]));
}

/// An Update told what the instance owns answers it again beside what it
/// attaches: a rename keeps the port, where an Update told nothing
/// answers none.
#[test]
fn an_update_told_its_ports_keeps_them() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let config = json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
                        "image": "Ubuntu 24.04", "networks": [id]});
    let made = update(&ovh, &vm, &config, None);
    let mut renamed = config.clone();
    renamed["name"] = json!("vm-2");
    assert_eq!(
        update(&ovh, &vm, &renamed, Some(&made))["ports"],
        made["ports"]
    );
    assert_eq!(update(&ovh, &vm, &renamed, None)["ports"], json!({}));
}

/// An instance made on a network owns the port it was made with.
#[test]
fn an_instance_made_on_a_network_owns_its_port() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let ovh = provider(&server);
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.instance",
        "",
        &json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
                "image": "Ubuntu 24.04", "networks": [id]}),
    );
    let computed = wire::from_doc(made.computed.as_ref().unwrap()).unwrap();
    let port = &private_ports(&c, &made.remote)[0];
    assert_eq!(
        computed["ports"],
        json!({id.clone(): {"id": port["id"], "ipv4": port["fixedIps"][0]["ip"]}})
    );
}
