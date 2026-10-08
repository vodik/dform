use super::*;

/// One deployment `apply` applies, of the stack in `file`, and the
/// stack's inputs (not its key).
pub(super) struct Dependency {
    pub(super) name: String,
    pub(super) file: PathBuf,
    pub(super) keys: Vec<(String, String)>,
    pub(super) inputs: BTreeSet<String>,
    /// Its whole key, a key `keys` leaves out at its default.
    pub(super) key: Vec<(String, String)>,
    /// One of the roots the order was asked for, not a deployment one of
    /// them reads.
    pub(super) root: bool,
}

/// `apply X` in a project: the deployments of the project's stacks X
/// reads (a keyed read of a deployment, R-73), and theirs, each before its
/// readers, then X; nothing that reads X (R-30: the stack is the unit of
/// partial work).
/// Empty when X reads none, and for a plan file, a world fixture or a
/// program outside a project. A cycle is an error naming it.
pub(super) fn apply_order(cli: &Cli) -> Result<Vec<Dependency>> {
    let Cmd::Apply {
        plan_file: None,
        destroy: false,
        ..
    } = &cli.cmd
    else {
        return Ok(Vec::new());
    };
    if !cli.in_project || cli.world.is_some() {
        return Ok(Vec::new());
    }
    // The project (`apply` with no target): every stack, each with its
    // default key; else the target.
    let roots: Vec<(PathBuf, Vec<(String, String)>)> = match cli.files.as_slice() {
        [] => cli
            .every_stack
            .iter()
            .map(|f| (f.clone(), Vec::new()))
            .collect(),
        [one] => vec![(one.clone(), cli.keys.clone())],
        _ => return Ok(Vec::new()),
    };
    let mut order = order_of(&roots)?;
    if order.len() == 1 && cli.every_stack.is_empty() {
        order.clear();
    }
    Ok(order)
}

/// The deployments of `roots`, each a stack's file and the key values a
/// target gives, and those they read (R-30), each before its readers, in
/// the roots' order otherwise. A cycle is an error naming it.
pub(super) fn order_of(roots: &[(PathBuf, Vec<(String, String)>)]) -> Result<Vec<Dependency>> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let Some(project) = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?
    else {
        return Ok(Vec::new());
    };
    let found = crate::project::discover(&project);
    // The stack in `file` keyed by `keys`: its name and the deployments it
    // reads that are this project's.
    let reads = |file: &Path,
                 keys: &[(String, String)]|
     -> Result<(crate::stack::Instance, BTreeSet<String>, Vec<Dependency>)> {
        let t = deployment::Target {
            files: vec![file.to_path_buf()],
            input_files: Vec::new(),
            providers: Vec::new(),
        };
        let loaded = deployment::load(
            &t,
            env!("CARGO_PKG_VERSION"),
            &|p: &Path| std::fs::read_to_string(p),
            &mut deployment::Notes::default(),
        )?;
        // The key the target gives, a key it does not at its default.
        let given: Vec<crate::ast::Atom> = keys
            .iter()
            .map(|(k, v)| {
                crate::ast::atom(
                    "input",
                    vec![crate::ast::str_term(k), crate::ast::str_term(v)],
                    Default::default(),
                )
            })
            .collect();
        let instance = crate::stack::instance(&loaded.cfg, &loaded.stack, &loaded.program, &given)
            .unwrap_or_else(|_| crate::stack::Instance {
                stack: loaded.stack.clone(),
                key: keys.to_vec(),
                defaulted: Vec::new(),
            });
        let (mut names, any) =
            crate::stack::reads(&loaded.program, &loaded.deployed, &instance.key);
        // A deployment named by what the program computes: any of the
        // stack's may be read, so every one there is goes first.
        if !any.is_empty() {
            for name in crate::stack::registry(&project.state_root())?.into_keys() {
                let stack = name.split_once('[').map_or(name.as_str(), |(s, _)| s);
                if any.contains(stack) {
                    names.insert(name);
                }
            }
        }
        let mut deps = Vec::new();
        for name in names {
            let (stack, key) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((s, k)) => (s.to_string(), k),
                None => (name.clone(), ""),
            };
            // Another project's (`acme.platform`) is that project's to apply.
            let [one] = found.named(&stack)[..] else {
                continue;
            };
            let keys: Vec<(String, String)> = key
                .split(',')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .ok_or_else(|| anyhow::anyhow!("a read of {name}: expected K=V in the key"))
                })
                .collect::<Result<_>>()?;
            deps.push(Dependency {
                name,
                file: one.file.clone(),
                key: keys.clone(),
                keys,
                inputs: BTreeSet::new(),
                root: false,
            });
        }
        let inputs = loaded
            .program
            .statements
            .iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Input(i) if !i.key => Some(i.name.clone()),
                _ => None,
            })
            .collect();
        Ok((instance, inputs, deps))
    };
    let mut order: Vec<Dependency> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    type Reads<'a> = dyn Fn(
            &Path,
            &[(String, String)],
        ) -> Result<(crate::stack::Instance, BTreeSet<String>, Vec<Dependency>)>
        + 'a;
    fn visit(
        mut d: Dependency,
        reads: &Reads,
        order: &mut Vec<Dependency>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        if order.iter().any(|o| o.name == d.name) {
            return Ok(());
        }
        if let Some(i) = path.iter().position(|p| *p == d.name) {
            bail!(
                "apply {}: the stacks read each other's outputs in a cycle: {} -> {}",
                path[0],
                path[i..].join(" -> "),
                d.name
            );
        }
        let (_, inputs, deps) = reads(&d.file, &d.keys)?;
        d.inputs = inputs;
        path.push(d.name.clone());
        for dep in deps {
            visit(dep, reads, order, path)?;
        }
        path.pop();
        order.push(d);
        Ok(())
    }
    for (file, keys) in roots {
        let (instance, inputs, _) = reads(file, keys)?;
        let target = Dependency {
            name: instance.name(),
            file: file.clone(),
            keys: keys.clone(),
            inputs,
            key: instance.key,
            root: true,
        };
        // A root another root read first is a root still.
        if let Some(d) = order.iter_mut().find(|d| d.name == target.name) {
            d.root = true;
            continue;
        }
        visit(target, &reads, &mut order, &mut path)?;
    }
    Ok(order)
}
