//! What the live OVH API was seen to do that the provider must live with
//! (docs/providers/ovh.md, "API behaviour observed"), played by the fake
//! from its recordings (`fake::cassette`): a private port names its
//! network by the OVH id, not the OpenStack id it was attached by; a call
//! that takes no body refuses one; an instance's `ipAddresses` lags an
//! attach. Each first as the fake answers it, then as the provider copes.
//! Last, a port made with the address the program asks
//! (`networks = [{ network, ip }]`), at the fake and through the provider.

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
    assert!(
        !a.notes.iter().any(|n| n.ends_with("is attached")),
        "{:?}",
        a.notes
    );
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

/// The provider sends no body where the API takes none: an S3
/// credential made, its secret revealed, a zone refreshed after a record
/// is written.
#[test]
fn the_provider_sends_no_body_where_the_api_takes_none() {
    let (server, _) = world();
    let ovh = provider(&server);
    let user = server.add_user("backup");
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.s3_credential",
        "",
        &json!({"user": user.to_string()}),
    );
    let reveal = pb::RevealRequest {
        held: Some(pb::Held {
            r#type: "ovh.s3_credential".into(),
            remote: made.remote.clone(),
            path: "secret".into(),
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
/// provider's networks, `private_ips` and `private_ip` come from the
/// ports, so they are right at once.
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
    assert_eq!(computed["private_ip"], ip);
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
/// attaches: a rename keeps the port; an Update told nothing takes the
/// one port on the network as its own.
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
    assert_eq!(update(&ovh, &vm, &renamed, None)["ports"], made["ports"]);
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

// One port per network, converged against the ports as they are
// (docs/providers/ovh.md, "An instance's networks"): the others beside
// its own detached only when its own is known, the one port taken as its
// own when none is, and several with none known refused.

/// An Update of `vm`, told `prior`: the refusal.
fn update_refused(ovh: &Ovh, vm: &str, config: &Json, prior: Option<&Json>) -> String {
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
        Err(backend::CallError::Refused(m)) => m,
        other => panic!("update not refused: {other:?}"),
    }
}

/// An Update of `vm` with `config`, told `prior`: its computed values
/// and its notes.
fn update_noted(ovh: &Ovh, vm: &str, config: &Json, prior: Option<&Json>) -> (Json, Vec<String>) {
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
        Ok(Reply::Apply(a)) => (
            wire::from_doc(a.computed.as_ref().unwrap()).unwrap(),
            a.notes,
        ),
        other => panic!("update: {other:?}"),
    }
}

/// The lab's state: instance `vm` with three ports on network `lab`, as
/// the attach loop left it. The network's id, and the instance's.
fn three_ports(server: &Server, c: &Client) -> (String, String) {
    let (id, os) = lab(c);
    let vm = server.add_instance("vm", "BHS5");
    let path = project(&format!("/instance/{vm}/interface"));
    for _ in 0..3 {
        c.post(&path, Some(&json!({"networkId": os}))).unwrap();
    }
    (id, vm)
}

fn on_lab(id: &str) -> Json {
    json!({"name": "vm", "region": "BHS5", "flavor": "b2-7",
           "image": "Ubuntu 24.04", "networks": [id]})
}

/// The instance's own port known (the second of the three): an Update
/// detaches the two others, each said, and keeps its own and its
/// address; the next Update changes nothing.
#[test]
fn the_ports_beside_its_own_are_detached() {
    let (server, c) = world();
    let (id, vm) = three_ports(&server, &c);
    let ports = private_ports(&c, &vm);
    let own = ports[1]["id"].as_str().unwrap().to_string();
    let ip = ports[1]["fixedIps"][0]["ip"].clone();
    let ovh = provider(&server);
    let prior = json!({"ports": {id.clone(): {"id": own}}});
    let (computed, notes) = update_noted(&ovh, &vm, &on_lab(&id), Some(&prior));
    let left = private_ports(&c, &vm);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], json!(own));
    assert_eq!(
        computed["ports"],
        json!({id.clone(): {"id": own, "ipv4": ip}})
    );
    assert_eq!(computed["private_ips"], json!({id.clone(): ip}));
    let said: Vec<_> = notes.iter().filter(|n| n.contains("detached")).collect();
    assert_eq!(said.len(), 2, "{notes:?}");
    assert!(
        said[0].ends_with(&format!("a second port beside its own ({own})")),
        "{said:?}"
    );
    let (_, notes) = update_noted(&ovh, &vm, &on_lab(&id), Some(&computed));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(private_ports(&c, &vm).len(), 1);
}

