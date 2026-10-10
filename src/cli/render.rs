//! `dform render [TARGET] [K=V..]` (R-202): a deployment's planned
//! documents as the objects their provider sends its API, for a tool
//! that applies them itself: Argo CD's config management plugin,
//! kustomize, `kubectl apply -f -`. dform as a generator.
//!
//! The program is evaluated as `test` evaluates a combination (no
//! credentials, no provider configured, the providers' offline schemas,
//! an image's digest and a provider's location stood in), with the
//! inputs the target and `--set` give and the rest their defaults; its
//! policy holds or the render is refused as the plan is (exit 4). No
//! provider's Plan is asked and nothing is read or written.
//!
//! A provider has a document form when the object it sends is the
//! document itself: Kubernetes's, the object with its `apiVersion` and
//! `kind`. A resource of any other provider (OVH, Postgres, Vault: an API
//! of calls, not objects) is said on stderr, `not rendered`, once. A
//! document with a hole (a value only an apply makes, a secret, an
//! undetermined deny) cannot be emitted: the render refuses naming each
//! cell, or with `--partial` prints the documents that have none and says
//! the rest on stderr.

use super::run_inputs::split_kv;
use super::test::Space;
use super::{Cli, Dependency, Held, Outcome};
use crate::address::Address;
use crate::plugin::Providers;
use crate::plugin::backend::KUBERNETES;
use crate::query::Redactor;
use crate::render::{Documents, Hole, Why, Wire};
use crate::resources::Resource;
use crate::value::Value;
use crate::{crd, deployment, query};
use anyhow::{Result, bail};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// `dform render [TARGET] [--json] [--partial]`.
#[derive(Debug, Clone)]
pub(super) struct Render {
    pub(super) json: bool,
    pub(super) partial: bool,
}

impl Render {
    /// The deployment's documents of a provider with a document form, in
    /// the plan's order, as a YAML stream (`--json`: an array) on stdout;
    /// what is not rendered, and why, on stderr.
    pub(super) fn run(&self, cli: &Cli, loaded: &deployment::Loaded) -> Result<Outcome> {
        let space = Space::new(cli, loaded)?;
        let mut pairs = Vec::new();
        for kv in &cli.set {
            let (k, v) = split_kv(kv)?;
            pairs.push((k.to_string(), v));
        }
        let e = space.evaluated(&pairs)?;
        let schema = space.backend.schema();
        let redact = query::Redactor::new(&e.res.facts, schema);
        cli.cmd.blocked(&e.violations, &redact)?;
        let mut rendered = Rendered::new(&e.resources, &space.backend, &redact)?;
        rendered.undecided =
            crate::render::undecided(&e.res, |t| provider(&space.backend, t) == KUBERNETES);
        for n in space.notes() {
            eprintln!("{n}");
        }
        for line in rendered.skipped() {
            eprintln!("{line}");
        }
        match self.partial {
            true => rendered.holes().iter().for_each(|l| eprintln!("{l}")),
            false => rendered.refuse_holes(&deployment_name(cli, loaded))?,
        }
        let text = match self.json {
            true => format!("{}\n", serde_json::to_string_pretty(&rendered.objects)?),
            false => crate::render::yaml_stream(&rendered.objects, &LEAD)?,
        };
        cli.held.print(&text);
        Ok(Outcome::Done)
    }
}

/// The provider that serves `typ`, by name: the schema's, else the one
/// its namespace routes to (a kind a CRD of the program defines).
fn provider<'a>(backend: &'a Providers, typ: &str) -> &'a str {
    match backend.schema().provider_of.get(typ) {
        Some(p) => p,
        None => backend.provider_of(typ),
    }
}

/// The keys a Kubernetes object is read by, first in its YAML.
const LEAD: [&str; 3] = ["apiVersion", "kind", "metadata"];

/// The deployment as the user writes it: `stacks.dform[env=staging]`.
fn deployment_name(cli: &Cli, loaded: &deployment::Loaded) -> String {
    match cli.keys.is_empty() {
        true => loaded.path.clone(),
        false => {
            let keys: Vec<String> = cli.keys.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{}[{}]", loaded.path, keys.join(","))
        }
    }
}

/// What a render made of one evaluation: the objects, in order; the
/// resources of a provider with no document form, and why; and the
/// holes.
#[derive(Default)]
struct Rendered {
    objects: Vec<Json>,
    /// Each resource not rendered for what it is, with why.
    skipped: Vec<(Address, String)>,
    /// The cells it cannot fill.
    holes: Vec<Hole>,
    /// The denies, and whether resources are made, that read a value
    /// only an apply makes (`render::undecided`).
    undecided: Vec<String>,
}

