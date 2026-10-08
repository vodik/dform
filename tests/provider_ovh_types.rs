//! The OVH provider's Public Cloud types beyond the instance (R-157):
//! S3 containers, users with S3 credentials, volumes attached to
//! instances, private networks and their subnets, each against the fake
//! OVH API (`dform_provider_ovh::fake`). Most tests call the provider in
//! this process, as dform's protocol does (`Handler::handle`), so each
//! call's effect on the API is looked at directly; the last plans and
//! applies a program with all of them through dform.
//!
//! The provider is configured with the fake's environment and no
//! configuration file, so no real `ovh.conf` is read.

mod common;
use common::{Run, Scratch};
use dform_core::plugin::backend::{Call, CallError, Handler, Reply};
use dform_core::plugin::{pb, wire};
use dform_provider_ovh::fake::{self, Server};
use dform_provider_ovh::ovh::Ovh;
use serde_json::{Value as Json, json};
use std::sync::Mutex;

/// The provider, configured against a fake API.
struct Lab {
    server: Server,
    ovh: Ovh,
}

/// What an Apply answered, and the statuses it said on the way.
#[derive(Debug)]
struct Applied {
    remote: String,
    attrs: Json,
    computed: Json,
    said: Vec<String>,
}

impl Lab {
    fn new() -> Lab {
        let server = Server::start();
        server.build_polls(2);
        let ovh = Ovh::new();
        let env: Vec<(&str, String)> = server.env();
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        ovh.configure_with(
            &json!({"settings": {"endpoint": server.endpoint, "project": fake::PROJECT}}),
            &get,
            &[],
        )
        .unwrap();
        Lab { server, ovh }
    }

    fn call(&self, call: Call, said: &Mutex<Vec<String>>) -> Result<Reply, CallError> {
        let progress = |e: pb::Event| {
            if let Some(s) = e.status {
                said.lock().unwrap().push(s);
            }
        };
        self.ovh.handle(call, &progress)
    }