/// None of the three known as its own (state kept before ports were, or
/// an instance adopted): the Update is refused naming each port and its
/// address, and nothing changes, the rename it also asked included.
#[test]
fn several_ports_none_known_refuse_and_change_nothing() {
    let (server, c) = world();
    let (id, vm) = three_ports(&server, &c);
    let ports = private_ports(&c, &vm);
    let ovh = provider(&server);
    let mut renamed = on_lab(&id);
    renamed["name"] = json!("vm-2");
    let before = server.calls().len();
    let m = update_refused(&ovh, &vm, &renamed, None);
    let named: Vec<String> = ports
        .iter()
        .map(|p| {
            format!(
                "{} ({})",
                p["id"].as_str().unwrap(),
                p["fixedIps"][0]["ip"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        m.contains(&format!(
            "network {id} has 3 ports and none is known as its own: {}",
            named.join(", ")
        )),
        "{m}"
    );
    assert!(
        m.contains(&format!(
            "`DELETE /cloud/project/{}/instance/{vm}/interface/PORT`",
            fake::PROJECT
        )),
        "{m}"
    );
    let changed: Vec<String> = server.calls()[before..]
        .iter()
        .filter(|c| !c.starts_with("GET "))
        .cloned()
        .collect();
    assert!(changed.is_empty(), "{changed:?}");
    assert_eq!(server.instances()[0]["name"], "vm");
    // A prior whose port is gone is none known.
    let gone = json!({"ports": {id.clone(): {"id": "if-gone"}}});
    update_refused(&ovh, &vm, &on_lab(&id), Some(&gone));
    assert_eq!(private_ports(&c, &vm).len(), 3);
    // A person detaches two; the one left is taken as its own.
    for p in &ports[1..] {
        c.delete(&project(&format!(
            "/instance/{vm}/interface/{}",
            p["id"].as_str().unwrap()
        )))
        .unwrap();
    }
    let (computed, notes) = update_noted(&ovh, &vm, &on_lab(&id), None);
    assert_eq!(computed["ports"][&id]["id"], ports[0]["id"]);
    assert!(
        notes.iter().any(|n| n.contains("is taken as its own")),
        "{notes:?}"
    );
}

/// A network left: its own port is detached and the one that is not its
/// own is left, said; with none known, its one port is detached.
#[test]
fn a_network_left_detaches_its_own_port() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let made = update(&ovh, &vm, &on_lab(&id), None);
    let own = made["ports"][&id]["id"].clone();
    let path = project(&format!("/instance/{vm}/interface"));
    c.post(&path, Some(&json!({"networkId": os}))).unwrap();
    let theirs = private_ports(&c, &vm)[1]["id"].clone();
    let off = json!({"name": "vm", "region": "BHS5", "flavor": "b2-7", "image": "Ubuntu 24.04"});
    let (_, notes) = update_noted(&ovh, &vm, &off, Some(&made));
    let left = private_ports(&c, &vm);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], theirs);
    assert!(
        notes.iter().any(|n| n.contains(&format!(
            "its interface {} on network {id} is detached",
            own.as_str().unwrap()
        ))),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.ends_with("is not its own, and is left")),
        "{notes:?}"
    );
    update_noted(&ovh, &vm, &off, None);
    assert!(private_ports(&c, &vm).is_empty());
}