impl Rendered {
    /// The documents of `resources` whose provider has a document form,
    /// as that form, in the plan's order; each other one with why.
    fn new(resources: &[Resource], backend: &Providers, redact: &Redactor) -> Result<Rendered> {
        let schema = backend.schema();
        let mut out = Rendered::default();
        let docs = Documents::new(resources, redact);
        let mut k8s: Option<Kubernetes> = None;
        let mut known: Vec<(&Resource, Json)> = Vec::new();
        for r in resources {
            let provider = provider(backend, &r.addr.typ);
            if provider != KUBERNETES {
                let why = format!("{provider} has no document form");
                out.skipped.push((r.addr.clone(), why));
                continue;
            }
            let form = match &mut k8s {
                Some(f) => f,
                None => k8s.insert(Kubernetes::new(resources)?),
            };
            match docs.wire(r, schema)? {
                Wire::Holes(h) => out.holes.extend(h),
                Wire::Known(doc) => match form.object(&r.addr.typ, doc) {
                    Ok(o) => known.push((r, o)),
                    Err(why) => out.skipped.push((r.addr.clone(), why)),
                },
            }
        }
        if let Some(form) = &k8s {
            let extra = form.edges(&known);
            let objects: BTreeMap<&Address, &Json> =
                known.iter().map(|(r, o)| (&r.addr, o)).collect();
            let rs: Vec<&Resource> = known.iter().map(|(r, _)| *r).collect();
            out.objects = crate::render::order(&rs, &extra)
                .into_iter()
                .map(|r| objects[&r.addr].clone())
                .collect();
        }
        Ok(out)
    }

    /// Each resource not rendered for what it is, a line each: its
    /// provider has no document form, or its kind is not known.
    fn skipped(&self) -> Vec<String> {
        self.skipped
            .iter()
            .map(|(a, why)| format!("not rendered: {} ({why})", crate::report::address(a)))
            .collect()
    }

    /// Each hole, a line: a cell, a deny or a resource undecided.
    fn holes(&self) -> Vec<String> {
        let cells = self.holes.iter().map(|h| h.to_string());
        cells
            .chain(self.undecided.iter().cloned())
            .map(|h| format!("hole: {h}"))
            .collect()
    }

    /// The render's refusal when it has a hole: a generator cannot emit
    /// a document with one (`--partial` emits the rest).
    fn refuse_holes(&self, name: &str) -> Result<()> {
        let n = self.holes.len() + self.undecided.len();
        if n == 0 {
            return Ok(());
        }
        let count = match n {
            1 => "1 of what its documents need is".to_string(),
            n => format!("{n} of what its documents need are"),
        };
        let mut lines = vec![format!(
            "render {name}: {count} not known to a render, and a rendered document has no \
             holes:"
        )];
        lines.extend(self.holes());
        // The fix for what the holes are: an apply makes a value; a
        // secret reaches the cluster another way.
        let secret = self.holes.iter().any(|h| matches!(h.why, Why::Secret(_)));
        let applied =
            self.holes.iter().any(|h| !matches!(h.why, Why::Secret(_))) || n > self.holes.len();
        let fix = match (applied, secret) {
            (true, false) => "apply the deployment first, so a value an apply makes is known",
            (false, true) => {
                "give the cluster a secret another way (an operator that reads \
                              a secret store)"
            }
            _ => "apply the deployment first, and give the cluster a secret another way",
        };
        lines.push(format!(
            "help: {fix}; or render the documents with none with `--partial`"
        ));
        bail!(lines.join("\n  "))
    }
}

/// Kubernetes's document form: the object the provider applies
/// (`dform_k8s::object::manifest`), the document with `apiVersion` and
/// `kind` from its type, as the provider's snapshot serves the type or a
/// CRD the program makes defines it (R-126). Never `status` or
/// `metadata.managedFields`; no label or annotation of dform's, so a
/// consumer applies it as it is.
struct Kubernetes {
    /// Each type's `apiVersion` and `kind`.
    kinds: BTreeMap<String, (String, String)>,
    /// The CRD each type a CRD of the program defines is defined by.
    defined_by: BTreeMap<String, Address>,
}