    fn apply(
        &self,
        op: pb::Op,
        typ: &str,
        name: &str,
        remote: &str,
        config: Json,
    ) -> Result<Applied, String> {
        let said = Mutex::new(Vec::new());
        let r = pb::ApplyRequest {
            op: op as i32,
            r#type: typ.into(),
            name: name.into(),
            remote: remote.into(),
            config: Some(wire::doc(&config)),
            idempotency_key: format!("{typ}/{name}/{remote}/{op:?}"),
            ..Default::default()
        };
        match self.call(Call::Apply(r), &said) {
            Ok(Reply::Apply(a)) => Ok(Applied {
                remote: a.remote,
                attrs: a
                    .attrs
                    .as_ref()
                    .map_or(Json::Null, |d| wire::from_doc(d).unwrap()),
                computed: a
                    .computed
                    .as_ref()
                    .map_or(Json::Null, |d| wire::from_doc(d).unwrap()),
                said: said.into_inner().unwrap(),
            }),
            Ok(_) => Err("not an Apply's reply".into()),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn create(&self, typ: &str, name: &str, config: Json) -> Applied {
        self.apply(pb::Op::Create, typ, name, "", config).unwrap()
    }

    fn update(&self, typ: &str, name: &str, remote: &str, config: Json) -> Applied {
        self.apply(pb::Op::Update, typ, name, remote, config)
            .unwrap()
    }

    fn delete(&self, typ: &str, name: &str, remote: &str) -> Applied {
        self.apply(pb::Op::Delete, typ, name, remote, Json::Null)
            .unwrap()
    }

    /// Read: the configured attributes and the computed ones, if found.
    fn read(&self, typ: &str, name: &str, remote: &str) -> Option<(Json, Json)> {
        let r = pb::ReadRequest {
            r#type: typ.into(),
            name: name.into(),
            remote: remote.into(),
        };
        match self.call(Call::Read(r), &Mutex::default()).unwrap() {
            Reply::Read(r) if r.found => Some((
                wire::from_doc(r.attrs.as_ref().unwrap()).unwrap(),
                wire::from_doc(r.computed.as_ref().unwrap()).unwrap(),
            )),
            _ => None,
        }
    }

    /// Plan against what Read or Apply answered (`attrs`, `computed`):
    /// the paths that change, and whether it replaces. The prior is the
    /// world's document as dform makes it: the configured attributes, and
    /// an optional computed one's value where the program sets it.
    fn plan(&self, typ: &str, prior: (&Json, &Json), desired: &Json) -> (Vec<String>, bool) {
        let mut world = prior.0.clone();
        for (path, _) in self.ovh.schema().optional_computed_of(typ) {
            if desired.get(&path).is_some()
                && world.get(&path).is_none()
                && let Some(v) = prior.1.get(&path)
            {
                world[path.as_str()] = v.clone();
            }
        }
        let (changes, replaces) = self
            .ovh
            .plan(typ, "x", "", Some(&world), Some(desired))
            .unwrap();
        (changes.into_iter().map(|c| c.path).collect(), replaces)
    }

    fn plan_err(&self, typ: &str, prior: Option<&Json>, desired: &Json) -> String {
        format!(
            "{:#}",
            self.ovh
                .plan(typ, "x", "", prior, Some(desired))
                .unwrap_err()
        )
    }

    fn calls(&self, method: &str, suffix: &str) -> usize {
        self.server
            .calls()
            .iter()
            .filter(|c| c.starts_with(&format!("{method} ")))
            .filter(|c| c.split('?').next().unwrap_or_default().ends_with(suffix))
            .count()
    }
}

const CONTAINER: &str = "ovh.storage_container";

/// A container is made in its region, read back as it was made, its
/// versioning turned on and suspended in place; a second one of the same
/// name is refused, to be adopted; one made elsewhere is read by its
/// remote id; a new name replaces it; it is deleted.
#[test]
fn a_storage_container_is_made_changed_and_deleted() {
    let lab = Lab::new();
    let user = lab.server.add_user("backup").to_string();
    let c = lab.create(
        CONTAINER,
        "logs",
        json!({"region": "BHS5", "name": "lab-logs", "owner": user}),
    );
    assert_eq!(c.remote, "BHS5/lab-logs");
    assert_eq!(c.attrs, json!({"region": "BHS5", "name": "lab-logs"}));
    assert_eq!(
        (&c.computed["versioning"], &c.computed["owner"]),
        (&json!(false), &json!(user))
    );
    assert_eq!(
        c.computed["virtual_host"],
        "lab-logs.s3.bhs5.io.cloud.ovh.net"
    );
    let (attrs, computed) = lab.read(CONTAINER, "logs", &c.remote).unwrap();
    assert_eq!((&attrs, &computed), (&c.attrs, &c.computed));
    // Nothing set that the API chose: nothing to do.
    let (paths, _) = lab.plan(CONTAINER, (&attrs, &computed), &c.attrs);
    assert!(paths.is_empty(), "{paths:?}");

    // Versioning on, then off: `enabled`, then `suspended`, in place.
    let on = json!({"region": "BHS5", "name": "lab-logs", "versioning": true, "owner": user});
    let (paths, replaces) = lab.plan(CONTAINER, (&attrs, &computed), &on);
    assert_eq!((paths, replaces), (vec!["versioning".to_string()], false));
    let u = lab.update(CONTAINER, "logs", &c.remote, on);
    assert_eq!(u.computed["versioning"], true);
    let off = json!({"region": "BHS5", "name": "lab-logs", "versioning": false});
    lab.update(CONTAINER, "logs", &c.remote, off);
    assert_eq!(
        lab.server.containers()[0]["versioning"]["status"],
        "suspended"
    );

    // The same key again, from another Create: refused, to be adopted.
    let again = lab
        .apply(
            pb::Op::Create,
            CONTAINER,
            "other",
            "",
            json!({"region": "BHS5", "name": "lab-logs"}),
        )
        .unwrap_err();
    assert!(
        again.contains("already exists with this key") && again.contains("adopt it"),
        "{again}"
    );

    // Made elsewhere: read by its remote id, as adopting it reads it.
    lab.server.add_container("ca-east-tor", "found");
    let (found, _) = lab.read(CONTAINER, "found", "ca-east-tor/found").unwrap();
    assert_eq!(found["name"], "found");

    // A new name replaces it.
    let (_, replaces) = lab.plan(
        CONTAINER,
        (&attrs, &computed),
        &json!({"region": "BHS5", "name": "lab-logs-2"}),
    );
    assert!(replaces);

    lab.delete(CONTAINER, "logs", &c.remote);
    assert!(lab.read(CONTAINER, "logs", &c.remote).is_none());
    // Gone already is deleted.
    lab.delete(CONTAINER, "logs", &c.remote);
}

const USER: &str = "ovh.cloud_project_user";

/// A reveal of `path` of the user `remote`, as the engine asks it.
fn reveal(lab: &Lab, typ: &str, remote: &str, path: &str, lease: &str) -> Result<String, String> {
    let r = pb::RevealRequest {
        held: Some(pb::Held {
            provider: "ovh".into(),
            deployment: "main".into(),
            r#type: typ.into(),
            remote: remote.into(),
            path: path.into(),
            digest: String::new(),
        }),
        lease: lease.into(),
    };
    match lab.call(Call::Reveal(r), &Mutex::default()) {
        Ok(Reply::Reveal(r)) => Ok(String::from_utf8(r.value).unwrap()),
        Ok(_) => Err("not a Reveal's reply".into()),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// A user is made with its roles, said `creating` then `ok`, and given
/// an S3 credential: its access key is computed, its secret only a label
/// (the value never leaves the API but for Reveal, with the lease). Its
/// roles change in place; a new description replaces it; the same
/// description from another Create is refused, to be adopted.
#[test]
fn a_user_has_an_s3_credential_whose_secret_is_held() {
    let lab = Lab::new();
    let doc = json!({"description": "backup", "roles": ["objectstore_operator"]});
    let u = lab.create(USER, "backup", doc.clone());
    assert_eq!(u.said, ["creating", "ok"], "{u:?}");
    let id: i64 = u.remote.parse().unwrap();
    let creds = lab.server.s3_credentials(id);
    assert_eq!(creds.len(), 1);
    let (access, secret) = &creds[0];
    assert_eq!(u.attrs, doc);
    assert_eq!(u.computed["s3_access_key"], access.as_str());
    assert_eq!(u.computed["status"], "ok");
    assert_eq!(
        u.computed["s3_secret_key"],
        json!({"$secret": "ovh.cloud_project_user/backup#s3_secret_key"})
    );
    // The secret and the password the API answered are nowhere in what
    // the provider answers.
    let answered = format!("{u:?}");
    assert!(
        !answered.contains(secret.as_str()) && !answered.contains("pw-"),
        "{answered}"
    );
    let (_, computed) = lab.read(USER, "backup", &u.remote).unwrap();
    assert_eq!(computed, u.computed);

    // Reveal: the secret, read from the API, with the lease only.
    assert_eq!(
        reveal(&lab, USER, &u.remote, "s3_secret_key", "lease-1").unwrap(),
        *secret
    );
    let no_lease = reveal(&lab, USER, &u.remote, "s3_secret_key", "").unwrap_err();
    assert!(
        no_lease.contains("without the deployment's lease"),
        "{no_lease}"
    );
    let elsewhere = reveal(&lab, USER, &u.remote, "username", "lease-1").unwrap_err();
    assert!(elsewhere.contains("holds no secret there"), "{elsewhere}");

    // Roles in place: one PUT of their ids.
    let more = json!({"description": "backup",
                      "roles": ["volume_operator", "objectstore_operator"]});
    let (paths, replaces) = lab.plan(USER, (&u.attrs, &u.computed), &more);
    assert!(!paths.is_empty() && !replaces, "{paths:?}");
    let changed = lab.update(USER, "backup", &u.remote, more);
    assert_eq!(
        changed.attrs["roles"],
        json!(["objectstore_operator", "volume_operator"])
    );
    assert_eq!(lab.calls("PUT", "/role"), 1);
    // The same roles in another order: nothing to do.
    let (paths, _) = lab.plan(
        USER,
        (&changed.attrs, &changed.computed),
        &json!({"description": "backup",
                "roles": ["volume_operator", "objectstore_operator"]}),
    );
    assert!(paths.is_empty(), "{paths:?}");
    // A new description replaces it.
    let (_, replaces) = lab.plan(
        USER,
        (&changed.attrs, &changed.computed),
        &json!({"description": "backup-2", "roles": ["objectstore_operator"]}),
    );
    assert!(replaces);

    // Another Create of the same description: refused, to be adopted.
    let again = lab
        .apply(pb::Op::Create, USER, "other", "", doc.clone())
        .unwrap_err();
    assert!(again.contains("already exists with this key"), "{again}");
    // Adopting one made elsewhere with no credential gives it one.
    let found = lab.server.add_user("found");
    lab.server.world.lock().unwrap().s3.remove(&found);
    let adopted = lab
        .apply(
            pb::Op::Adopt,
            USER,
            "found",
            &found.to_string(),
            json!({"description": "found", "roles": ["objectstore_operator"]}),
        )
        .unwrap();
    assert_eq!(lab.server.s3_credentials(found).len(), 1);
    assert!(adopted.computed["s3_access_key"].is_string());

    lab.delete(USER, "backup", &u.remote);
    assert!(lab.read(USER, "backup", &u.remote).is_none());
}

const VOLUME: &str = "ovh.volume";

/// A volume is made, said `creating` then `available`, and attached to
/// its instance (`attaching`, `in-use`): the attachment is its attribute,
/// read back as the instance's id. It grows in place and is replaced to
/// shrink; it moves to another instance (detached, attached) and is
/// detached when the program clears it; an attached volume is detached
/// before it is deleted. One made elsewhere is refused to another Create
/// of its name, to be adopted.
#[test]
fn a_volume_is_attached_moved_grown_and_deleted() {
    let lab = Lab::new();
    let one = lab.server.add_instance("one", "BHS5");
    let two = lab.server.add_instance("two", "BHS5");
    let doc = |size: i64, instance: Option<&str>| {
        let mut d = json!({"name": "data", "region": "BHS5", "size": size,
                           "type": "high-speed", "description": "lab data"});
        if let Some(i) = instance {
            d["instance"] = json!(i);
        }
        d
    };
    let v = lab.create(VOLUME, "data", doc(20, Some(&one)));
    assert_eq!(v.said, ["creating", "available", "attaching", "in-use"]);
    assert_eq!(
        v.attrs,
        json!({"name": "data", "region": "BHS5", "size": 20, "instance": one})
    );
    assert_eq!(v.computed["status"], "in-use");
    assert_eq!(v.computed["type"], "high-speed");
    assert_eq!(lab.server.volumes()[0]["attachedTo"], json!([one]));
    let (attrs, computed) = lab.read(VOLUME, "data", &v.remote).unwrap();
    assert_eq!(attrs, v.attrs);
    let read = (&attrs, &computed);
    assert_eq!(lab.plan(VOLUME, read, &doc(20, Some(&one))).0.len(), 0);

    // Bigger in place, smaller replaced, another region replaced.
    assert_eq!(
        lab.plan(VOLUME, read, &doc(50, Some(&one))),
        (vec!["size".to_string()], false)
    );
    assert!(lab.plan(VOLUME, read, &doc(10, Some(&one))).1);
    let mut moved = doc(20, Some(&one));
    moved["region"] = json!("ca-east-tor");
    assert!(lab.plan(VOLUME, read, &moved).1);
    let grown = lab.update(VOLUME, "data", &v.remote, doc(50, Some(&one)));
    assert_eq!(grown.attrs["size"], 50);
    assert_eq!(lab.calls("POST", "/upsize"), 1);

    // To the other instance: detached, then attached; then cleared.
    assert_eq!(
        lab.plan(
            VOLUME,
            (&grown.attrs, &grown.computed),
            &doc(50, Some(&two))
        ),
        (vec!["instance".to_string()], false)
    );
    let to_two = lab.update(VOLUME, "data", &v.remote, doc(50, Some(&two)));
    assert_eq!(
        to_two.said,
        ["detaching", "available", "attaching", "in-use"]
    );
    assert_eq!(to_two.attrs["instance"], two.as_str());
    let cleared = lab.update(VOLUME, "data", &v.remote, doc(50, None));
    assert_eq!(cleared.attrs.get("instance"), None);
    assert_eq!(lab.server.volumes()[0]["status"], "available");

    // Another Create of a name the region has: refused, to be adopted.
    let found = lab.server.add_volume("found", "BHS5", 10);
    let again = lab
        .apply(
            pb::Op::Create,
            VOLUME,
            "other",
            "",
            json!({"name": "found", "region": "BHS5", "size": 10}),
        )
        .unwrap_err();
    assert!(again.contains("already exists with this key"), "{again}");
    let adopted = lab
        .apply(
            pb::Op::Adopt,
            VOLUME,
            "found",
            &found,
            json!({"name": "found", "region": "BHS5", "size": 10, "instance": one}),
        )
        .unwrap();
    assert_eq!(adopted.attrs["instance"], one.as_str());

    // Deleting an attached volume detaches it first.
    let gone = lab.delete(VOLUME, "found", &found);
    assert_eq!(gone.said.first().map(String::as_str), Some("detaching"));
    assert!(lab.read(VOLUME, "found", &found).is_none());
    lab.delete(VOLUME, "data", &v.remote);
    assert!(lab.server.volumes().is_empty());
}

/// A bootable volume's image is sent as the region's image id; one the
/// region does not have is refused at plan, naming what it has.
#[test]
fn a_volume_from_an_image() {
    let lab = Lab::new();
    lab.create(
        VOLUME,
        "boot",
        json!({"name": "boot", "region": "BHS5", "size": 10, "image": "Debian 13"}),
    );
    let sent = lab
        .server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/volume"))
        .unwrap();
    assert_eq!(sent.body["imageId"], "image-debian-13-BHS5");
    let bad = lab.plan_err(
        VOLUME,
        None,
        &json!({"name": "bad", "region": "BHS5", "size": 10, "image": "Arch"}),
    );
    assert!(
        bad.contains("plan ovh.volume[\"x\"]: image: image \"Arch\" is not in region BHS5")
            && bad.contains("Debian 13"),
        "{bad}"
    );
}

const NETWORK: &str = "ovh.network";

/// A private network is made in its regions (`BUILDING`, then `ACTIVE`);
/// it is renamed and added to a region in place, and replaced to leave
/// one or to change its VLAN; the same name from another Create is
/// refused; it is deleted.
#[test]
fn a_private_network_is_made_grown_and_deleted() {
    let lab = Lab::new();
    let doc = |regions: &[&str]| json!({"name": "lab", "vlan_id": 42, "regions": regions});
    let n = lab.create(NETWORK, "lab", doc(&["BHS5"]));
    assert_eq!(n.said, ["BUILDING", "ACTIVE"]);
    assert_eq!(n.remote, "pn-1000123_42");
    assert_eq!(n.attrs, json!({"name": "lab"}));
    assert_eq!(n.computed["regions"], json!(["BHS5"]));
    assert_eq!(n.computed["vlan_id"], 42);
    assert_eq!(n.computed["regions_status"], json!({"BHS5": "ACTIVE"}));

    let both = doc(&["ca-east-tor", "BHS5"]);
    let (paths, replaces) = lab.plan(NETWORK, (&n.attrs, &n.computed), &both);
    assert!(!paths.is_empty() && !replaces, "{paths:?}");
    let grown = lab.update(NETWORK, "lab", &n.remote, both);
    assert_eq!(grown.computed["regions"], json!(["BHS5", "ca-east-tor"]));
    assert_eq!(lab.calls("POST", "/region"), 1);
    assert!(
        lab.plan(NETWORK, (&grown.attrs, &grown.computed), &doc(&["BHS5"]))
            .1
    );
    let mut vlan = doc(&["BHS5", "ca-east-tor"]);
    vlan["vlan_id"] = json!(7);
    assert!(lab.plan(NETWORK, (&grown.attrs, &grown.computed), &vlan).1);
    let mut renamed = doc(&["BHS5", "ca-east-tor"]);
    renamed["name"] = json!("lab-2");
    assert_eq!(
        lab.plan(NETWORK, (&grown.attrs, &grown.computed), &renamed),
        (vec!["name".to_string()], false)
    );

    let again = lab
        .apply(pb::Op::Create, NETWORK, "other", "", json!({"name": "lab"}))
        .unwrap_err();
    assert!(again.contains("already exists with this key"), "{again}");
    // Renamed on its id: one PUT, the same network.
    let puts = lab.calls("PUT", &grown.remote);
    let named = lab.update(NETWORK, "lab", &grown.remote, renamed);
    assert_eq!(named.remote, grown.remote);
    assert_eq!(named.attrs["name"], "lab-2");
    assert_eq!(lab.calls("PUT", &grown.remote), puts + 1);

    let gone = lab.delete(NETWORK, "lab", &n.remote);
    assert_eq!(gone.said, ["DELETING"]);
    assert!(lab.server.networks().is_empty());
}

const RECORD: &str = "ovh.domain_record";

/// A record whose Create's answer was lost is found by its lookup, its
/// content (R-195): `provider.created` answers its id for the key the
/// Create carried, and a record of another target is not taken for it.
#[test]
fn a_records_lost_create_is_found_by_its_content() {
    let lab = Lab::new();
    lab.server.add_record("example.com", "www", "A", "10.0.0.9");
    let doc = json!({"zone": "example.com", "subdomain": "www", "type": "A", "target": "10.0.0.1"});
    let r = lab.create(RECORD, "www", doc);
    let key = format!("{RECORD}/www//Create");
    assert_eq!(lab.ovh.created(&key).unwrap(), Some(r.remote));
    assert_eq!(lab.ovh.created("another key").unwrap(), None);
}

/// A project that is not on a vRack: a private network is refused at
/// plan and at apply naming the network and what is missing, never with
/// the API's text.
#[test]
fn a_private_network_needs_the_projects_vrack() {
    let lab = Lab::new();
    lab.server.no_vrack();
    let doc = json!({"name": "lab"});
    let at_plan = lab.plan_err(NETWORK, None, &doc);
    assert!(
        at_plan.starts_with("plan ovh.network[\"x\"]: project ")
            && at_plan.contains("is not on a vRack")
            && at_plan.contains("outside dform"),
        "{at_plan}"
    );
    let at_apply = lab
        .apply(pb::Op::Create, NETWORK, "lab", "", doc)
        .unwrap_err();
    assert!(
        at_apply.contains("apply ovh.network[\\\"lab\\\"]: project ")
            && at_apply.contains("is not on a vRack")
            && !at_apply.contains("Your project is not attached"),
        "{at_apply}"
    );
    assert!(lab.server.networks().is_empty());
}

const SUBNET: &str = "ovh.subnet";

/// A subnet is made in its network's region with the pool the program
/// writes, its ends the API's `start` and `end`; read back by listing the
/// network's, the pool computed; replaced by any change, a new pool too;
/// refused to another Create of its range, and deleted (its network
/// after it).
#[test]
fn a_subnet_is_made_replaced_and_deleted() {
    let lab = Lab::new();
    let net = lab.create(NETWORK, "lab", json!({"name": "lab", "regions": ["BHS5"]}));
    let doc = json!({"network": net.remote, "region": "BHS5", "range": "10.1.0.0/24",
                     "pool": "10.1.0.10..=10.1.0.100", "dhcp": true});
    let sub = lab.create(SUBNET, "lab", doc.clone());
    let (network, _) = sub.remote.split_once('/').unwrap();
    assert_eq!(network, net.remote);
    let want = json!({"network": net.remote, "region": "BHS5", "range": "10.1.0.0/24"});
    assert_eq!(sub.attrs, want);
    assert_eq!(sub.computed["pool"], "10.1.0.10..=10.1.0.100");
    assert_eq!(
        (&sub.computed["dhcp"], &sub.computed["no_gateway"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(sub.computed["gateway_ip"], "10.1.0.1");
    let sent = lab
        .server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/subnet"))
        .unwrap();
    assert_eq!(
        sent.body,
        json!({"region": "BHS5", "network": "10.1.0.0/24", "start": "10.1.0.10",
               "end": "10.1.0.100", "dhcp": true, "noGateway": false})
    );
    let (attrs, computed) = lab.read(SUBNET, "lab", &sub.remote).unwrap();
    assert_eq!(attrs, want);
    assert_eq!(computed["pool"], "10.1.0.10..=10.1.0.100");
    assert!(lab.plan(SUBNET, (&attrs, &computed), &doc).0.is_empty());
    let mut no_dhcp = doc.clone();
    no_dhcp["dhcp"] = json!(false);
    assert!(lab.plan(SUBNET, (&attrs, &computed), &no_dhcp).1);
    // The API changes no pool in place: a new one replaces the subnet.
    let mut wider = doc.clone();
    wider["pool"] = json!("10.1.0.10..=10.1.0.200");
    assert_eq!(
        lab.plan(SUBNET, (&attrs, &computed), &wider),
        (vec!["pool".to_string()], true)
    );

    let again = lab
        .apply(pb::Op::Create, SUBNET, "other", "", doc)
        .unwrap_err();
    assert!(again.contains("already exists with this key"), "{again}");
    // A network with a subnet is not deleted: dform deletes the subnet
    // first, as it refers to the network.
    assert!(
        lab.apply(pb::Op::Delete, NETWORK, "lab", &net.remote, Json::Null)
            .is_err()
    );
    lab.delete(SUBNET, "lab", &sub.remote);
    assert!(lab.read(SUBNET, "lab", &sub.remote).is_none());
    // The replacement: made again with the new pool.
    let sub = lab.create(SUBNET, "lab", wider);
    assert_eq!(sub.computed["pool"], "10.1.0.10..=10.1.0.200");
    lab.delete(SUBNET, "lab", &sub.remote);
    lab.delete(NETWORK, "lab", &net.remote);
    assert!(lab.read(SUBNET, "lab", &sub.remote).is_none());
}

/// A subnet the program gives no pool: Plan says the one it is given,
/// the range's hosts after the gateway (from the first host without
/// one) to the last before broadcast, as the OVH console fills it in;
/// Create sends its ends, and the program plans clean against what Read
/// answers.
#[test]
fn a_subnets_pool_defaults_to_its_ranges_hosts() {
    let lab = Lab::new();
    let net = lab.create(NETWORK, "lab", json!({"name": "lab", "regions": ["BHS5"]}));
    let doc = json!({"network": net.remote, "region": "BHS5", "range": "10.42.0.0/24",
                     "dhcp": true});
    let (changes, _) = lab.ovh.plan(SUBNET, "lab", "", None, Some(&doc)).unwrap();
    let pool = changes.iter().find(|c| c.path == "pool").unwrap();
    assert_eq!(
        (&pool.before, &pool.after),
        (&None, &Some(json!("10.42.0.2..=10.42.0.254")))
    );
    let sub = lab.create(SUBNET, "lab", doc.clone());
    let posted = |n: usize| {
        lab.server
            .seen()
            .into_iter()
            .filter(|c| c.method == "POST" && c.path.ends_with("/subnet"))
            .nth(n)
            .unwrap()
            .body
    };
    assert_eq!(
        (&posted(0)["start"], &posted(0)["end"]),
        (&json!("10.42.0.2"), &json!("10.42.0.254"))
    );
    assert_eq!(sub.computed["pool"], "10.42.0.2..=10.42.0.254");
    let (attrs, computed) = lab.read(SUBNET, "lab", &sub.remote).unwrap();
    // An existing subnet's pool is not planned again.
    assert!(lab.plan(SUBNET, (&attrs, &computed), &doc).0.is_empty());
    lab.delete(SUBNET, "lab", &sub.remote);

    // Without a gateway the pool starts at the first host.
    let mut bare = doc.clone();
    bare["no_gateway"] = json!(true);
    let (changes, _) = lab.ovh.plan(SUBNET, "lab", "", None, Some(&bare)).unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c.path == "pool" && c.after == Some(json!("10.42.0.1..=10.42.0.254"))),
        "{changes:?}"
    );
    lab.create(SUBNET, "lab", bare);
    assert_eq!(
        (&posted(1)["start"], &posted(1)["end"]),
        (&json!("10.42.0.1"), &json!("10.42.0.254"))
    );
}

/// The API's `start` and `end` are not attributes: Plan refuses each,
/// naming `pool` and the pool they spell. A pool outside the range's
/// hosts, or holding the gateway, is refused; so is a range with no host
/// for one.
#[test]
fn a_subnets_pool_is_one_attribute_inside_its_range() {
    let lab = Lab::new();
    let doc = |extra: Json| {
        let mut d = json!({"network": "pn-1", "region": "BHS5", "range": "10.42.0.0/24"});
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        d
    };
    let start = lab.plan_err(
        SUBNET,
        None,
        &doc(json!({"start": "10.42.0.10", "end": "10.42.0.200"})),
    );
    assert_eq!(
        start,
        "plan ovh.subnet[\"x\"]: start is not an attribute of ovh.subnet: the pool's first and \
         last address are one, `pool = \"10.42.0.10..=10.42.0.200\"`"
    );
    let end = lab.plan_err(SUBNET, None, &doc(json!({"end": "10.42.0.200"})));
    assert!(
        end.contains("end is not an attribute of ovh.subnet")
            && end.contains("`pool = \"FIRST..=LAST\"`"),
        "{end}"
    );
    let outside = lab.plan_err(
        SUBNET,
        None,
        &doc(json!({"pool": "10.42.0.10..=10.42.1.20"})),
    );
    assert!(
        outside.ends_with(
            "pool 10.42.0.10..=10.42.1.20 is not in range 10.42.0.0/24: its hosts are \
             10.42.0.2..=10.42.0.254"
        ),
        "{outside}"
    );
    let gateway = lab.plan_err(SUBNET, None, &doc(json!({"pool": "10.42.0.1..=10.42.0.9"})));
    assert!(
        gateway.ends_with(
            "pool 10.42.0.1..=10.42.0.9 holds the subnet's gateway, 10.42.0.1: its hosts are \
             10.42.0.2..=10.42.0.254 (or no_gateway = true)"
        ),
        "{gateway}"
    );
    assert!(
        lab.ovh
            .plan(
                SUBNET,
                "x",
                "",
                None,
                Some(&doc(
                    json!({"pool": "10.42.0.1..=10.42.0.9", "no_gateway": true})
                ))
            )
            .is_ok()
    );
    let tiny = lab.plan_err(SUBNET, None, &doc(json!({"range": "10.42.0.0/31"})));
    assert!(
        tiny.ends_with("range 10.42.0.0/31 has no host for a pool"),
        "{tiny}"
    );
}

/// An instance on a private network: made with an interface on the
/// public network and one on the private network, by their OpenStack
/// ids in its region; read back as the reference and its address there,
/// by network. A network not in its region is refused naming the ones it
/// is in.
#[test]
fn an_instance_joins_a_private_network() {
    let lab = Lab::new();
    lab.server.build_polls(0);
    let net = lab.create(NETWORK, "lab", json!({"name": "lab", "regions": ["BHS5"]}));
    lab.create(
        SUBNET,
        "lab",
        json!({"network": net.remote, "region": "BHS5", "range": "10.1.0.0/24",
               "pool": "10.1.0.10..=10.1.0.100"}),
    );
    let doc = |region: &str| {
        json!({"name": "vm", "region": region, "flavor": "d2-2", "image": "Debian 13",
               "networks": [net.remote]})
    };
    let vm = lab.create("ovh.instance", "vm", doc("BHS5"));
    let sent = lab
        .server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/instance"))
        .unwrap();
    assert_eq!(
        sent.body["networks"],
        json!([{"networkId": "ext-BHS5"}, {"networkId": "net-0-BHS5"}])
    );
    assert_eq!(vm.attrs["networks"], json!([net.remote]));
    let ip = vm.computed["private_ips"][&net.remote].as_str().unwrap();
    assert!(ip.starts_with("10.1.0."), "{ip}");
    assert_eq!(vm.computed["private_ip"], ip);
    assert!(vm.computed["public_ip"].is_string());
    let (attrs, _) = lab.read("ovh.instance", "vm", &vm.remote).unwrap();
    assert_eq!(attrs["networks"], json!([net.remote]));

    let elsewhere = lab
        .apply(pb::Op::Create, "ovh.instance", "far", "", {
            let mut d = doc("ca-east-tor");
            d["name"] = json!("far");
            d
        })
        .unwrap_err();
    assert!(
        elsewhere.contains("network lab is not in region ca-east-tor (it is in BHS5)"),
        "{elsewhere}"
    );
}

/// Every file under `dir`.
fn files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn ovh() -> String {
    common::exe("dform-provider-ovh")
}

/// `dform ARGS` in the scratch project, the provider pointed at `server`.
fn dform(s: &Scratch, server: &Server, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in server.env() {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

fn project(name: &str, program: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n",
            ovh()
        ),
    );
    s.write("main.df", program);
    s
}

/// The program of R-157's done-when: a private network and its subnet, an
/// instance on it, a volume attached to the instance, a user with S3
/// credentials and a container it owns.
fn lab_program(server: &Server, image: &str) -> String {
    format!(
        r#"
use ovh {{ endpoint = "{}", project = "{}" }}

resource ovh.network lab {{
  name = "lab"
  vlan_id = 42
  regions = ["BHS5"]
}}

resource ovh.subnet lab {{
  network = lab
  region = "BHS5"
  range = "10.42.0.0/24"
  dhcp = true
}}

resource ovh.instance server {{
  name = "lab-server"
  region = "BHS5"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  networks = [lab]
}}

resource ovh.volume data {{
  name = "lab-data"
  region = "BHS5"
  size = 20Gi
  type = "high-speed"
  image = "{image}"
  instance = server
}}

resource ovh.cloud_project_user backup {{
  description = "lab backups"
  roles = ["objectstore_operator"]
}}

resource ovh.storage_container backups {{
  region = "BHS5"
  name = "lab-backups"
  owner = backup
  versioning = true
}}
"#,
        server.endpoint,
        fake::DESCRIPTION
    )
}

/// R-157's done-when: the program plans, applies against the fake API,
/// and plans clean. The instance is on the private network, the volume
/// attached to it, the container owned by the user, whose S3 secret is
/// nowhere in dform's files; the volume's image, write-only, is kept as
/// its digest in state (R-106), and a new one replaces the volume. An
/// empty program deletes them all, each after what refers to it.
#[test]
fn a_program_with_every_type_plans_applies_and_plans_clean() {
    let server = Server::start();
    let s = project("ovh-types", &lab_program(&server, "Debian 13"));
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    for line in [
        "+ ovh.network lab",
        "+ ovh.subnet lab",
        "+ ovh.instance server",
        "+ ovh.volume data",
        "+ ovh.cloud_project_user backup",
        "+ ovh.storage_container backups",
        "instance = server",
        "owner = backup",
        // The subnet's pool, which the program leaves out.
        "pool = \"10.42.0.2..=10.42.0.254\"",
    ] {
        assert!(plan.stdout.contains(line), "{line}\n{}", plan.stdout);
    }
    assert!(server.instances().is_empty() && server.volumes().is_empty());

    let applied = dform(&s, &server, &["apply", "main.df"]).success();
    // Each status the API gave, beside the change (R-130).
    for (change, status) in [
        ("+ ovh.volume data", "attaching"),
        ("+ ovh.network lab", "BUILDING"),
        ("+ ovh.cloud_project_user backup", "creating"),
    ] {
        assert!(
            applied
                .stderr
                .lines()
                .any(|l| l.trim_start().starts_with(change) && l.ends_with(status)),
            "{change} {status}\n{}",
            applied.stderr
        );
    }
    assert_eq!(server.networks()[0]["id"], "pn-1000123_42");
    assert_eq!(server.subnets()[0]["cidr"], "10.42.0.0/24");
    let pool = &server.subnets()[0]["ipPools"][0];
    assert_eq!(
        (&pool["start"], &pool["end"]),
        (&json!("10.42.0.2"), &json!("10.42.0.254"))
    );
    let instance = &server.instances()[0];
    let private = instance["ipAddresses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["type"] == "private")
        .unwrap()
        .clone();
    assert_eq!(private["networkId"], "net-42-BHS5");
    let volume = &server.volumes()[0];
    assert_eq!(volume["attachedTo"], json!([instance["id"]]));
    assert_eq!(volume["size"], 20);
    let user = &server.users()[0];
    let id = user["id"].as_i64().unwrap();
    let creds = server.s3_credentials(id);
    assert_eq!(creds.len(), 1);
    let container = &server.containers()[0];
    assert_eq!(container["ownerId"], id);
    assert_eq!(container["versioning"]["status"], "enabled");

    let state = s.read("dform.state/main/state.json");
    let st: Json = serde_json::from_str(&state).unwrap();
    let written = &st["resources"]["ovh.volume::data"]["written"]["image"];
    assert!(
        written.as_str().is_some_and(|d| d.contains("sha256:")),
        "{state}"
    );
    assert!(!state.contains("Debian"), "{state}");
    for f in files(&s.path("dform.state")) {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        assert!(!text.contains(&creds[0].1), "{}: {text}", f.display());
    }

    let again = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    let json = dform(&s, &server, &["plan", "main.df", "--json"]).success();
    assert!(!json.stdout.contains(&creds[0].1), "{}", json.stdout);

    // Another image replaces the volume.
    s.write("main.df", &lab_program(&server, "Ubuntu 24.04"));
    let changed = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        changed.stdout.contains("ovh.volume data") && changed.stdout.contains("replace"),
        "{}",
        changed.stdout
    );

    // Nothing left in the program: everything goes.
    s.write(
        "main.df",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n",
            server.endpoint
        ),
    );
    dform(&s, &server, &["apply", "main.df"]).success();
    assert!(server.instances().is_empty() && server.volumes().is_empty());
    assert!(server.networks().is_empty() && server.subnets().is_empty());
    assert!(server.users().is_empty() && server.containers().is_empty());
}

/// A program that writes the API's `start` and `end` on a subnet is
/// refused at plan, naming `pool`; one that writes `pool` plans it.
#[test]
fn a_program_writes_a_subnets_pool() {
    let server = Server::start();
    let program = |pool: &str| {
        format!(
            "use ovh {{ endpoint = \"{}\", project = \"{}\" }}\n\n\
             resource ovh.network lab {{ name = \"lab\", regions = [\"BHS5\"] }}\n\n\
             resource ovh.subnet lab {{\n  network = lab\n  region = \"BHS5\"\n  \
             range = \"10.42.0.0/24\"\n{pool}}}\n",
            server.endpoint,
            fake::DESCRIPTION
        )
    };
    let s = project(
        "ovh-subnet-pool",
        &program("  start = \"10.42.0.10\"\n  end = \"10.42.0.200\"\n"),
    );
    let refused = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        refused.stderr.contains(
            "start is not an attribute of ovh.subnet: the pool's first and last address are \
             one, `pool = \"10.42.0.10..=10.42.0.200\"`"
        ),
        "{}",
        refused.stderr
    );
    s.write(
        "main.df",
        &program("  pool = \"10.42.0.10..=10.42.0.200\"\n"),
    );
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("pool = \"10.42.0.10..=10.42.0.200\""),
        "{}",
        plan.stdout
    );
    dform(&s, &server, &["apply", "main.df"]).success();
    let pool = &server.subnets()[0]["ipPools"][0];
    assert_eq!(
        (&pool["start"], &pool["end"]),
        (&json!("10.42.0.10"), &json!("10.42.0.200"))
    );
    let again = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    // Its ends quoted are the same range (R-180).
    s.write(
        "main.df",
        &program("  pool = \"10.42.0.10\"..=\"10.42.0.200\"\n"),
    );
    let ends = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(ends.stdout.contains("is up to date"), "{}", ends.stdout);
}