/// A Create sent again (its answer lost) finds the instance it made and
/// owns the port it was made with on each network, as the Create would
/// have answered: one attached beside it since is detached, where told
/// nothing the two would be refused.
#[test]
fn a_create_sent_again_owns_the_port_it_was_made_with() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let ovh = provider(&server);
    let made = apply(&ovh, pb::Op::Create, "ovh.instance", "", &on_lab(&id));
    let vm = made.remote.clone();
    let first = private_ports(&c, &vm)[0]["id"].clone();
    c.post(
        &project(&format!("/instance/{vm}/interface")),
        Some(&json!({"networkId": os})),
    )
    .unwrap();
    let again = apply(&ovh, pb::Op::Create, "ovh.instance", "", &on_lab(&id));
    assert_eq!(again.remote, vm);
    let computed = wire::from_doc(again.computed.as_ref().unwrap()).unwrap();
    assert_eq!(computed["ports"][&id]["id"], first);
    let left = private_ports(&c, &vm);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], first);
    assert_eq!(server.instances().len(), 1);
}

// A fixed private address (`networks = [{ network = lab, ip = .. }]`):
// the create's `networks[].ip` and an attach's `ip` give the port that
// address, one of a subnet's hosts of the network in the region and no
// other port's.

/// `lab` with `ip` asked, as a program writes it.
fn at(id: &str, ip: &str) -> Json {
    json!({"name": "vm", "region": "BHS5", "flavor": "b2-7", "image": "Ubuntu 24.04",
           "networks": [{"network": id, "ip": ip}]})
}

/// The fake makes a port with the address asked of it, at the create and
/// at an attach; refuses one in no subnet of the network in the region
/// (or the subnet's gateway) with a 400, and one another port has with a
/// 409.
#[test]
fn the_fake_makes_a_port_with_the_address_asked() {
    let (server, c) = world();
    let (_, os) = lab(&c);
    let create = |name: &str, ip: &str| {
        c.post(
            &project("/instance"),
            Some(&json!({"name": name, "region": "BHS5",
                         "flavorId": "flavor-b2-7-BHS5", "imageId": "image-ubuntu-24.04-BHS5",
                         "networks": [{"networkId": "ext-BHS5"}, {"networkId": os, "ip": ip}]})),
        )
    };
    let vm = create("a", "10.0.0.5").unwrap();
    let vm = vm["id"].as_str().unwrap();
    let port = &private_ports(&c, vm)[0];
    assert_eq!(port["fixedIps"][0]["ip"], "10.0.0.5");
    let o = c.get(&project(&format!("/instance/{vm}"))).unwrap();
    assert!(
        o["ipAddresses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ip"] == "10.0.0.5"),
        "{o}"
    );
    let taken = create("b", "10.0.0.5").unwrap_err();
    assert_eq!(taken.status(), Some(409), "{taken}");
    assert!(
        taken.to_string().contains("10.0.0.5 is already allocated"),
        "{taken}"
    );
    for outside in ["10.9.0.5", "10.0.0.1", "10.0.0.255"] {
        let e = create("b", outside).unwrap_err();
        assert_eq!(e.status(), Some(400), "{e}");
        assert!(e.to_string().contains("is in no subnet of network"), "{e}");
    }
    let path = project(&format!("/instance/{vm}/interface"));
    let attached = c
        .post(&path, Some(&json!({"networkId": os, "ip": "10.0.0.6"})))
        .unwrap();
    assert_eq!(attached["fixedIps"][0]["ip"], "10.0.0.6");
    let again = c.post(&path, Some(&json!({"networkId": os, "ip": "10.0.0.6"})));
    assert_eq!(again.unwrap_err().status(), Some(409));
    assert_eq!(server.instances().len(), 1);
}