impl Kubernetes {
    fn new(resources: &[Resource]) -> Result<Kubernetes> {
        let mut kinds = snapshot_kinds()?;
        let mut defined_by = BTreeMap::new();
        for r in resources
            .iter()
            .filter(|r| crd::TYPES.contains(&r.addr.typ.as_str()))
        {
            for (typ, api_version, kind) in crd_kinds(&r.attrs) {
                defined_by.insert(typ.clone(), r.addr.clone());
                kinds.entry(typ).or_insert((api_version, kind));
            }
        }
        Ok(Kubernetes { kinds, defined_by })
    }

    /// The object of `doc`, a document of `typ`; `Err` says why there is
    /// none: its kind is not known. An `apiVersion` and `kind` the
    /// document writes must be its type's.
    fn object(&self, typ: &str, doc: Json) -> std::result::Result<Json, String> {
        let Json::Object(mut m) = doc else {
            return Err(format!("a {typ} document is an object"));
        };
        m.remove("status");
        if let Some(Json::Object(meta)) = m.get_mut("metadata") {
            meta.remove("managedFields");
        }
        let written = |k: &str| m.get(k).and_then(Json::as_str).map(str::to_string);
        let (api_version, kind) =
            match (self.kinds.get(typ), written("apiVersion"), written("kind")) {
                (Some(k), _, _) => k.clone(),
                (None, Some(a), Some(k)) => (a, k),
                (None, _, _) => {
                    return Err(format!(
                        "{typ} is no kind of the Kubernetes provider's snapshot, and no CRD the \
                     program makes defines it"
                    ));
                }
            };
        for (k, want) in [("apiVersion", &api_version), ("kind", &kind)] {
            if let Some(v) = written(k).filter(|v| v != want) {
                return Err(format!("{k} is {v:?}; the type says {want:?}"));
            }
        }
        let mut out = serde_json::Map::new();
        out.insert("apiVersion".into(), json!(api_version));
        out.insert("kind".into(), json!(kind));
        out.extend(m);
        Ok(Json::Object(out))
    }

    /// What a consumer applies first beyond what a document references:
    /// a CRD before the objects of its kinds (a plan's later tick), a
    /// Namespace before the objects in it.
    fn edges(&self, known: &[(&Resource, Json)]) -> BTreeMap<Address, BTreeSet<Address>> {
        let namespaces: BTreeMap<&str, &Address> = known
            .iter()
            .filter(|(_, o)| o.get("kind").and_then(Json::as_str) == Some("Namespace"))
            .filter_map(|(r, o)| Some((o.pointer("/metadata/name")?.as_str()?, &r.addr)))
            .collect();
        let mut out: BTreeMap<Address, BTreeSet<Address>> = BTreeMap::new();
        for (r, o) in known {
            let before = self.defined_by.get(&r.addr.typ).into_iter().chain(
                o.pointer("/metadata/namespace")
                    .and_then(Json::as_str)
                    .and_then(|ns| namespaces.get(ns).copied()),
            );
            out.entry(r.addr.clone())
                .or_default()
                .extend(before.cloned());
        }
        out
    }
}

/// Each type of the Kubernetes provider's snapshot (its full name and
/// the short names it serves, `k8s.deployment`) with its `apiVersion` and
/// `kind`: what its derived schema's kinds say (`dform_k8s::openapi`),
/// read without deriving the rest.
fn snapshot_kinds() -> Result<BTreeMap<String, (String, String)>> {
    use dform_k8s::openapi;
    let doc: Json = serde_json::from_str(openapi::SNAPSHOT)?;
    let mut out = BTreeMap::new();
    let gvs = doc.get("paths").and_then(Json::as_object);
    for gv in gvs.into_iter().flat_map(|m| m.values()) {
        let paths = gv.get("paths").and_then(Json::as_object);
        for (path, item) in paths.into_iter().flatten() {
            let gvk = item.pointer("/patch/x-kubernetes-group-version-kind");
            let (true, Some(gvk)) = (path.ends_with("/{name}"), gvk) else {
                continue;
            };
            let s = |k: &str| gvk.get(k).and_then(Json::as_str).unwrap_or("");
            let api_version = match s("group") {
                "" => s("version").to_string(),
                g => format!("{g}/{}", s("version")),
            };
            let typ = openapi::type_name(s("group"), s("version"), s("kind"));
            out.insert(typ, (api_version, s("kind").to_string()));
        }
    }
    for (alias, typ) in openapi::aliases()? {
        if let Some(k) = out.get(&typ).cloned() {
            out.insert(alias, k);
        }
    }
    Ok(out)
}

