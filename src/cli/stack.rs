use super::*;

/// `dform stack list`, a result set (R-63): one row per deployment with
/// state (a stack with none, one row saying so), its stack, file and
/// where its state is, its last apply and a pending saved plan.
pub(super) fn stack_list(cli: &Cli) -> Result<()> {
    use report::table::{Cell, Table};
    let project = crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let d = crate::project::discover(&project);
    for w in &d.warnings {
        eprintln!("warning: {w}");
    }
    d.check()?;
    if d.stacks.is_empty() {
        println!("no stacks in {}", project.root.display());
        return Ok(());
    }
    let registry = crate::stack::registry(&cli.root)?;
    // The project module's deployments (R-114): the matrix is the source
    // of which there are, the registry of where each one's state is.
    let (listed, label) = matrix::listed(&project, &cli.root);
    let mut t = Table::new([
        "stack",
        "file",
        "deployment",
        "listed",
        "state",
        "applied",
        "by",
        "commit",
        "result",
        "pending",
    ]);
    for s in &d.stacks {
        let key = if s.keys.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.keys.join(", "))
        };
        let row = |deployment: &str, state: String, last: LastApply| {
            let listed = match listed.get(deployment) {
                Some(true) => label.clone(),
                Some(false) => format!("removed from {label}"),
                None => String::new(),
            };
            [
                format!("{}{key}", s.name),
                s.file.display().to_string(),
                deployment.to_string(),
                listed,
                state,
                last.applied,
                last.by,
                last.commit,
                last.result,
                last.pending,
            ]
            .into_iter()
            .map(Cell::text)
            .collect::<Vec<_>>()
        };
        let failed = |e: &anyhow::Error| LastApply {
            result: format!("{e:#}"),
            ..Default::default()
        };
        // Where the stack's deployments are: its backend's, else the state
        // root's.
        let backend = stack_backend(s);
        let base = deployment::stack_location(&cli.root, &s.name, backend.as_ref());
        let shown = |l: &store::Location| match l {
            store::Location::S3(spec) => spec.to_string(),
            store::Location::Local(_) => String::new(),
        };
        let opener = open_s3(&cli.root, false);
        let keys = match base.open(&opener).and_then(|st| st.list("")) {
            Ok(k) => k,
            Err(e) => {
                t.push(row("", base.to_string(), failed(&e)));
                continue;
            }
        };
        let mut deployments: Vec<(String, store::Location)> = Vec::new();
        let keyed = backend.as_ref().and_then(crate::stack::keyed_parent);
        if s.keys.is_empty() {
            deployments.push((s.name.clone(), base.clone()));
        } else if let (Some(b), Some((parent, rest))) = (&backend, keyed) {
            // A backend that names the key (`local("state/app-{env}")`):
            // each place under its directory the template matches.
            let found = deployment::stack_location(&cli.root, &s.name, Some(&parent))
                .open(&opener)
                .and_then(|st| st.list(""))
                .unwrap_or_default();
            let mut segs: Vec<String> = found
                .iter()
                .filter_map(|k| crate::stack::template_key(&rest, &s.keys, k))
                .collect();
            segs.sort();
            segs.dedup();
            for seg in segs {
                deployments.push((
                    format!("{}[{seg}]", s.name),
                    deployment::deployment_location(&cli.root, &s.name, Some(b), Some(&seg)),
                ));
            }
        } else {
            let mut segs: Vec<&str> = keys
                .iter()
                .filter_map(|k| k.split_once('/').map(|(seg, _)| seg))
                .filter(|seg| seg.contains('='))
                .collect();
            segs.dedup();
            for seg in segs {
                deployments.push((format!("{}[{seg}]", s.name), base.child(Some(seg))));
            }
        }
        for (name, e) in &registry {
            let ours = name == &s.name || name.starts_with(&format!("{}[", s.name));
            if ours && !deployments.iter().any(|(n, _)| n == name) {
                deployments.push((name.clone(), e.state.clone()));
            }
        }
        // A deployment the module lists that has no state yet.
        let mut unapplied: Vec<&String> = listed
            .iter()
            .filter(|(n, l)| {
                **l && n.split_once('[').map_or(n.as_str(), |(st, _)| st) == s.name
                    && !deployments.iter().any(|(d, _)| d == *n)
            })
            .map(|(n, _)| n)
            .collect();
        let mut any = false;
        for (name, location) in deployments {
            let store = match location.open(&opener) {
                Ok(st) => st,
                Err(e) => {
                    any = true;
                    t.push(row(&name, location.to_string(), failed(&e)));
                    continue;
                }
            };
            let entries = crate::audit::Log::new(store.clone(), None)
                .entries()
                .unwrap_or_default();
            // A destroyed deployment is gone; its log stays (R-149).
            if (entries.is_empty() && store.get(store::STATE)?.is_none()) || destroyed(&entries) {
                if let Some((n, true)) = listed.get_key_value(&name) {
                    unapplied.push(n);
                }
                continue;
            }
            any = true;
            let state = match registry.get(&name).and_then(|e| e.backend.clone()) {
                Some(b) => format!("handed over to {b}"),
                None => shown(&location),
            };
            t.push(row(&name, state, last_apply(&entries)));
        }
        unapplied.sort();
        unapplied.dedup();
        for name in unapplied {
            any = true;
            let never = LastApply {
                applied: "never".into(),
                ..Default::default()
            };
            t.push(row(name, String::new(), never));
        }
        if !any {
            let none = LastApply {
                result: "no deployment has state".into(),
                ..Default::default()
            };
            t.push(row("", shown(&base), none));
        }
    }
    print!("{}", t.without_empty_columns().render(&cli.table));
    Ok(())
}