/// An instance made with an address asked on a network sends it
/// (`networks[].ip`), has it as its address there, owns the port as one
/// made with it (`ports[NET].fixed`), and reads back as the program
/// writes it: told its prior, a Read answers `{network, ip}`.
#[test]
fn an_instance_made_with_an_address_has_it_and_reads_it_back() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let ovh = provider(&server);
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.instance",
        "",
        &at(&id, "10.0.0.50"),
    );
    let sent = server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/instance"))
        .unwrap();
    assert_eq!(
        sent.body["networks"],
        json!([{"networkId": "ext-BHS5"}, {"networkId": os, "ip": "10.0.0.50"}])
    );
    let attrs = wire::from_doc(made.attrs.as_ref().unwrap()).unwrap();
    let computed = wire::from_doc(made.computed.as_ref().unwrap()).unwrap();
    assert_eq!(
        attrs["networks"],
        json!([{"network": id, "ip": "10.0.0.50"}])
    );
    assert_eq!(computed["private_ips"], json!({id.clone(): "10.0.0.50"}));
    assert_eq!(computed["private_ip"], "10.0.0.50");
    let port = &private_ports(&c, &made.remote)[0];
    assert_eq!(
        computed["ports"],
        json!({id.clone(): {"id": port["id"], "ipv4": "10.0.0.50", "fixed": true}})
    );
    let (again, read) = ovh
        .read("ovh.instance", &made.remote, "vm", Some(&computed))
        .unwrap()
        .unwrap();
    assert_eq!(again["networks"], attrs["networks"]);
    assert_eq!(read, computed);
    let (plain, _) = ovh
        .read("ovh.instance", &made.remote, "vm", None)
        .unwrap()
        .unwrap();
    assert_eq!(plain["networks"], json!([id]));
}

/// The plan of `doc` for instance `name`: its error.
fn plan_refused(ovh: &Ovh, name: &str, prior: Option<&Json>, doc: &Json) -> String {
    format!(
        "{:#}",
        ovh.plan("ovh.instance", name, "", prior, Some(doc))
            .unwrap_err()
    )
}

/// Plan refuses an address that is no host of a subnet of the network
/// in the instance's region, naming the subnets the network has; its
/// gateway too; and one that is no IPv4 address. A network the plan
/// makes, or one with no subnet in the region yet, is judged at Apply,
/// where the create refuses it the same way.
#[test]
fn an_address_outside_the_networks_subnets_is_refused_at_plan() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let ovh = provider(&server);
    let outside = plan_refused(&ovh, "vm", None, &at(&id, "10.9.0.5"));
    assert_eq!(
        outside,
        format!(
            "plan ovh.instance[\"vm\"]: networks: 10.9.0.5 is no host of a subnet of network \
             {id} in BHS5 (nor its gateway): its subnets are 10.0.0.0/24 in BHS5"
        )
    );
    let gateway = plan_refused(&ovh, "vm", None, &at(&id, "10.0.0.1"));
    assert!(gateway.contains("10.0.0.1 is no host"), "{gateway}");
    let text = plan_refused(&ovh, "vm", None, &at(&id, "10.0.0.300"));
    assert!(
        text.ends_with("networks: ip \"10.0.0.300\" is not an IPv4 address"),
        "{text}"
    );
    assert!(
        ovh.plan("ovh.instance", "vm", "", None, Some(&at(&id, "10.0.0.5")))
            .is_ok()
    );
    let mut pending = at(&id, "10.9.0.5");
    pending["networks"][0]["network"] = json!({"$null": "ovh.network/lab#id"});
    assert!(
        ovh.plan("ovh.instance", "vm", "", None, Some(&pending))
            .is_ok()
    );
    let refused = apply_refused(&ovh, &at(&id, "10.9.0.5"));
    assert!(
        refused.ends_with("its subnets are 10.0.0.0/24 in BHS5"),
        "{refused}"
    );
    assert!(server.instances().is_empty());
}