/// The types a CRD document defines (`crd::defines`), each with its
/// `apiVersion` and `kind`: each served version's, and the short name at
/// the preferred one.
fn crd_kinds(doc: &Value) -> Vec<(String, String, String)> {
    let at = |path: &[&str]| {
        path.iter().try_fold(doc, |v, k| match v {
            Value::Obj(m) => m.get(*k),
            _ => None,
        })
    };
    let s = |path: &[&str]| at(path).and_then(Value::as_str).map(str::to_string);
    let (Some(group), Some(kind)) = (s(&["spec", "group"]), s(&["spec", "names", "kind"])) else {
        return Vec::new();
    };
    let versions: Vec<String> = match at(&["spec", "versions"]) {
        Some(Value::List(vs)) => vs
            .iter()
            .filter(|v| !matches!(v, Value::Obj(m) if m.get("served") == Some(&Value::Bool(false))))
            .filter_map(|v| match v {
                Value::Obj(m) => m.get("name").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let api = |v: &str| format!("{group}/{v}");
    let mut out: Vec<(String, String, String)> = versions
        .iter()
        .map(|v| (crd::type_name(&group, v, &kind), api(v), kind.clone()))
        .collect();
    if let Some(v) = crd::preferred(versions.iter().map(String::as_str)) {
        out.push((crd::short_name(&group, &kind), api(v), kind.clone()));
    }
    out
}

/// `render` on the project module: each deployment it lists, its YAML
/// after a `# NAME` line (`--json`: an array of `{deployment, objects}`);
/// one that fails is said and the others rendered.
pub(super) fn matrix(
    cli: &Cli,
    order: &[Dependency],
    of: &dyn Fn(&Dependency, super::Cmd) -> Cli,
) -> Result<Outcome> {
    let super::Cmd::Render(r) = &cli.cmd else {
        bail!("internal: a render");
    };
    let mut outcome = Outcome::Done;
    let mut docs = Vec::new();
    for d in order.iter().filter(|d| d.root) {
        let mut dep = of(d, cli.cmd.clone());
        dep.held = Held::new();
        let result = super::run(dep.clone(), None);
        let text = dep.held.take().text;
        match r.json {
            false => print!("# {}\n{text}", d.full),
            true => {
                let objects: Json = serde_json::from_str(&text).unwrap_or_else(|_| json!([]));
                docs.push(json!({"deployment": d.full, "objects": objects}));
            }
        }
        // Each deployment is rendered; the status is the first's that
        // was not.
        if let Err(e) = result {
            super::matrix::say(cli, &e);
            if let Outcome::Done = outcome {
                outcome = Outcome::of_error(&e);
            }
        }
    }
    if r.json {
        println!("{}", serde_json::to_string_pretty(&docs)?);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object is its document with the kind the type says, never what
    /// the server writes; a kind the document writes otherwise is no
    /// object of its type.
    #[test]
    fn an_object_is_its_document_with_its_kind() {
        let k8s = Kubernetes::new(&[]).unwrap();
        let doc = json!({"metadata": {"name": "a", "managedFields": []}, "status": {}, "data": {}});
        assert_eq!(
            k8s.object("k8s.config_map", doc).unwrap(),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a"}, "data": {}})
        );
        assert_eq!(
            k8s.object("k8s.config_map", json!({"kind": "Secret"}))
                .unwrap_err(),
            "kind is \"Secret\"; the type says \"ConfigMap\""
        );
        assert!(k8s.object("k8s.acme.widget", json!({})).is_err());
    }

    /// A CRD defines each served version's type and its short name at the
    /// preferred version.
    #[test]
    fn a_crd_defines_its_kinds() {
        let doc = crate::tables::document(
            "yaml",
            "spec:\n  group: acme.example.com\n  names:\n    kind: Widget\n  versions:\n  \
             - name: v1\n  - name: v1beta1\n  - name: v0\n    served: false\n",
        )
        .unwrap();
        let kinds: Vec<(String, String, String)> = crd_kinds(&doc);
        let row = |t: &str, a: &str| (t.to_string(), a.to_string(), "Widget".to_string());
        assert_eq!(
            kinds,
            [
                row("k8s.acme.example.com.v1.widget", "acme.example.com/v1"),
                row(
                    "k8s.acme.example.com.v1beta1.widget",
                    "acme.example.com/v1beta1"
                ),
                row("k8s.acme.widget", "acme.example.com/v1"),
            ]
        );
    }
}
