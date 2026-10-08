use super::*;

/// When the deployment's master was first applied, from its audit log: its
/// first `master` entry, else (a deployment applied before they were
/// logged) its first apply.
pub(super) fn born(entries: Option<&Vec<serde_json::Value>>) -> Option<String> {
    let es = entries?;
    es.iter()
        .find(|e| e["kind"] == "master")
        .or_else(|| es.iter().find(|e| e["kind"] == "apply_start"))
        .and_then(|e| e["time"].as_str().map(str::to_string))
}

/// `dform secrets list` (R-161): each secret by key, never a value.
/// A deployment as a command names it: `apps env=lab`.
pub(super) fn written(i: &crate::stack::Instance) -> String {
    let mut out = i.stack.clone();
    for (k, v) in &i.key {
        out.push_str(&format!(" {k}={v}"));
    }
    out
}

/// Each given secret a file of them holds (R-108): where it lives, its
/// generation and when it was set.
pub(super) fn given_rows(
    list: &mut [crate::secrets::inventory::Secret],
    files: &[crate::custody::given::Read],
    mixing: &crate::custody::Mixing,
) {
    for r in files {
        let Some(f) = &r.file else { continue };
        for l in &f.leaves {
            let name = l.name();
            let Some(s) = list.iter_mut().find(|s| s.key == name) else {
                continue;
            };
            s.lives = Some(format!(
                "{}, {}",
                r.shown,
                crate::custody::given::sealed_to(f, &|k| mixing.name_of(k))
            ));
            if let Some(g) = f.given(&name) {
                s.generation = g.generation;
                s.since = Some(g.at.clone());
            }
        }
    }
}

/// A file of given secrets, as `secrets list` ends: its values and who
/// opens it.
pub(super) fn print_given_file(r: &crate::custody::given::Read, mixing: &crate::custody::Mixing) {
    match &r.file {
        None => println!(
            "{}: not written yet: `dform secrets set` writes it",
            r.shown
        ),
        Some(f) => println!(
            "{}: {} given secret{}, {}",
            r.shown,
            f.leaves.len(),
            if f.leaves.len() == 1 { "" } else { "s" },
            crate::custody::given::sealed_to(f, &|k| mixing.name_of(k))
        ),
    }
}