/// A Create of `config`: the refusal.
fn apply_refused(ovh: &Ovh, config: &Json) -> String {
    let r = pb::ApplyRequest {
        op: pb::Op::Create as i32,
        r#type: "ovh.instance".into(),
        name: "vm".into(),
        config: Some(wire::doc(config)),
        idempotency_key: "refused".into(),
        ..Default::default()
    };
    match ovh.handle(Call::Apply(r), &backend::silent) {
        Err(backend::CallError::Refused(m)) => m,
        other => panic!("create not refused: {other:?}"),
    }
}

/// Two instances asking one address on one network are refused at Plan,
/// the second naming the first; an instance planned again with another
/// address frees the one it asked, and one deleted every one.
#[test]
fn two_instances_asking_one_address_are_refused_at_plan() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let ovh = provider(&server);
    let mut b = at(&id, "10.0.0.5");
    b["name"] = json!("b");
    assert!(
        ovh.plan("ovh.instance", "a", "", None, Some(&at(&id, "10.0.0.5")))
            .is_ok()
    );
    assert_eq!(
        plan_refused(&ovh, "b", None, &b),
        format!(
            "plan ovh.instance[\"b\"]: networks: 10.0.0.5 on network {id} is asked by both \
             ovh.instance a and ovh.instance b, and an address is one port's: give each instance \
             its own"
        )
    );
    // The same instance planned again is no second asker.
    assert!(
        ovh.plan("ovh.instance", "a", "", None, Some(&at(&id, "10.0.0.5")))
            .is_ok()
    );
    assert!(
        ovh.plan("ovh.instance", "a", "", None, Some(&at(&id, "10.0.0.6")))
            .is_ok()
    );
    assert!(ovh.plan("ovh.instance", "b", "", None, Some(&b)).is_ok());
    assert!(
        ovh.plan("ovh.instance", "a", "", None, Some(&at(&id, "10.0.0.5")))
            .is_err()
    );
    ovh.plan("ovh.instance", "b", "", Some(&b), None).unwrap();
    assert!(
        ovh.plan("ovh.instance", "a", "", None, Some(&at(&id, "10.0.0.5")))
            .is_ok()
    );
}

/// An address asked anew on a network the instance is on replaces it,
/// and its plan says why: one changed, or one asked where DHCP
/// gave it one. The same address plans clean; one no longer asked
/// changes in place (the port keeps its address).
#[test]
fn an_address_asked_anew_replaces_the_instance() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let ovh = provider(&server);
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.instance",
        "",
        &at(&id, "10.0.0.50"),
    );
    let attrs = wire::from_doc(made.attrs.as_ref().unwrap()).unwrap();
    let plan = |doc: &Json| {
        ovh.plan("ovh.instance", "vm", "", Some(&attrs), Some(doc))
            .unwrap()
    };
    let same = plan(&at(&id, "10.0.0.50"));
    assert!(same.changes.is_empty() && !same.replaces, "{same:?}");
    let moved = plan(&at(&id, "10.0.0.51"));
    assert!(moved.replaces, "{moved:?}");
    assert_eq!(
        moved.reason,
        Some(format!(
            "its address on network {id} changes from 10.0.0.50 to 10.0.0.51; no API call \
             changes a port's address"
        ))
    );
    let mut dhcp = at(&id, "10.0.0.50");
    dhcp["networks"] = json!([id]);
    let dropped = plan(&dhcp);
    assert!(
        !dropped.changes.is_empty() && !dropped.replaces,
        "{dropped:?}"
    );
    let asked = ovh
        .plan(
            "ovh.instance",
            "vm",
            "",
            Some(&dhcp),
            Some(&at(&id, "10.0.0.50")),
        )
        .unwrap();
    assert!(asked.replaces, "{asked:?}");
    assert_eq!(
        asked.reason,
        Some(format!(
            "its address on network {id} is asked (10.0.0.50), and was DHCP's; no API call \
             changes a port's address"
        ))
    );
    let r = pb::ApplyRequest {
        op: pb::Op::Replace as i32,
        r#type: "ovh.instance".into(),
        name: "vm".into(),
        remote: made.remote.clone(),
        config: Some(wire::doc(&at(&id, "10.0.0.51"))),
        prior: made.computed.clone(),
        idempotency_key: "replace".into(),
        ..Default::default()
    };
    let replaced = match ovh.handle(Call::Apply(r), &backend::silent) {
        Ok(Reply::Apply(a)) => a,
        other => panic!("replace: {other:?}"),
    };
    // The plan said why; the Apply does not say it again.
    assert!(replaced.notes.is_empty(), "{:?}", replaced.notes);
    let computed = wire::from_doc(replaced.computed.as_ref().unwrap()).unwrap();
    assert_eq!(computed["private_ips"][&id], "10.0.0.51");
}

