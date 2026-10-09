//! What a run is given besides its program: `--set` and `--data` read,
//! a key input checked, and what a plan file records of them.

use super::Cli;
use crate::ast::{Atom, Term};
use crate::value::Value;
use crate::{deployment, zset};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The environment variables `env_var` reads, by label (`env.var/NAME`),
/// as a plan file records them: the label and the value's digest keyed
/// with the stack's plan key, as a secret input's. One not set now is
/// left out.
pub(super) fn env_inputs(
    labels: impl Iterator<Item = String>,
    key: Option<&zset::file::Key>,
) -> Vec<serde_json::Value> {
    labels
        .filter_map(|label| {
            let name = label.strip_prefix("env.var/")?;
            let v = std::env::var(name).ok()?;
            Some(keyed(&label, key, v.as_bytes()))
        })
        .collect()
}

/// A secret as a plan file records it: its label, and its digest keyed
/// with the deployment's master; a run that does not hold it records the
/// label alone, never an unkeyed digest (R-164).
pub(super) fn keyed(label: &str, key: Option<&zset::file::Key>, bytes: &[u8]) -> serde_json::Value {
    match key {
        Some(k) => serde_json::json!({ "sensitive": label, "digest": k.digest(bytes) }),
        None => serde_json::json!({ "sensitive": label }),
    }
}

/// A secret answer's label (`table.text.@document/LOCATION#3`) as the
/// program writes its call: `io.read("vault://kv/app#key")`.
pub(super) fn answer_text(label: &str) -> String {
    crate::value::null_parts(label)
        .map(|(pred, inputs, _)| crate::externs::call_text(&pred, &inputs))
        .unwrap_or_else(|| label.to_string())
}

/// The secret answers of dform's own externs (a location's read into a
/// secret `let`) as a plan file records them: each label and its value's
/// digest with the plan key, and the version a secret manager answered it
/// at (R-172).
pub(super) fn answer_inputs(
    externs: &crate::externs::Externs,
    key: Option<&zset::file::Key>,
) -> Vec<serde_json::Value> {
    let versions = externs.secret_versions();
    externs
        .secret_answers()
        .into_iter()
        .map(|(label, v)| {
            let bytes = match &v {
                Value::Str(s) => s.clone().into_bytes(),
                v => serde_json::to_vec(v).unwrap_or_default(),
            };
            let mut a = keyed(&label, key, &bytes);
            if let Some(v) = versions.get(&label) {
                a["version"] = serde_json::json!(v);
            }
            a
        })
        .collect()
}

impl Cli {
    /// This run's inputs as a plan file records them: each `--input-file` and
    /// a `--set` of a secret input by their digest with the stack's key (the
    /// latter with its label).
    pub(super) fn plan_inputs(
        &self,
        secret: &BTreeSet<String>,
        key: Option<&zset::file::Key>,
    ) -> Result<zset::file::Inputs> {
        let files = &self.files;
        let read = |f: &PathBuf| {
            std::fs::read(f).map_err(|e| anyhow::anyhow!("read {}: {e}", f.display()))
        };
        let show = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        Ok(zset::file::Inputs {
            files: files
                .iter()
                .map(|f| {
                    Ok(zset::file::FileDigest {
                        path: f.display().to_string(),
                        fnv64: zset::file::fnv64(&read(f)?),
                    })
                })
                .collect::<Result<_>>()?,
            input_files: self
                .input_files
                .iter()
                .map(|f| {
                    Ok(zset::file::KeyedDigest {
                        path: f.display().to_string(),
                        digest: match key {
                            Some(k) => k.digest(&read(f)?),
                            None => String::new(),
                        },
                    })
                })
                .collect::<Result<_>>()?,
            set: self
                .set
                .iter()
                .map(|kv| {
                    Ok(match kv.split_once('=') {
                        Some((k, v)) if secret.contains(k) => {
                            let label = crate::value::null_label(crate::modules::INPUT, "", k);
                            let bytes = match v.strip_prefix('@') {
                                Some(f) => read(&PathBuf::from(f))?,
                                None => v.as_bytes().to_vec(),
                            };
                            keyed(&label, key, &bytes)
                        }
                        // `k=@FILE`: the file's digest, keyed, as an
                        // `--input-file`'s.
                        Some((_, v)) if v.starts_with('@') => {
                            let bytes = read(&PathBuf::from(&v[1..]))?;
                            match key {
                                Some(k) => {
                                    serde_json::json!({ "set": kv, "digest": k.digest(&bytes) })
                                }
                                None => serde_json::json!({ "set": kv }),
                            }
                        }
                        _ => serde_json::Value::String(kv.clone()),
                    })
                })
                .collect::<Result<_>>()?,
            data: self.data.clone(),
            providers: self.providers.clone(),
            world: show(&self.world),
            inventory: show(&self.inventory),
            env: Vec::new(),
            answers: Vec::new(),
            stack_outputs: Vec::new(),
        })
    }
}

