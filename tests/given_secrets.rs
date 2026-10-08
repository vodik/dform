//! Given secrets (R-108): what a human types for a deployment lives in a
//! file per deployment in the repository, SOPS's JSON sealed to the
//! deployment's age recipients (and its master's own key where a
//! passphrase or the key file opens it), written by `dform secrets set`
//! and read into the deployment's `secret(T)` inputs by `set from
//! secrets.decode(io.read(..))`. A plan without the master reads each
//! value by its stand-in, so a value not set again is proven unchanged.

mod common;
use age::secrecy::ExposeSecret;
use common::{Run, Scratch, dform, repo, yes};
use std::io::Write;
use std::process::Stdio;

const NOW: &str = "2026-10-07T09:00:00Z";

/// An operator's age identity (secret) and recipient (public).
struct Member {
    identity: String,
    recipient: String,
}

fn member() -> Member {
    let id = age::x25519::Identity::generate();
    Member {
        identity: id.to_string().expose_secret().to_string(),
        recipient: id.to_public().to_string(),
    }
}

/// `dform ARGS` in `s` as alice at `NOW`, holding `identity` and given
/// `env`, `stdin` on its standard input.
fn run_in(
    s: &Scratch,
    identity: Option<&Member>,
    env: &[(&str, &str)],
    args: &[&str],
    stdin: &str,
) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env("DFORM_CREDENTIALS", s.path("no-credentials"))
        .env("DFORM_ACTOR", "alice")
        .env("DFORM_TEST_NOW", NOW)
        .env_remove("RANDOM_MASTER")
        .env_remove("AGE_IDENTITY")
        .env_remove("PASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(m) = identity {
        c.env("AGE_IDENTITY", &m.identity);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    Run::from(child.wait_with_output().unwrap())
}

/// With the passphrase, no identity.
fn run(s: &Scratch, args: &[&str]) -> Run {
    run_in(s, None, &[("PASS", "pw")], args, "")
}

/// `dform secrets set p NAME`, the value on stdin.
fn set(s: &Scratch, name: &str, value: &str) -> Run {
    run_in(
        s,
        None,
        &[("PASS", "pw")],
        &["secrets", "set", "p", name],
        value,
    )
}

/// A deployment `p` with two secret inputs a file of given secrets
/// gives, its master sealed as `secrets` (a `[secrets]` table) says.
fn project(name: &str, secrets: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!("[project]\nedition = \"2026\"\n\n{secrets}"),
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"admin\", \"string\", [\"sensitive\"])\n\
               type_attr(db.secret, \"token\", \"string\", [\"sensitive\"])\n"),
    );
    s.write(
        "stacks/p.df",
        r#"input admin: secret(string)
input token: secret(string) = "none yet"
use fake
set from secrets.decode(io.read("secrets/p.json"))
resource db.secret app {
  admin = admin
  token = token
}
"#,
    );
    s
}

const PASSPHRASE: &str = "[secrets]\npassphrase = \"env:PASS\"\n";

fn world(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/p/remote.json")["resources"]["db.secret::app"]["attrs"].clone()
}