/// An Update that adds a network with an address asked attaches the
/// port with it (`ip` beside `networkId`) and owns it as made with it;
/// asked no longer, the port is kept, its address no longer the
/// program's. An Update whose kept port has another address than the
/// one asked is refused before anything changes.
#[test]
fn an_update_attaches_with_the_address_asked() {
    let (server, c) = world();
    let (id, os) = lab(&c);
    let vm = server.add_instance("vm", "BHS5");
    let ovh = provider(&server);
    let (made, notes) = update_noted(&ovh, &vm, &at(&id, "10.0.0.60"), None);
    let attach = server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/interface"))
        .unwrap();
    assert_eq!(attach.body, json!({"networkId": os, "ip": "10.0.0.60"}));
    assert!(
        notes
            .iter()
            .any(|n| n.ends_with("is attached, with address 10.0.0.60")),
        "{notes:?}"
    );
    assert_eq!(made["ports"][&id]["fixed"], true);
    assert_eq!(made["private_ips"][&id], "10.0.0.60");
    let mut dhcp = at(&id, "10.0.0.60");
    dhcp["networks"] = json!([id]);
    let (kept, _) = update_noted(&ovh, &vm, &dhcp, Some(&made));
    assert_eq!(
        kept["ports"][&id],
        json!({"id": made["ports"][&id]["id"], "ipv4": "10.0.0.60"})
    );
    let refused = update_refused(&ovh, &vm, &at(&id, "10.0.0.61"), Some(&made));
    assert!(
        refused.contains(
            "has 10.0.0.60, not 10.0.0.61 as asked, and no API call changes a port's address"
        ),
        "{refused}"
    );
    assert_eq!(private_ports(&c, &vm).len(), 1);
}

/// A replacement made first that asks an address the old instance has
/// deletes the old one first, said: two ports cannot have it.
#[test]
fn a_replacement_asking_the_old_address_deletes_the_old_one_first() {
    let (server, c) = world();
    let (id, _) = lab(&c);
    let ovh = provider(&server);
    let made = apply(
        &ovh,
        pb::Op::Create,
        "ovh.instance",
        "",
        &at(&id, "10.0.0.50"),
    );
    let mut next = at(&id, "10.0.0.50");
    next["name"] = json!("vm-2");
    next["image"] = json!("Debian 13");
    let r = pb::ApplyRequest {
        op: pb::Op::Replace as i32,
        r#type: "ovh.instance".into(),
        name: "vm".into(),
        remote: made.remote.clone(),
        config: Some(wire::doc(&next)),
        create_first: true,
        idempotency_key: "replace".into(),
        ..Default::default()
    };
    let replaced = match ovh.handle(Call::Apply(r), &backend::silent) {
        Ok(Reply::Apply(a)) => a,
        other => panic!("replace: {other:?}"),
    };
    assert!(
        replaced.notes.iter().any(|n| n.ends_with(&format!(
            "the replacement asks 10.0.0.50 on network {id}, which the old instance has, so \
             the old one goes first"
        ))),
        "{:?}",
        replaced.notes
    );
    assert_eq!(server.instances().len(), 1);
    let computed = wire::from_doc(replaced.computed.as_ref().unwrap()).unwrap();
    assert_eq!(computed["private_ips"][&id], "10.0.0.50");
    assert_ne!(replaced.remote, made.remote);
}