/// `dform secrets set NAME` and `unset` (R-108): the value read from stdin
/// or the terminal, sealed into the file of given secrets the program
/// reads, every other value kept; a `given` entry in the audit log.
pub(super) fn secrets_set(
    deployment: &str,
    secret: &[(String, crate::ast::TypeExpr)],
    name: &str,
    remove: bool,
    files: &[crate::custody::given::Read],
    (mixing, master): (&crate::custody::Mixing, &crate::custody::Master),
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::custody::given;
    let verb = match remove {
        true => "unset",
        false => "set",
    };
    let holds = |r: &&given::Read| {
        r.file
            .as_ref()
            .is_some_and(|f| f.leaves.iter().any(|l| l.name() == name))
    };
    let read = match files {
        [] => bail!(
            "secrets {verb} {name}: {deployment} reads no file of given secrets; read one into \
             its secret inputs, `set from secrets.decode(io.read(\"secrets/{}.json\"))`",
            deployment.split('[').next().unwrap_or(deployment)
        ),
        [r] => r,
        rs => match rs.iter().find(holds) {
            Some(r) => r,
            None => bail!(
                "secrets {verb} {name}: {deployment} reads {} files of given secrets ({}), and \
                 none holds {name}; give it in one of them with sops, then set it here",
                rs.len(),
                rs.iter()
                    .map(|r| r.shown.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    let ty = secret.iter().find(|(a, _)| a == name).map(|(_, t)| t);
    if !remove && ty.is_none() {
        bail!(
            "secrets set {name}: {name} is not a secret input of {deployment} ({}); declare it \
             `input {name}: secret(string)`",
            match secret.is_empty() {
                true => "it declares none".to_string(),
                false => format!(
                    "its secret inputs: {}",
                    secret
                        .iter()
                        .map(|(a, _)| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        );
    }
    let Some(path) = &read.path else {
        bail!(
            "secrets {verb} {name}: {} is not a file of the project, and `secrets {verb}` writes \
             one; read a project file, `io.read(\"secrets/..\")`",
            read.shown
        );
    };
    let stack_key = match given::to_master(mixing) {
        false => None,
        true => match &master.digest {
            Some(k) => Some(given::stack_recipient(k)),
            None => bail!(
                "secrets {verb} {name}: {} is sealed to {deployment}'s master, which this run \
                 does not hold ({})",
                read.shown,
                master.without.as_deref().unwrap_or("no master")
            ),
        },
    };
    let to = given::To {
        recipients: mixing.recipients.iter().map(|r| r.key.clone()).collect(),
        stack_key,
    };
    // The file as it is, every value opened.
    let file = read.file.clone().unwrap_or_default();
    let (values, key) = match &read.file {
        None => (Vec::new(), None),
        Some(f) => {
            let ids = given::identities()?;
            let Some(k) = given::data_key(f, &ids)? else {
                bail!(
                    "secrets {verb} {name}: {}: {}",
                    read.shown,
                    given::why_not(f, &ids, &|r| mixing.name_of(r))
                );
            };
            (given::open(f, &k, &read.shown)?, Some(k))
        }
    };
    if remove && !values.iter().any(|(l, _)| l.name() == name) {
        bail!(
            "secrets unset {name}: {} gives no {name} ({})",
            read.shown,
            match values.is_empty() {
                true => "it gives none".to_string(),
                false => format!(
                    "it gives {}",
                    values
                        .iter()
                        .map(|(l, _)| l.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        );
    }
    let plain = match (remove, ty) {
        (false, Some(ty)) => Some(given::typed(
            name,
            ty,
            given::ask(&format!("{name} of {deployment}"))?,
        )?),
        _ => None,
    };
    let who = crate::audit::who();
    let next = given::with(
        &file,
        (&values, key),
        name,
        plain,
        &to,
        (&who, &crate::memo::now()),
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("make the directory of {}", read.shown))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, given::text(&next)?)
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("write {}", read.shown))?;
    let generation = next.given(name).map(|g| g.generation);
    audit.append(
        "given",
        serde_json::json!({
            "key": name,
            "file": read.shown,
            "generation": generation,
            "removed": remove,
            "recipients": next
                .recipients()
                .iter()
                .map(|k| match next.stack_key() == Some(k.as_str()) {
                    true => "the deployment's master".to_string(),
                    false => mixing.name_of(k),
                })
                .collect::<Vec<_>>(),
            "who": who,
        }),
    )?;
    match generation {
        Some(g) => println!(
            "sealed {name} of {deployment} into {} (generation {g}), {}; commit it: the next plan \
             reads it",
            read.shown,
            given::sealed_to(&next, &|k| mixing.name_of(k))
        ),
        None => println!(
            "removed {name} of {deployment} from {}; commit it: the next plan reads it",
            read.shown
        ),
    }
    Ok(())
}

pub(super) fn secrets_list(
    deployment: &str,
    list: &[crate::secrets::inventory::Secret],
    current: u32,
    json: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    let mut t = Table::new([
        "key",
        "kind",
        "generation",
        "epoch",
        "age",
        "read by",
        "lands",
    ]);
    for s in list {
        // A given secret's from its file (R-108), which says when it was
        // set; a managed secret's is its version in its manager (R-172).
        let generation = match s.kind {
            crate::secrets::inventory::Kind::Random | crate::secrets::inventory::Kind::Memo => {
                s.generation.to_string()
            }
            crate::secrets::inventory::Kind::Given if s.since.is_some() => s.generation.to_string(),
            crate::secrets::inventory::Kind::Managed => s.version.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let cells: Vec<String> = s.cells.iter().map(|c| c.to_string()).collect();
        let read = match cells.is_empty() {
            true => s
                .lives
                .clone()
                .map(|l| format!("(lives in {l})"))
                .unwrap_or_default(),
            false => cells.join(", "),
        };
        t.push(vec![
            Cell::text(s.key.clone()),
            Cell::text(s.kind.word()),
            Cell::text(generation.clone()).with_json(match s.generation {
                _ if generation.is_empty() => serde_json::Value::Null,
                _ if s.kind == crate::secrets::inventory::Kind::Managed => {
                    generation.clone().into()
                }
                g => g.into(),
            }),
            // The master epoch (R-165), once there is more than one.
            Cell::text(match (s.epoch, current) {
                (Some(e), c) if c > 1 && e < c => format!("{e} (earlier)"),
                (Some(e), c) if c > 1 => e.to_string(),
                _ => String::new(),
            })
            .with_json(s.epoch.into()),
            Cell::text(
                s.since
                    .as_deref()
                    .map(crate::secrets::inventory::age)
                    .unwrap_or_default(),
            )
            .with_json(s.since.clone().into()),
            Cell::text(read).with_json(cells.into()),
            Cell::text(s.lands().map(|l| l.words()).unwrap_or_default()),
        ]);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&t.json())?);
        return Ok(());
    }
    println!(
        "{deployment}: {} secret{}",
        list.len(),
        if list.len() == 1 { "" } else { "s" }
    );
    print!("{}", t.without_empty_columns().render(o));
    let earlier = list
        .iter()
        .filter(|s| s.epoch.is_some_and(|e| e < current))
        .count();
    if earlier > 0 {
        println!(
            "epoch {current} is current; {earlier} secret{} on an earlier epoch: rotate each to \
             move it",
            if earlier == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Who opens an epoch of the master, and who could: `secrets list`'s
/// lines after the table, the offboarding list.
pub(super) fn print_holders(h: &crate::custody::Holders) {
    let which = match h.current {
        true => "current",
        false => "earlier",
    };
    println!(
        "master epoch {} ({which}, id {}): opens with {}",
        h.epoch,
        crate::report::short_id(&h.id),
        match h.opens.is_empty() {
            true => "nothing dform.toml names".to_string(),
            false => h.opens.join(", "),
        }
    );
    if !h.could.is_empty() {
        println!(
            "  could also be opened by {} (removed since it began): each secret on it is theirs \
             until rotated off it",
            h.could
                .iter()
                .map(|(n, at)| format!("{n} (removed {at})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Recipients as an audit entry names them: `{key, name}`.
pub(super) fn recipients_json(rs: &[crate::custody::Recipient]) -> serde_json::Value {
    rs.iter()
        .map(|r| serde_json::json!({ "key": r.key, "name": r.name }))
        .collect()
}

/// `dform secrets rotate KEY` (R-161): the key's generation moves on (a
/// memo forgets what it keeps), in state under its lock and in the audit
/// log; what reads it, and how a new value lands there, said first.
pub(super) fn secrets_rotate(
    dep: &crate::store::Deployment,
    list: &[crate::secrets::inventory::Secret],
    kept: &std::collections::BTreeMap<String, crate::memo::Kept>,
    key: &str,
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::secrets::inventory::Kind;
    let deployment = dep.name();
    // A memo state keeps that is no secret (a time, a name) is forgotten
    // the same way.
    let plain;
    let found = match list.iter().find(|s| s.key == key) {
        Some(s) => Some(s),
        None if kept.contains_key(key) => {
            plain = crate::secrets::inventory::Secret::memo(key);
            Some(&plain)
        }
        None => None,
    };
    let Some(s) = found else {
        let keys: Vec<&str> = list
            .iter()
            .filter(|s| matches!(s.kind, Kind::Random | Kind::Memo))
            .map(|s| s.key.as_str())
            .collect();
        bail!(
            "secrets rotate {key}: {deployment} has no secret {key} (its keys: {}); a key the \
             program no longer derives is no secret to rotate",
            match keys.is_empty() {
                true => "none".to_string(),
                false => keys.join(", "),
            }
        );
    };
    if let (Kind::Given | Kind::Held | Kind::Managed, lives) = (s.kind, &s.lives) {
        bail!(
            "secrets rotate {key}: {key} is {} in {deployment}, and lives in {}: rotate it \
             there, then plan",
            s.kind.word(),
            lives.as_deref().unwrap_or("its source")
        );
    }
    if !dep.has_state()? {
        bail!(
            "secrets rotate {key}: {deployment} was never applied: its secrets are new at its \
             first apply"
        );
    }
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    let who = crate::audit::who();
    let (r, memo) = st.rotate(key, &crate::memo::now(), &who);
    // What the next plan changes, and how (R-161's blast radius).
    println!(
        "rotating {key} of {deployment} ({}): generation {} -> {}",
        s.kind.word(),
        r.generation - 1,
        r.generation
    );
    for c in &s.cells {
        println!("  {c}  {}", c.lands.words());
    }
    if memo.is_some() {
        println!("  forgot what memo.first keeps: the next apply keeps its candidate");
    } else if s.cells.is_empty() {
        println!("  (nothing reads it)");
    }
    dep.save_state(&st)?;
    audit.append(
        "rotated",
        serde_json::json!({
            "key": key,
            "kind": s.kind.word(),
            "generation": r.generation,
            "memo": memo.is_some(),
            "who": who,
        }),
    )?;
    println!(
        "rotated {key} of {deployment}: generation {}, by {who}; the next plan changes it",
        r.generation
    );
    lock.release()
}

/// `dform secrets cycle` (R-165): each `random.*` key pinned to the epoch
/// it derives from, then a new master made the next epoch, recorded in
/// state and the audit log. No value changes.
pub(super) fn secrets_cycle(
    dep: &crate::store::Deployment,
    list: &[crate::secrets::inventory::Secret],
    master: &crate::custody::Master,
    mixing: &crate::custody::Mixing,
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::secrets::inventory::Kind;
    let deployment = dep.name();
    if !dep.has_state()? {
        bail!("secrets cycle: {deployment} was never applied: its first apply makes its master");
    }
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    let from = master.epoch.max(1);
    // Pinned first: a key pinned to the epoch it derives from is the same
    // value, so a cycle stopped here changes nothing.
    let pinned = st.pin(
        list.iter()
            .filter(|s| s.kind == Kind::Random)
            .map(|s| s.key.clone()),
        from,
    );
    dep.save_state(&st)?;
    let (epoch, id) = crate::custody::cycle(dep.store().as_ref(), deployment, master, mixing)?;
    let was = st.master.replace(id.clone());
    dep.save_state(&st)?;
    audit.append(
        "cycled",
        serde_json::json!({
            "from": was,
            "to": id,
            "epoch": epoch,
            "pinned": pinned,
            "who": crate::audit::who(),
        }),
    )?;
    let on: Vec<&str> = list
        .iter()
        .filter(|s| matches!(s.kind, Kind::Random | Kind::Memo))
        .map(|s| s.key.as_str())
        .collect();
    println!(
        "cycled the master of {deployment}: epoch {epoch} (id {}) is current, for new secrets and \
         each one rotated; {} stay{} on epoch {from} until rotated{}",
        crate::report::short_id(&id),
        match on.len() {
            1 => "1 secret".to_string(),
            n => format!("{n} secrets"),
        },
        if on.len() == 1 { "s" } else { "" },
        match on.is_empty() {
            true => String::new(),
            false => format!(": {}", on.join(", ")),
        }
    );
    lock.release()
}