/// The audit log `entries` end in a `destroy` that completed: no apply
/// started since (R-149).
pub(super) fn destroyed(entries: &[serde_json::Value]) -> bool {
    entries
        .iter()
        .rev()
        .find_map(|e| match e["kind"].as_str() {
            Some("destroyed") => Some(true),
            Some("apply_start") => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// A deployment's last apply and pending saved plan, from its audit log.
#[derive(Debug, Default)]
pub(super) struct LastApply {
    pub(super) applied: String,
    pub(super) by: String,
    pub(super) commit: String,
    pub(super) result: String,
    pub(super) pending: String,
}

pub(super) fn last_apply(entries: &[serde_json::Value]) -> LastApply {
    let field = |e: &serde_json::Value, k: &str| e[k].as_str().unwrap_or("").to_string();
    let start = entries.iter().rposition(|e| e["kind"] == "apply_start");
    let mut out = match start {
        None => LastApply {
            applied: "never".into(),
            ..Default::default()
        },
        Some(i) => {
            let e = &entries[i];
            let end = entries[i..]
                .iter()
                .find(|e| e["kind"] == "apply_end")
                .map(|e| field(e, "result"))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "running or interrupted".into());
            LastApply {
                applied: field(e, "time"),
                by: field(e, "who"),
                commit: e["commit"]
                    .as_str()
                    .map(|c| crate::report::short_id(c).to_string())
                    .unwrap_or_default(),
                result: end,
                pending: String::new(),
            }
        }
    };
    let plan = entries
        .iter()
        .rposition(|e| e["kind"] == "plan" && e["file"].is_string() && e["digest"].is_string());
    if let Some(p) = plan
        && start.is_none_or(|s| p > s)
    {
        out.pending = format!(
            "{} ({})",
            field(&entries[p], "file"),
            field(&entries[p], "digest")
        );
    }
    out
}

/// `stack rekey`: the deployment whose state moves, and where to.
pub(super) struct Rekey {
    pub(super) from: crate::stack::Instance,
    pub(super) to: crate::stack::Instance,
}

/// `stack rekey STACK K=V...`: STACK is the program's stack, and keyed; the
/// pairs are the old key, then the new one, each naming every key input
/// once (the old one left out: the state from before the stack was keyed).
/// The run is set to the old values (the new ones for the unkeyed state),
/// so it evaluates the deployment whose state moves.
pub(super) fn rekey_args(
    cli: &mut Cli,
    cfg: &crate::stack::Stack,
    files: &[PathBuf],
    stack: &str,
    pairs: &[String],
) -> Result<Rekey> {
    let own = cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    if stack != own {
        bail!(
            "stack rekey {stack}: the program ({}) owns stack {own}",
            files[0].display()
        );
    }
    if cli.world.is_some() {
        bail!(
            "stack rekey {stack}: with --world the state sits beside the world file; \
             there is nothing to move"
        );
    }
    let keys: Vec<&str> = cfg.keys.iter().map(|(k, _)| k.as_str()).collect();
    if keys.is_empty() {
        bail!(
            "stack rekey {stack}: the stack has no key; key it first (`key env: ..` in its file)"
        );
    }
    let side = |pairs: &[String]| -> Result<Vec<(String, String)>> {
        for p in pairs {
            let Some((k, _)) = p.split_once('=') else {
                bail!("stack rekey {stack}: expected K=V, got '{p}'");
            };
            if !keys.contains(&k) {
                bail!(
                    "stack rekey {stack}: {k} is not a key of the stack (its key: {})",
                    keys.join(", ")
                );
            }
        }
        keys.iter()
            .map(|k| {
                let vs: Vec<&str> = pairs
                    .iter()
                    .filter_map(|p| p.split_once('='))
                    .filter(|(x, _)| x == k)
                    .map(|(_, v)| v)
                    .collect();
                match vs.as_slice() {
                    [v] => Ok((k.to_string(), v.to_string())),
                    [] => bail!("stack rekey {stack}: no value for the key input {k}"),
                    _ => bail!("stack rekey {stack}: {k} is given twice on one side"),
                }
            })
            .collect()
    };
    let n = keys.len();
    let (from, to) = if pairs.len() == n {
        (Vec::new(), side(pairs)?)
    } else if pairs.len() == 2 * n {
        (side(&pairs[..n])?, side(&pairs[n..])?)
    } else {
        bail!(
            "stack rekey {stack} K=V...: the old key, then the new one, each naming {}",
            keys.join(", ")
        );
    };
    if from == to {
        bail!("stack rekey {stack}: the old key and the new one are the same");
    }
    let run_as = if from.is_empty() { &to } else { &from };
    cli.set
        .retain(|kv| !kv.split_once('=').is_some_and(|(x, _)| keys.contains(&x)));
    cli.set
        .extend(run_as.iter().map(|(k, v)| format!("{k}={v}")));
    let instance = |key| crate::stack::Instance {
        stack: own.clone(),
        key,
        defaulted: Vec::new(),
    };
    Ok(Rekey {
        from: instance(from),
        to: instance(to),
    })
}

/// Where the deployment `name` (`app`, `app[env=prod]`) is, for a command
/// that runs no program: where the registry has it, else where its stack's
/// program's backend says, else under the state root. With its default
/// directory (its world's when its state is in a bucket) and the lease
/// times.
/// Before `stack handover NAME --to s3(..)`: a bucket never holds a key
/// file (R-164), so a deployment whose master is one is sealed first, as
/// dform.toml's `[secrets]` says (the same master: nothing derived
/// changes), and the sealed master is what moves; with no `[secrets]` the
/// handover is refused, naming the setting.
pub(super) fn seal_before_handover(
    cli: &Cli,
    name: &str,
    from: &crate::stack::Place,
    to: &str,
    s3: store::OpenS3,
    times: store::LeaseTimes,
) -> Result<()> {
    if !to.trim_start().starts_with("s3") {
        return Ok(());
    }
    let src = from.location.open(s3)?;
    if crate::zset::file::Key::load(src.as_ref())?.is_none() {
        return Ok(());
    }
    let stack = name.split_once('[').map_or(name, |(s, _)| s);
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let mixing = crate::custody::Mixing::of(project.as_ref().map(|p| &p.manifest), stack)?;
    if mixing.key_file() {
        bail!(
            "handover {name} to {to}: its master is the key file {}, and a bucket never holds \
             one (read access to the state would be read access to every derived secret): set \
             `[secrets] passphrase = \"env:NAME\"` (or `recipients = [\"age1..\"]`) in \
             dform.toml, and the handover seals it",
            src.locate(store::KEY)
        );
    }
    let dep = store::Deployment::new(src.clone(), name, times);
    let master = dep.master(&mixing, crate::custody::Want::default())?;
    if master.key.is_none() {
        bail!(
            "handover {name} to {to}: sealing its key file needs the master: {}",
            master.without.as_deref().unwrap_or("not held")
        );
    }
    match crate::custody::reseal(src.as_ref(), name, &master, &mixing)? {
        Some(done) if done.key_file => {
            dep.audit(cli.audit_sink.clone(), crate::audit::SINK_TIMEOUT, false)
                .append(
                    "custody",
                    serde_json::json!({
                        "sealed": store::KEY,
                        "into": store::MASTER,
                        "id": master.id,
                        "who": crate::audit::who(),
                    }),
                )?;
            eprintln!(
                "{name}: its key file is sealed into {} for the handover ({})",
                store::MASTER,
                mixing.describe()
            );
            Ok(())
        }
        _ => bail!(
            "handover {name} to {to}: its key file could not be sealed ({}): a bucket never holds \
             one",
            mixing.describe()
        ),
    }
}

pub(super) fn place_of(
    cli: &Cli,
    name: &str,
) -> Result<(crate::stack::Place, PathBuf, store::LeaseTimes)> {
    let root = &cli.root;
    let home = crate::stack::instance_dir(root, name);
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let times = project
        .as_ref()
        .map(|p| p.manifest.lease_times())
        .unwrap_or_default();
    let location = match crate::stack::registry(root)?.remove(name) {
        Some(e) => e.state,
        None => {
            let (stack, seg) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((stack, seg)) => (stack, Some(seg)),
                None => (name, None),
            };
            let backend = project.as_ref().and_then(|p| {
                let d = crate::project::discover(p);
                match d.named(stack).as_slice() {
                    [one] => stack_backend(one),
                    _ => None,
                }
            });
            deployment::deployment_location(root, stack, backend.as_ref(), seg)
        }
    };
    let place = crate::stack::Place {
        world: crate::stack::world_file(&location, &home),
        location,
    };
    Ok((place, home, times))
}

/// The backend of the project's stack `found`, as the manifest says;
/// `None` when it says none.
pub(super) fn stack_backend(found: &crate::project::Found) -> Option<crate::stack::Backend> {
    let program = loader::load_program(std::slice::from_ref(&found.file)).ok()?;
    crate::stack::config(&program).ok()?.backend
}
