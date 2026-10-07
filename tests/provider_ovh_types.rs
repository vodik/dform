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
    notes: Vec<String>,
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
                notes: a.notes,
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
            ..Default::default()
        };
        match self.call(Call::Read(r), &Mutex::default()).unwrap() {
            Reply::Read(r) if r.found => Some((
                wire::from_doc(r.attrs.as_ref().unwrap()).unwrap(),
                wire::from_doc(r.computed.as_ref().unwrap()).unwrap(),
            )),
            _ => None,
        }
    }

    /// Plan: the paths that change, and whether it replaces.
    fn plan(&self, typ: &str, prior: Option<&Json>, desired: &Json) -> (Vec<String>, bool) {
        let (changes, replaces) = self.ovh.plan(typ, "x", "", prior, Some(desired)).unwrap();
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
    assert_eq!(
        c.attrs,
        json!({"region": "BHS5", "name": "lab-logs", "versioning": false,
               "owner": user})
    );
    assert_eq!(
        c.computed["virtual_host"],
        "lab-logs.s3.bhs5.io.cloud.ovh.net"
    );
    let (attrs, _) = lab.read(CONTAINER, "logs", &c.remote).unwrap();
    assert_eq!(attrs, c.attrs);

    // Versioning on, then off: `enabled`, then `suspended`, in place. (The
    // owner the program does not set is the world's, as dform fills an
    // optional computed attribute.)
    let on = json!({"region": "BHS5", "name": "lab-logs", "versioning": true, "owner": user});
    let (paths, replaces) = lab.plan(CONTAINER, Some(&attrs), &on);
    assert_eq!((paths, replaces), (vec!["versioning".to_string()], false));
    let u = lab.update(CONTAINER, "logs", &c.remote, on);
    assert_eq!(u.attrs["versioning"], true);
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
        Some(&attrs),
        &json!({"region": "BHS5", "name": "lab-logs-2", "owner": user}),
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
    let (paths, replaces) = lab.plan(USER, Some(&u.attrs), &more);
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
        Some(&changed.attrs),
        &json!({"description": "backup",
                "roles": ["volume_operator", "objectstore_operator"]}),
    );
    assert!(paths.is_empty(), "{paths:?}");
    // A new description replaces it.
    let (_, replaces) = lab.plan(
        USER,
        Some(&changed.attrs),
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
    assert_eq!(v.attrs, doc(20, Some(&one)));
    assert_eq!(v.computed["status"], "in-use");
    assert_eq!(lab.server.volumes()[0]["attachedTo"], json!([one]));
    let (attrs, _) = lab.read(VOLUME, "data", &v.remote).unwrap();
    assert_eq!(attrs, v.attrs);
    assert_eq!(
        lab.plan(VOLUME, Some(&attrs), &doc(20, Some(&one))).0.len(),
        0
    );

    // Bigger in place, smaller replaced, another region replaced.
    assert_eq!(
        lab.plan(VOLUME, Some(&attrs), &doc(50, Some(&one))),
        (vec!["size".to_string()], false)
    );
    assert!(lab.plan(VOLUME, Some(&attrs), &doc(10, Some(&one))).1);
    let mut moved = doc(20, Some(&one));
    moved["region"] = json!("ca-east-tor");
    assert!(lab.plan(VOLUME, Some(&attrs), &moved).1);
    let grown = lab.update(VOLUME, "data", &v.remote, doc(50, Some(&one)));
    assert_eq!(grown.attrs["size"], 50);
    assert_eq!(lab.calls("POST", "/upsize"), 1);

    // To the other instance: detached, then attached; then cleared.
    assert_eq!(
        lab.plan(VOLUME, Some(&grown.attrs), &doc(50, Some(&two))),
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

resource ovh.instance server {{
  name = "lab-server"
  region = "BHS5"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
}}

resource ovh.volume data {{
  name = "lab-data"
  region = "BHS5"
  size = 20Gi
  type = "high-speed"
  image = "{image}"
  instance = server
}}
"#,
        server.endpoint,
        fake::DESCRIPTION
    )
}

/// R-157's done-when: the program plans, applies against the fake API,
/// and plans clean; the volume's image, write-only, is kept as its digest
/// in state (R-106), and a new one replaces the volume.
#[test]
fn a_program_with_every_type_plans_applies_and_plans_clean() {
    let server = Server::start();
    let s = project("ovh-types", &lab_program(&server, "Debian 13"));
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    for line in [
        "+ ovh.instance server",
        "+ ovh.volume data",
        "instance = server",
    ] {
        assert!(plan.stdout.contains(line), "{line}\n{}", plan.stdout);
    }
    assert!(server.instances().is_empty() && server.volumes().is_empty());

    let applied = dform(&s, &server, &["apply", "main.df"]).success();
    // Each status the API gave, beside the change (R-130).
    for line in ["+ ovh.volume data      0.0s  attaching", "in-use"] {
        assert!(applied.stderr.contains(line), "{line}\n{}", applied.stderr);
    }
    let instance = server.instances()[0]["id"].clone();
    let volume = &server.volumes()[0];
    assert_eq!(volume["attachedTo"], json!([instance]));
    assert_eq!(volume["size"], 20);
    let state = s.read("dform.state/main/state.json");
    let st: Json = serde_json::from_str(&state).unwrap();
    let written = &st["resources"]["ovh.volume::data"]["written"]["image"];
    assert!(
        written.as_str().is_some_and(|d| d.contains("sha256:")),
        "{state}"
    );
    assert!(!state.contains("Debian"), "{state}");

    let again = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);

    // Another image replaces the volume.
    s.write("main.df", &lab_program(&server, "Ubuntu 24.04"));
    let changed = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        changed.stdout.contains("ovh.volume data") && changed.stdout.contains("replace"),
        "{}",
        changed.stdout
    );
}
