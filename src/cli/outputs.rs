//! Secret outputs sealed to the deployments that read them (R-166).

use crate::ast::Atom;
use crate::value::Value;
use crate::{deployment, store};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The deployments of the project that read `own`'s outputs (R-166): each
/// registered one whose program reads it (by name, or by one it computes,
/// which may be any), with the public key its master publishes, when it
/// has one. The grant is the reader's use: the producer's plan prints it.
pub(super) fn readers_of(
    root: &Path,
    own: &str,
    s3: store::OpenS3,
) -> Result<Vec<(String, Option<[u8; 32]>)>> {
    let dir = root.parent().unwrap_or(Path::new("."));
    let Some(project) = crate::project::Project::find(dir, env!("CARGO_PKG_VERSION"))? else {
        return Ok(Vec::new());
    };
    let found = crate::project::discover(&project);
    // As a reader's program names it (`platform[env=lab]`, `platform`).
    let own = crate::stack::short_of(own);
    let own_stack = own.split_once('[').map_or(own.as_str(), |(s, _)| s);
    let mut loaded: std::collections::BTreeMap<PathBuf, Option<deployment::Loaded>> =
        Default::default();
    let mut out = Vec::new();
    for (name, entry) in crate::stack::registry(root)? {
        if crate::stack::short_of(&name) == own {
            continue;
        }
        let (stack, key) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
            Some((s, k)) => (s.to_string(), k.to_string()),
            None => (name.clone(), String::new()),
        };
        let [one] = found.named(&stack)[..] else {
            continue;
        };
        let l = loaded.entry(one.file.clone()).or_insert_with(|| {
            let t = deployment::Target {
                files: vec![one.file.clone()],
                input_files: Vec::new(),
                providers: Vec::new(),
            };
            deployment::load(
                &t,
                env!("CARGO_PKG_VERSION"),
                &|p: &Path| std::fs::read_to_string(p),
                &mut deployment::Notes::default(),
            )
            .ok()
        });
        let Some(l) = l else { continue };
        let given: Vec<Atom> = key
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| {
                crate::ast::atom(
                    "input",
                    vec![
                        crate::ast::str_term(k.trim()),
                        crate::ast::str_term(v.trim()),
                    ],
                    Default::default(),
                )
            })
            .collect();
        let Ok(instance) = crate::stack::instance(&l.cfg, (&l.stack, &l.path), &l.program, &given)
        else {
            continue;
        };
        let (names, any) = crate::stack::reads(&l.program, &l.deployed, &instance.key);
        if names.contains(&own) || any.contains(own_stack) {
            let public = crate::custody::public_of(entry.state.open(s3)?.as_ref())?;
            out.push((name, public));
        }
    }
    Ok(out)
}

/// What a secret output `path` of `deployment` sealed to `reader` is
/// bound to (`custody::seal_to`'s label).
pub(super) fn sealed_label(deployment: &str, path: &str, reader: &str) -> String {
    format!("{deployment}#{path} to {reader}")
}

/// The grant a plan prints (R-166): each secret output no provider holds,
/// and the deployments it is sealed to, `output kubeconfig  sealed to
/// apps[env=lab]`; one whose master is not made yet is said so.
pub(super) fn grants_text(unheld: &[String], readers: &[(String, Option<[u8; 32]>)]) -> String {
    if readers.is_empty() {
        return String::new();
    }
    let to: Vec<String> = readers
        .iter()
        .map(|(r, public)| match public {
            Some(_) => r.clone(),
            None => format!("{r} (no master yet: applied once, it is sealed to by the next apply)"),
        })
        .collect();
    let mut out = String::from("\n");
    for k in unheld {
        out.push_str(&format!("output {k}  sealed to {}\n", to.join(", ")));
    }
    out
}

/// Open each secret output sealed to `deployment` among `read` with its
/// master (R-166): its value goes in the read (`stack::Read::opened`).
/// What was opened, by the deployment and output, and what no provider
/// holds and is not sealed to it (said: its producer's next apply seals
/// to it); a run without the master opens none and says so.
/// The stand-in of a sealed output (R-164): a function of its label and
/// keyed digest, which stays while the value does; what a run without
/// the master reads in its place.
fn standin_of(label: &str, digest: &str) -> String {
    format!(
        "sealed-{}",
        &crate::approval::sha256_hex(format!("{label}\0{digest}").as_bytes())[..32]
    )
}

