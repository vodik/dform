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
    let own_stack = own.split_once('[').map_or(own, |(s, _)| s);
    let mut loaded: std::collections::BTreeMap<PathBuf, Option<deployment::Loaded>> =
        Default::default();
    let mut out = Vec::new();
    for (name, entry) in crate::stack::registry(root)? {
        if name == own {
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
        let Ok(instance) = crate::stack::instance(&l.cfg, &l.stack, &l.program, &given) else {
            continue;
        };
        let (names, any) = crate::stack::reads(&l.program, &l.deployed, &instance.key);
        if names.contains(own) || any.contains(own_stack) {
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
pub(super) fn open_sealed(
    read: &mut [crate::stack::Read],
    deployment: &str,
    master: &crate::custody::Master,
) -> (Vec<String>, Vec<String>) {
    let mut opened = Vec::new();
    let mut unsealed = Vec::new();
    for r in read.iter_mut() {
        let Some(p) = &r.published else { continue };
        for (k, o) in &p.secret {
            let Some(sealed) = o.sealed.get(deployment) else {
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
            let label = sealed_label(&p.deployment, k, deployment);
            let standin = format!(
                "sealed-{}",
                &crate::approval::sha256_hex(format!("{label}\0{}", o.digest).as_bytes())[..32]
            );
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