fn entries(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("dform.state/p/state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == kind)
        .collect()
}

#[test]
fn a_given_secret_is_sealed_into_the_file_and_read_into_its_input() {
    let s = project("given-set", PASSPHRASE);
    // Nothing given yet: the input is missing, and says so.
    let r = run(&s, &["plan", "p"]).failure();
    assert!(
        r.stderr
            .contains("input admin is required and has no value"),
        "{}",
        r.stderr
    );
    let r = set(&s, "admin", "hunter2\n").success();
    assert_eq!(
        r.stdout,
        "sealed admin of p into secrets/p.json (generation 1), sealed to the deployment's \
         master; commit it: the next plan reads it\n"
    );
    // SOPS's shape: the value sealed, the data key sealed to the master's
    // own recipient, which the file names.
    let text = s.read("secrets/p.json");
    assert!(!text.contains("hunter2"), "{text}");
    let f: serde_json::Value = serde_json::from_str(&text).unwrap();
    let admin = f["admin"].as_str().unwrap();
    assert!(
        admin.starts_with("ENC[AES256_GCM,data:") && admin.ends_with(",type:str]"),
        "{admin}"
    );
    let sops = &f["sops"];
    assert_eq!(sops["age"].as_array().unwrap().len(), 1, "{sops}");
    assert_eq!(sops["age"][0]["recipient"], sops["dform"]["stack_key"]);
    assert!(
        sops["age"][0]["enc"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN AGE ENCRYPTED FILE-----\n")
    );
    assert_eq!(sops["lastmodified"], NOW);
    assert!(sops["mac"].as_str().unwrap().starts_with("ENC[AES256_GCM,"));
    assert_eq!(
        sops["dform"]["given"]["admin"],
        serde_json::json!({ "generation": 1, "at": NOW, "by": "alice" })
    );
    let given = entries(&s, "given");
    assert_eq!(given.len(), 1);
    assert_eq!(given[0]["key"], "admin");
    assert_eq!(given[0]["file"], "secrets/p.json");
    assert_eq!(given[0]["who"], "alice");
    assert_eq!(
        given[0]["recipients"],
        serde_json::json!(["the deployment's master"])
    );

    // The plan reads it into the input, at its line, never in the clear,
    // nor in a plan file.
    let r = run(&s, &["plan", "p", "--out", "plan.json"]).success();
    assert!(
        r.stdout.contains("admin = (sensitive)  secrets/p.json:2"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("hunter2") && !r.stderr.contains("hunter2"));
    assert!(!s.read("plan.json").contains("hunter2"));
    run(&s, &["apply", "plan.json"]).success();
    assert_eq!(world(&s)["admin"], "hunter2");

    // Listed by the file, its generation and who opens it.
    let r = run(&s, &["secrets", "list", "p"]).success();
    let line = r
        .stdout
        .lines()
        .find(|l| l.starts_with("admin "))
        .unwrap_or_else(|| panic!("{}", r.stdout));
    assert_eq!(
        line.split_whitespace().collect::<Vec<_>>().join(" "),
        "admin given 1 now db.secret app.admin update",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("secrets/p.json: 1 given secret, sealed to the deployment's master\n"),
        "{}",
        r.stdout
    );
    // Rotated by setting it again.
    let r = run(&s, &["secrets", "rotate", "p", "admin"]).failure();
    assert!(
        r.stderr.contains(
            "secrets rotate admin: admin is given in p, sealed in secrets/p.json: give it its \
             new value with `dform secrets set p admin`, then plan"
        ),
        "{}",
        r.stderr
    );
    let r = set(&s, "admin", "hunter3").success();
    assert!(r.stdout.contains("(generation 2)"), "{}", r.stdout);
    let r = run(&s, &["plan", "p"]).success();
    assert_eq!(r.summary(), "plan: 1 change (1 update) over 1 tick");
    run(&s, &["apply", "p"]).success();
    assert_eq!(world(&s)["admin"], "hunter3");
}

#[test]
fn a_plan_without_the_master_reads_a_given_secret_by_its_stand_in() {
    let s = project("given-nokey", PASSPHRASE);
    set(&s, "admin", "hunter2").success();
    set(&s, "token", "t0k3n").success();
    run(&s, &["apply", "p"]).success();
    let nokey = |s: &Scratch| run_in(s, None, &[], &["plan", "p"], "").success();
    let r = nokey(&s);
    assert!(
        r.stderr
            .contains("p: planned without its master (PASS is not set)"),
        "{}",
        r.stderr
    );
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    // Set again: that value changed, and only a run with the master can
    // say how.
    set(&s, "token", "t0k3n-2").success();
    let r = nokey(&s);
    assert_eq!(r.summary(), "plan: 1 change (1 update) over 1 tick");
    assert!(
        r.stdout.contains(
            "~ db.secret app                        stacks/p.df:5  secret changed, needs the key"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("token = (sensitive) → (sensitive)"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("admin ="), "{}", r.stdout);
}

#[test]
fn unset_removes_one_and_keeps_every_other_ciphertext() {
    let s = project("given-unset", PASSPHRASE);
    set(&s, "admin", "hunter2").success();
    set(&s, "token", "t0k3n").success();
    let before: serde_json::Value = s.json("secrets/p.json");
    // A value set again: the other's ciphertext and the data key kept.
    set(&s, "admin", "hunter3").success();
    let after: serde_json::Value = s.json("secrets/p.json");
    assert_eq!(before["token"], after["token"]);
    assert_eq!(before["sops"]["age"], after["sops"]["age"]);
    assert_ne!(before["admin"], after["admin"]);

    let r = run(&s, &["secrets", "unset", "p", "token"]).success();
    assert_eq!(
        r.stdout,
        "removed token of p from secrets/p.json; commit it: the next plan reads it\n"
    );
    let f: serde_json::Value = s.json("secrets/p.json");
    assert!(f.get("token").is_none(), "{f}");
    assert_eq!(f["admin"], after["admin"]);
    assert!(f["sops"]["dform"]["given"].get("token").is_none(), "{f}");
    // The MAC is the remaining values': the plan opens the file.
    run(&s, &["apply", "p"]).success();
    assert_eq!(world(&s)["admin"], "hunter3");
    assert_eq!(world(&s)["token"], "none yet");
    let r = run(&s, &["secrets", "unset", "p", "token"]).failure();
    assert!(
        r.stderr
            .contains("secrets unset token: secrets/p.json gives no token (it gives admin)"),
        "{}",
        r.stderr
    );
    // A secret input's, and not empty.
    let r = set(&s, "nope", "x").failure();
    assert!(
        r.stderr.contains(
            "secrets set nope: nope is not a secret input of p (its secret inputs: admin, \
             token); declare it `input nope: secret(string)`"
        ),
        "{}",
        r.stderr
    );
    let r = set(&s, "admin", "").failure();
    assert!(
        r.stderr.contains("admin of p: no value was given"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_secret_input_given_inline_on_the_command_line_warns() {
    let s = project("given-warn", PASSPHRASE);
    set(&s, "admin", "hunter2").success();
    let r = run(&s, &["plan", "p", "--set", "token=inline"]).success();
    assert!(
        r.stderr.contains(
            "warning: --set token: argv is readable by every user on this host through /proc \
             and lands in shell history; use --set token=@FILE or `dform secrets set p token`"
        ),
        "{}",
        r.stderr
    );
    s.write("token.json", "\"from-a-file\"");
    let r = run(&s, &["plan", "p", "--set", "token=@token.json"]).success();
    assert!(!r.stderr.contains("warning: --set"), "{}", r.stderr);
}

#[test]
fn a_sealed_file_gives_secret_inputs_alone() {
    let s = project("given-plain", PASSPHRASE);
    set(&s, "admin", "hunter2").success();
    set(&s, "token", "t0k3n").success();
    // `token` declared plain since: the file may not give it.
    let program = s.read("stacks/p.df").replace(
        "input token: secret(string) = \"none yet\"",
        "input token: string = \"none yet\"",
    );
    s.write("stacks/p.df", &program);
    let r = run(&s, &["plan", "p"]).failure();
    let line = s
        .read("secrets/p.json")
        .lines()
        .position(|l| l.starts_with("  \"token\":"))
        .unwrap()
        + 1;
    assert!(
        r.stderr.contains(&format!(
            "secrets/p.json:{line}: token is not a secret input: a file of given secrets gives \
             only `secret(T)` inputs; declare it `input token: secret(string)`"
        )),
        "{}",
        r.stderr
    );
    // A value in the clear is no given secret.
    let mut f: serde_json::Value = s.json("secrets/p.json");
    f["admin"] = "hunter2".into();
    s.write("secrets/p.json", &f.to_string());
    let r = run(&s, &["plan", "p"]).failure();
    assert!(
        r.stderr.contains(
            "admin is in the clear: a given secret is sealed; `dform secrets set` seals it"
        ),
        "{}",
        r.stderr
    );
}

/// The data key of a file of given secrets, opened with `identity` as
/// SOPS opens it: the armored age file of its stanza for that recipient.
fn data_key(f: &serde_json::Value, m: &Member) -> [u8; 32] {
    use std::io::Read;
    let id: age::x25519::Identity = m.identity.parse().unwrap();
    let stanza = f["sops"]["age"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["recipient"] == m.recipient.as_str())
        .unwrap_or_else(|| panic!("not sealed to {}: {f}", m.recipient));
    let enc = stanza["enc"].as_str().unwrap();
    let d = age::Decryptor::new(age::armor::ArmoredReader::new(enc.as_bytes())).unwrap();
    let mut r = d
        .decrypt(std::iter::once(&id as &dyn age::Identity))
        .unwrap();
    let mut key = Vec::new();
    r.read_to_end(&mut key).unwrap();
    key.try_into().unwrap()
}

type Gcm = aes_gcm::AesGcm<aes_gcm::aes::Aes256, aes_gcm::aead::consts::U32>;

/// SOPS's `ENC[AES256_GCM,..]` of `plain` at `aad`, as sops writes it.
fn sops_encrypt(key: &[u8; 32], plain: &str, aad: &str, iv: [u8; 32]) -> String {
    use aes_gcm::KeyInit;
    use aes_gcm::aead::{Aead, Payload};
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD;
    let out = Gcm::new_from_slice(key)
        .unwrap()
        .encrypt(
            (&iv).into(),
            Payload {
                msg: plain.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    let (data, tag) = out.split_at(out.len() - 16);
    format!(
        "ENC[AES256_GCM,data:{},iv:{},tag:{},type:str]",
        b.encode(data),
        b.encode(iv),
        b.encode(tag)
    )
}

/// What sops decrypts `enc` at `aad` to.
fn sops_decrypt(key: &[u8; 32], enc: &str, aad: &str) -> String {
    use aes_gcm::KeyInit;
    use aes_gcm::aead::{Aead, Payload};
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD;
    let body = enc
        .strip_prefix("ENC[AES256_GCM,")
        .and_then(|e| e.strip_suffix(",type:str]"))
        .unwrap();
    let part = |k: &str| {
        let v = body
            .split(',')
            .find_map(|p| p.strip_prefix(&format!("{k}:")))
            .unwrap();
        b.decode(v).unwrap()
    };
    let mut data = part("data");
    data.extend(part("tag"));
    let iv: [u8; 32] = part("iv").try_into().unwrap();
    let plain = Gcm::new_from_slice(key)
        .unwrap()
        .decrypt(
            (&iv).into(),
            Payload {
                msg: &data,
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    String::from_utf8(plain).unwrap()
}

/// SOPS's MAC of `values` in order: SHA-512, upper-case hex.
fn sops_mac(values: &[&str]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha512::new();
    for v in values {
        h.update(v.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02X}")).collect()
}

/// A team's file, sealed to each member's age key and to no key of the
/// deployment's: each member opens it, with the age crate as SOPS does;
/// one removed from dform.toml is no longer sealed to, under a new data
/// key.
#[test]
fn a_file_sealed_to_the_recipients_opens_as_sops_opens_it() {
    let (alice, bob) = (member(), member());
    let team = |members: &[(&str, &Member)]| {
        let list: Vec<String> = members
            .iter()
            .map(|(n, m)| format!("{n} = \"{}\"", m.recipient))
            .collect();
        format!("[secrets]\nrecipients = {{ {} }}\n", list.join(", "))
    };
    let s = project("given-team", &team(&[("alice", &alice), ("bob", &bob)]));
    let as_alice = |args: &[&str], stdin: &str| run_in(&s, Some(&alice), &[], args, stdin);
    let r = as_alice(&["secrets", "set", "p", "admin"], "hunter2").success();
    assert!(
        r.stdout.contains("sealed to alice and bob;"),
        "{}",
        r.stdout
    );
    let f: serde_json::Value = s.json("secrets/p.json");
    assert!(f["sops"]["dform"].get("stack_key").is_none(), "{f}");
    let mut want = vec![alice.recipient.clone(), bob.recipient.clone()];
    want.sort();
    let got: Vec<String> = f["sops"]["age"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["recipient"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, want);
    // Each opens it as sops does: the same data key, the value at its
    // path, the MAC over the values under `lastmodified`.
    let key = data_key(&f, &bob);
    assert_eq!(key, data_key(&f, &alice));
    let admin = f["admin"].as_str().unwrap();
    assert_eq!(sops_decrypt(&key, admin, "admin:"), "hunter2");
    assert_eq!(
        sops_decrypt(&key, f["sops"]["mac"].as_str().unwrap(), NOW),
        sops_mac(&["hunter2"])
    );
    // Bob plans with it.
    let r = run_in(&s, Some(&bob), &[], &["plan", "p"], "").success();
    assert_eq!(r.summary(), "plan: 1 change (1 create) over 1 tick");
    assert!(!r.stderr.contains("planned without"), "{}", r.stderr);

    // Bob leaves: the next value Alice sets is under a new data key,
    // sealed to her alone.
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n{}",
            team(&[("alice", &alice)])
        ),
    );
    as_alice(&["secrets", "set", "p", "token"], "t0k3n").success();
    let g: serde_json::Value = s.json("secrets/p.json");
    assert_eq!(g["sops"]["age"].as_array().unwrap().len(), 1, "{g}");
    let key2 = data_key(&g, &alice);
    assert_ne!(key, key2);
    assert_ne!(g["admin"], f["admin"]);
    assert_eq!(
        sops_decrypt(&key2, g["admin"].as_str().unwrap(), "admin:"),
        "hunter2"
    );
    let r = run_in(&s, Some(&bob), &[], &["secrets", "set", "p", "admin"], "x").failure();
    assert!(
        r.stderr.contains("secrets/p.json: it is sealed to alice, and this run holds none of them (AGE_IDENTITY tried)"),
        "{}",
        r.stderr
    );
}

/// A file sops wrote (an age recipient, a nested value, its keys in no
/// sorted order, sops's metadata) is read as one `dform secrets set`
/// wrote, its values at their paths and lines.
#[test]
fn a_file_sops_wrote_is_read_into_the_inputs() {
    let alice = member();
    let s = project(
        "given-sops",
        &format!(
            "[secrets]\nrecipients = {{ alice = \"{}\" }}\n",
            alice.recipient
        ),
    );
    // A used module's secret input, given by its path.
    s.write("pg.df", "input password: secret(string)\n");
    let program = s
        .read("stacks/p.df")
        .replace("use fake\n", "use fake\nuse pg\n")
        .replace(
            "  token = token\n",
            "  token = \"${token}/${pg.password}\"\n",
        );
    s.write("stacks/p.df", &program);
    let key = [7u8; 32];
    let wrapped = {
        let r: age::x25519::Recipient = alice.recipient.parse().unwrap();
        let enc =
            age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient)).unwrap();
        let mut out = Vec::new();
        let armor =
            age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)
                .unwrap();
        let mut w = enc.wrap_output(armor).unwrap();
        w.write_all(&key).unwrap();
        w.finish().unwrap().finish().unwrap();
        String::from_utf8(out).unwrap()
    };
    let last = "2026-10-01T10:00:00Z";
    let text = format!(
        r#"{{
	"token": "{}",
	"pg": {{
		"password": "{}"
	}},
	"admin": "{}",
	"sops": {{
		"kms": null,
		"gcp_kms": null,
		"azure_kv": null,
		"hc_vault": null,
		"age": [
			{{
				"recipient": "{}",
				"enc": {}
			}}
		],
		"lastmodified": "{last}",
		"mac": "{}",
		"pgp": null,
		"unencrypted_suffix": "_unencrypted",
		"version": "3.9.4"
	}}
}}
"#,
        sops_encrypt(&key, "t0k3n", "token:", [1; 32]),
        sops_encrypt(&key, "s3cr3t", "pg:password:", [2; 32]),
        sops_encrypt(&key, "hunter2", "admin:", [3; 32]),
        alice.recipient,
        serde_json::to_string(&wrapped).unwrap(),
        sops_encrypt(
            &key,
            &sops_mac(&["t0k3n", "s3cr3t", "hunter2"]),
            last,
            [4; 32]
        ),
    );
    s.write("secrets/p.json", &text);
    let r = run_in(&s, Some(&alice), &[], &["plan", "p"], "").success();
    assert!(
        r.stdout.contains("admin = (sensitive)  secrets/p.json:6"),
        "{}",
        r.stdout
    );
    run_in(&s, Some(&alice), &[], &["apply", "p"], "").success();
    assert_eq!(world(&s)["admin"], "hunter2");
    assert_eq!(world(&s)["token"], "t0k3n/s3cr3t");
    // The MAC is over the values in the file's order: the same values in
    // another order are not what it says.
    let f: serde_json::Value = serde_json::from_str(&text).unwrap();
    s.write("secrets/p.json", &serde_json::to_string_pretty(&f).unwrap());
    let r = run_in(&s, Some(&alice), &[], &["plan", "p"], "").failure();
    assert!(
        r.stderr.contains(
            "secrets/p.json: its MAC is not its values': a value was altered, added or removed"
        ),
        "{}",
        r.stderr
    );
}