/// A secret output `k` of the producer `of` (as published, `o`) that a
/// reader in the tree read in memory (`v`), the reader named `to` (its
/// full and its short name): derived as its seal would be (the same
/// value, the same stand-in, whichever way it was read; a run without
/// the master has the value itself, no stand-in). Whether it is sealed
/// to it not yet, so its apply registers it, for the runs outside the
/// tree.
fn in_memory(
    of: &str,
    k: &str,
    o: &crate::stack::SecretOutput,
    v: &Value,
    (to, short): (&str, &str),
    master: &crate::custody::Master,
) -> bool {
    if let (Value::Str(text), Some(_)) = (v, &master.key) {
        let label = sealed_label(of, k, to);
        crate::secrets::standin::register(text, &label, &standin_of(&label, &o.digest));
    }
    !(o.sealed.contains_key(to) || o.sealed.contains_key(short)) && o.held.is_none()
}

pub(super) fn open_sealed(
    read: &mut [crate::stack::Read],
    deployment: &str,
    master: &crate::custody::Master,
) -> (Vec<String>, Vec<String>) {
    let mut opened = Vec::new();
    let mut unsealed = Vec::new();
    for r in read.iter_mut() {
        let Some(p) = &r.published else { continue };
        // Sealed to its full name, or (by a producer not applied since
        // R-200's full names) its short one: the label is the name sealed
        // to.
        let short = crate::stack::short_of(deployment);
        for (k, o) in &p.secret {
            // One the tree's run knows in memory (`stack::Planned`): no
            // seal is opened.
            if let Some(v) = r.opened.get(k) {
                let to = (deployment, short.as_str());
                if in_memory(&p.deployment, k, o, v, to, master) {
                    unsealed.push(format!("{}.{k}", r.name));
                }
                continue;
            }
            let Some((to, sealed)) = o
                .sealed
                .get_key_value(deployment)
                .or_else(|| o.sealed.get_key_value(&short))
            else {
                if o.held.is_none() && !o.digest.is_empty() {
                    eprintln!(
                        "{deployment}: {}.{k} is held by no provider and not sealed to it yet: \
                         apply {} again, which seals it to {deployment} (a reader from its \
                         first apply)",
                        r.name, r.name
                    );
                    unsealed.push(format!("{}.{k}", r.name));
                }
                continue;
            };
            // Its stand-in (R-164): a function of its keyed digest, which
            // stays while the value does; what a run without the master
            // reads in its place.
            let label = sealed_label(&p.deployment, k, to);
            let standin = standin_of(&label, &o.digest);
            let Some(key) = &master.key else {
                eprintln!(
                    "{deployment}: {}.{k} is sealed to it: opening it needs its master ({})",
                    r.name,
                    master.without.as_deref().unwrap_or("not held")
                );
                crate::secrets::standin::register(&standin, &label, &standin);
                r.opened.insert(k.clone(), Value::Str(standin));
                continue;
            };
            // Sealed to the current epoch's key, or (a producer not applied
            // since a cycle, R-165) an earlier one's.
            let earlier = master.earlier.iter().rev().filter_map(|e| e.key.as_ref());
            let plain = std::iter::once(key)
                .chain(earlier)
                .map(|k| crate::custody::open_sealed(k, &label, sealed))
                .find(Result::is_ok)
                .unwrap_or_else(|| crate::custody::open_sealed(key, &label, sealed));
            match plain.and_then(|b| Ok(serde_json::from_slice::<Value>(&b)?)) {
                Ok(v) => {
                    if let Value::Str(text) = &v {
                        crate::secrets::standin::register(text, &label, &standin);
                    }
                    r.opened.insert(k.clone(), v);
                    opened.push(format!("{}.{k}", r.name));
                }
                Err(e) => eprintln!("warning: {}.{k}: {e:#}", r.name),
            }
        }
    }
    (opened, unsealed)
}