impl Cli {
    /// Load a plan file for `apply PLAN`, its deployment's plan (the one
    /// of a sequence this run is, `Cli::sequence`); its inputs fill every
    /// input flag the command line leaves out.
    pub(super) fn with_plan_inputs(&mut self, path: &Path) -> Result<zset::file::PlanFile> {
        let saved = match &self.sequence {
            Some(s) => {
                zset::file::Sequence::load(path)?
                    .deployments
                    .into_iter()
                    .nth(s.step)
                    .ok_or_else(|| {
                        anyhow::anyhow!("internal: no step {} in {}", s.step, path.display())
                    })?
                    .plan
            }
            None => zset::file::PlanFile::load(path)?,
        };
        let i = &saved.inputs;
        if self.files.is_empty() {
            self.files = i.files.iter().map(|f| PathBuf::from(&f.path)).collect();
        }
        if self.set.is_empty() {
            for s in &i.set {
                match s {
                    serde_json::Value::String(kv) => self.set.push(kv.clone()),
                    file if file["set"].is_string() => self
                        .set
                        .push(file["set"].as_str().unwrap_or_default().to_string()),
                    secret => {
                        let label = secret["sensitive"].as_str().unwrap_or_default();
                        let k = label.rsplit_once('#').map_or(label, |(_, k)| k);
                        bail!(
                            "plan file {}: input {k} is secret and the file holds only its digest; \
                             give every --set again (--set {k}=...)",
                            path.display()
                        );
                    }
                }
            }
        }
        if self.input_files.is_empty() {
            self.input_files = i
                .input_files
                .iter()
                .map(|f| PathBuf::from(&f.path))
                .collect();
        }
        if self.data.is_empty() {
            self.data = i.data.clone();
        }
        if self.providers.is_empty() {
            self.providers = i.providers.clone();
        }
        if self.world.is_none() {
            self.world = i.world.as_ref().map(PathBuf::from);
        }
        if self.inventory.is_none() {
            self.inventory = i.inventory.as_ref().map(PathBuf::from);
        }
        Ok(saved)
    }
}

pub(super) fn build_extra_facts(data: &[String]) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    for kv in data {
        let (k, v) = split_kv(kv)?;
        out.push(atom_kv("data", k, v));
    }
    Ok(out)
}

pub(super) fn split_kv(s: &str) -> Result<(&str, Value)> {
    let (k, raw) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected key=value, got '{s}'"))?;
    match raw.strip_prefix('@') {
        Some(path) => Ok((k, set_file(k, Path::new(path))?)),
        None => Ok((k, deployment::value_of(raw))),
    }
}

/// `--set k=@FILE`: the value the document FILE holds, read as the input's
/// type later as any `--set` is: YAML, JSON or TOML by its extension, or
/// a `.df` file of the one fact `k(value)`.
pub(super) fn set_file(k: &str, path: &Path) -> Result<Value> {
    let at = || format!("--set {k}=@{}", path.display());
    let ext = path.extension().and_then(|e| e.to_str());
    if !matches!(ext, Some("df" | "yaml" | "yml" | "json" | "toml")) {
        bail!("{}: a .yaml, .json, .toml or .df file", at());
    }
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", at()))?;
    match ext {
        Some("df") => {
            let program = crate::parser::parse_program(&text).with_context(at)?;
            match program.statements.as_slice() {
                [crate::ast::Stmt::Fact(a)] if a.pred == k => match a.args.as_slice() {
                    [t] => t.ground(),
                    _ => None,
                }
                .ok_or_else(|| anyhow::anyhow!("{}: the fact {k}(..) holds one value", at())),
                _ => bail!("{}: a .df file of one fact, `{k}(value)`", at()),
            }
        }
        Some(ext @ ("yaml" | "yml" | "json" | "toml")) => {
            let format = if ext == "yml" { "yaml" } else { ext };
            crate::tables::document(format, &text).with_context(at)
        }
        _ => bail!("{}: a .yaml, .json, .toml or .df file", at()),
    }
}

pub(super) fn atom_kv(pred: &str, k: &str, v: Value) -> Atom {
    Atom {
        pred: pred.to_string(),
        args: vec![Term::Val(Value::Str(k.to_string())), Term::Val(v)],
        record: None,
        span: Default::default(),
    }
}

/// The program's secret inputs, each by its address (`admin_pw`,
/// `db.password`) and the type inside `secret(..)`.
pub(super) fn secret_inputs_of(
    declared: &[crate::inputs::Declared],
) -> Vec<(String, crate::ast::TypeExpr)> {
    declared
        .iter()
        .filter_map(|d| match &d.decl.ty {
            crate::ast::TypeExpr::Apply(n, xs) if n == "secret" && xs.len() == 1 => {
                Some((d.address.clone()?, xs[0].clone()))
            }
            _ => None,
        })
        .collect()
}

impl Cli {
    /// A key input's value is the target's: `--set` of one is an error, as is
    /// a target key the stack does not have. A key the target does not name is
    /// its input's default, for `plan` and `apply` alike (the controller names
    /// every one: `run_controller`).
    pub(super) fn check_keys(&self, cfg: &crate::stack::Stack, stack: &str) -> Result<()> {
        let keys: Vec<&str> = cfg.keys.iter().map(|(k, _)| k.as_str()).collect();
        for kv in &self.user_set {
            if let Some((k, _)) = kv.split_once('=')
                && keys.contains(&k)
            {
                bail!(
                    "--set {kv}: {k} is stack {stack}'s key; name the deployment in the target: \
                     `dform plan {stack} {kv}`"
                );
            }
        }
        for (k, _) in &self.keys {
            if !keys.contains(&k.as_str()) {
                if keys.is_empty() {
                    bail!("{k}=...: stack {stack} has no key; give an input with `--set {k}=...`");
                }
                bail!(
                    "{k} is not a key of stack {stack} (its key: {}); give an input with `--set {k}=...`",
                    keys.join(", ")
                );
            }
        }
        Ok(())
    }
}
